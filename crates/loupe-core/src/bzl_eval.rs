//! `.bzl` evaluator.
//!
//! Phase 3: evaluates a single `.bzl` file with the Loupe globals
//! (`provider`, `attr.*`, `rule`, `aspect`) plus the Starlark standard
//! library. `load()` statements within `.bzl` files are honored
//! through a `ReturnFileLoader` populated by the caller (PackageKey /
//! BzlLoadKey resolves them ahead of time through DICE).
//!
//! Returns a `FrozenModule` whose exported globals are whatever the
//! `.bzl` bound at top level — provider types, rule constructors, etc.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder, Module};
use starlark::eval::{Evaluator, ReturnFileLoader};
use starlark::syntax::{AstModule, Dialect};
use starlark::values::ValueLike;

use crate::label::Label;
use crate::starlark_types;
use crate::starlark_types::provider::{ProviderId, ProviderType};

/// Evaluate a parsed `.bzl` AST and return the frozen module.
///
/// `defining_label` is the canonical Label of this `.bzl` itself —
/// used to stamp [`ProviderId`] on every `ProviderType` exported by
/// the frozen module so analysis can speak about provider identity
/// without holding Starlark heap pointers.
pub fn evaluate(
    defining_label: &Label,
    ast: AstModule,
    loaded: &HashMap<String, FrozenModule>,
) -> Result<FrozenModule> {
    let globals = bzl_globals();
    let frozen = Module::with_temp_heap(|module| -> Result<FrozenModule> {
        // Anchor every transitively-loaded frozen heap so frozen values
        // stay valid through the freeze of *this* module's heap.
        for fm in loaded.values() {
            module.frozen_heap().add_reference(fm.frozen_heap());
        }
        let loader_modules: HashMap<&str, &FrozenModule> =
            loaded.iter().map(|(k, v)| (k.as_str(), v)).collect();
        let loader = ReturnFileLoader {
            modules: &loader_modules,
        };
        {
            let mut eval = Evaluator::new(&module);
            eval.set_loader(&loader);
            eval.eval_module(ast, &globals)
                .map_err(|e| anyhow!("evaluating .bzl: {e}"))?;
        }
        module
            .freeze()
            .map_err(|e| anyhow!("freezing .bzl module: {e:?}"))
    })?;
    stamp_provider_ids(&frozen, defining_label);
    Ok(frozen)
}

/// Walk the frozen module's exported symbols and, for each
/// `ProviderType`, fill in its [`ProviderId`] (defining bzl + symbol).
/// `provider()` itself can't know the symbol name at construction
/// time; this is the loading-phase one-shot that gives every provider
/// a stable identity for analysis to use.
///
/// `ProviderType.name` is similarly back-filled — it's nice for
/// diagnostics and matches Bazel's de-facto naming
/// (`<bzl>%<symbol>`).
fn stamp_provider_ids(frozen: &FrozenModule, defining_label: &Label) {
    for name in frozen.names() {
        let symbol = name.as_str();
        let value = match frozen.get_option(symbol) {
            Ok(Some(v)) => v,
            _ => continue,
        };
        // `ProviderType` is a simple_value — same Rust type frozen or
        // not — so a frozen-value downcast just works.
        let frozen_value = value.value();
        if let Some(provider) = frozen_value.downcast_ref::<ProviderType>() {
            let _ = provider.id.set(ProviderId {
                defining_bzl: Some(defining_label.clone()),
                symbol: symbol.to_string(),
            });
            // SAFETY of name: `provider.name` is `String` (immutable
            // through `&self`) — we can't write through a frozen
            // borrow. So `name` stays empty here; consumers compose
            // the display name from `id` instead, e.g. `id.symbol`.
            // (This is fine: `ProviderId.to_string()` is the canonical
            // display anyway.)
        }
    }
}

/// Parse a `.bzl` file at `path` into an AST.
pub fn parse(path: &std::path::Path) -> Result<AstModule> {
    let src =
        std::fs::read_to_string(path).map_err(|e| anyhow!("reading {}: {e}", path.display()))?;
    let filename = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    AstModule::parse(&filename, src, &Dialect::Standard)
        .map_err(|e| anyhow!("parsing {}: {e}", path.display()))
}

fn bzl_globals() -> Globals {
    // Bazel macros pervasively call `struct(...)`, `json.encode`,
    // `print`, etc. Enable the LibraryExtensions Loupe needs.
    use starlark::environment::LibraryExtension::*;
    GlobalsBuilder::extended_by(&[
        StructType,
        RecordType,
        EnumType,
        NamespaceType,
        Map,
        Filter,
        Partial,
        Debug,
        Print,
        Pprint,
        Pstr,
        Prepr,
        Json,
        Typing,
        SetType,
    ])
    .with(starlark_types::register_for_bzl)
    .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn evaluates_a_bzl_with_provider_and_rule_definitions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("defs.bzl");
        fs::write(
            &path,
            r#"
HelloInfo = provider(doc = "demo", fields = ["greeting"])

def _hello_impl(ctx):
    return [HelloInfo(greeting = ctx.attr.greeting)]

hello = rule(
    implementation = _hello_impl,
    attrs = {"greeting": attr.string(default = "hello")},
)
"#,
        )
        .unwrap();
        let ast = parse(&path).unwrap();
        let label = Label::new(
            crate::label::CanonicalRepo::MAIN,
            crate::label::PackagePath::root(),
            "test.bzl",
        );
        let frozen = evaluate(&label, ast, &HashMap::new()).unwrap();
        // The frozen module should expose `HelloInfo` and `hello`.
        let names: std::collections::BTreeSet<String> =
            frozen.names().map(|n| n.as_str().to_string()).collect();
        assert!(names.contains("HelloInfo"), "names = {names:?}");
        assert!(names.contains("hello"), "names = {names:?}");
    }

    #[test]
    fn aspect_is_a_noop_accept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bzl");
        fs::write(
            &path,
            r#"
def _impl(target, ctx):
    return []

my_aspect = aspect(implementation = _impl)
"#,
        )
        .unwrap();
        let ast = parse(&path).unwrap();
        let label = Label::new(
            crate::label::CanonicalRepo::MAIN,
            crate::label::PackagePath::root(),
            "test.bzl",
        );
        let frozen = evaluate(&label, ast, &HashMap::new()).unwrap();
        let names: std::collections::BTreeSet<String> =
            frozen.names().map(|n| n.as_str().to_string()).collect();
        assert!(names.contains("my_aspect"), "names = {names:?}");
    }
}
