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

use loupe_labels::Label;
use crate::starlark_types;
use crate::starlark_types::rule::FrozenRuleClass;

/// Evaluate a parsed `.bzl` AST and return the frozen module.
///
/// `defining_label` is the canonical Label of this `.bzl` itself —
/// used to stamp [`ProviderId`] on every `ProviderType` exported by
/// the frozen module so analysis can speak about provider identity
/// without holding Starlark heap pointers.
///
/// `repo_mapping` is the effective repo mapping of the
/// `.bzl`'s defining repo (`graph.effective_repo_mapping(label.package)`).
/// Stamped on every `RuleClass` so `attr.label(default = "@<apparent>...")`
/// resolves through the .bzl's perspective at rule-invoke time, not
/// the BUILD's.
pub fn evaluate(
    defining_label: &Label,
    repo_mapping: &std::collections::BTreeMap<String, loupe_labels::CanonicalRepo>,
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
        // Bazel-compat surface authored in user-space Starlark
        // (see `crate::builtins`). Injected as pre-bound names so user
        // `.bzl`s see `coverage_common`, `InstrumentedFilesInfo`, etc.
        // as if they were native globals — same shape as Bazel's
        // `builtins.zip` injection.
        crate::builtins::inject_builtins_into(&module)?;
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
        // Capture all names — including private ones — before freeze.
        // `Module::get` filters out underscore-prefixed names; we want
        // them so private rule bindings (`_my_rule = rule(...)` wrapped
        // by a public macro) get stamped too. Without this stamp, the
        // inner rule fires with `<unstamped_rule>` and
        // `compute_analysis` falls through to the source-file
        // early-return path (no impl runs).
        let all_names: Vec<String> = module
            .names_and_visibilities()
            .map(|(n, _)| n.as_str().to_string())
            .collect();
        let frozen_module = module
            .freeze()
            .map_err(|e| anyhow!("freezing .bzl module: {e:?}"))?;
        stamp_origin(&frozen_module, &all_names, defining_label, repo_mapping);
        Ok(frozen_module)
    })?;
    Ok(frozen)
}

fn stamp_origin(
    frozen: &FrozenModule,
    all_names: &[String],
    defining_label: &Label,
    repo_mapping: &std::collections::BTreeMap<String, loupe_labels::CanonicalRepo>,
) {
    stamp_origin_with_names(frozen, all_names, defining_label, repo_mapping)
}

/// Public alternative to [`stamp_rule_class_origin`] that also reaches
/// private (underscore-prefixed) bindings. Callers walk
/// `Module::names_and_visibilities()` pre-freeze (FrozenModule's
/// public iterator skips private names) and pass the list here. Used
/// by both the user `.bzl` evaluator and the embedded-builtins loader
/// so `_my_rule = rule(...)` wrapped by a public macro still gets
/// stamped with its binding name.
pub(crate) fn stamp_origin_with_names(
    frozen: &FrozenModule,
    all_names: &[String],
    defining_label: &Label,
    repo_mapping: &std::collections::BTreeMap<String, loupe_labels::CanonicalRepo>,
) {
    use crate::starlark_types::provider::{FrozenProviderType, ProviderId};
    for name in all_names {
        let Ok((owned, _vis)) = frozen.get_any_visibility(name) else {
            continue;
        };
        let frozen_value = unsafe { owned.unchecked_frozen_value() };
        if let Some(rc) = frozen_value.downcast_ref::<FrozenRuleClass>() {
            let _ = rc.kind.set(name.clone());
            let _ = rc.defining_bzl.set(defining_label.clone());
            let _ = rc.repo_mapping.set(repo_mapping.clone());
        }
        if let Some(pt) = frozen_value.downcast_ref::<FrozenProviderType>() {
            let _ = pt.id.set(ProviderId {
                defining_bzl: Some(defining_label.clone()),
                symbol: name.clone(),
            });
        }
    }
}

/// Parse a `.bzl` file at `path` into an AST.
pub fn parse(path: &std::path::Path) -> Result<AstModule> {
    parse_with_source(path).map(|(ast, _)| ast)
}

/// Parse a `.bzl` file at `path` into an AST plus its raw source bytes.
/// The source string is what callers digest into the marker file's
/// `predeclaredInputHash` so an edit to the rule's defining `.bzl`
/// busts every cached materialisation that used it.
pub fn parse_with_source(path: &std::path::Path) -> Result<(AstModule, String)> {
    let src =
        std::fs::read_to_string(path).map_err(|e| anyhow!("reading {}: {e}", path.display()))?;
    let filename = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let ast = AstModule::parse(&filename, src.clone(), &bazel_dialect())
        .map_err(|e| anyhow!("parsing {}: {e}", path.display()))?;
    Ok((ast, src))
}

/// Parse `.bzl` source text into an AST. `display_origin` is the
/// label-like string surfaced in parse-error messages (e.g.
/// `@bazel_tools//tools/cpp:toolchain_utils.bzl`).
pub fn parse_source(src: String, filename: &str, display_origin: &str) -> Result<AstModule> {
    AstModule::parse(filename, src, &bazel_dialect())
        .map_err(|e| anyhow!("parsing {display_origin}: {e}"))
}

/// Bazel's `.bzl` Starlark dialect: Standard plus keyword-only and
/// positional-only argument syntax (`def f(a, *, b)`/`def f(a, /, b)`).
/// Bazel's own toolchain helpers (`bazel_tools/tools/cpp/toolchain_utils.bzl`)
/// rely on `*` as a keyword-only marker, and pure `Standard` rejects
/// that.
fn bazel_dialect() -> Dialect {
    Dialect {
        enable_keyword_only_arguments: true,
        enable_positional_only_arguments: true,
        ..Dialect::Standard
    }
}

#[starlark::starlark_module]
pub(crate) fn bazel_json_namespace(globals: &mut starlark::environment::GlobalsBuilder) {
    fn encode(#[starlark(require = pos)] x: starlark::values::Value) -> anyhow::Result<String> {
        x.to_json()
    }

    fn decode<'v>(
        #[starlark(require = pos)] x: &str,
        heap: starlark::values::Heap<'v>,
    ) -> anyhow::Result<starlark::values::Value<'v>> {
        Ok(heap.alloc(serde_json::from_str::<serde_json::Value>(x)?))
    }

    /// `json.encode_indent(value, *, prefix="", indent="\t")` — Bazel's
    /// pretty-printer over the same value space `encode` accepts. Mirrors
    /// the [Bazel reference](https://bazel.build/rules/lib/json#encode_indent):
    /// `prefix` is prepended to each new line (after the first), `indent`
    /// is the per-level chunk.
    fn encode_indent<'v>(
        #[starlark(require = pos)] x: starlark::values::Value<'v>,
        #[starlark(require = named, default = "")] prefix: &str,
        #[starlark(require = named, default = "\t")] indent: &str,
    ) -> anyhow::Result<String> {
        // STUB: `prefix` and custom `indent` aren't honoured — serde_json's
        // `to_string_pretty` uses a fixed 2-space indent. The output
        // shape is otherwise identical to Bazel's, which is what
        // npm_package's generator-script consumers actually parse.
        let _ = (prefix, indent);
        let compact: String = x.to_json()?;
        let json: serde_json::Value = serde_json::from_str(&compact)?;
        Ok(serde_json::to_string_pretty(&json)?)
    }

    /// `json.indent(s, *, prefix="", indent="\t")` — starlark-go's
    /// name for `encode_indent`. Vendored upstream files reach for
    /// this. STUB: same fixed-indent behaviour as `encode_indent`.
    fn indent<'v>(
        #[starlark(require = pos)] s: &str,
        #[starlark(require = named, default = "")] prefix: &str,
        #[starlark(require = named, default = "\t")] indent: &str,
    ) -> anyhow::Result<String> {
        let _ = (prefix, indent);
        let json: serde_json::Value = serde_json::from_str(s)?;
        Ok(serde_json::to_string_pretty(&json)?)
    }
}

fn bzl_globals() -> Globals {
    // Bazel macros pervasively call `struct(...)`, `json.encode`,
    // `print`, etc. Enable the LibraryExtensions Loupe needs.
    //
    // `json` is *not* in the LibraryExtension set: starlark-rust ships
    // a 2-method (`encode`, `decode`) namespace; Bazel adds
    // `encode_indent`, which rules_js's npm_package generator relies
    // on. We register our own `json` namespace below.
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
        Typing,
        SetType,
    ])
    .with(starlark_types::register_for_bzl)
    .with(loupe_bzlmod::register_for_bzlmod)
    .with_namespace("json", bazel_json_namespace)
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
            loupe_labels::CanonicalRepo::MAIN,
            loupe_labels::PackagePath::root(),
            "test.bzl",
        );
        let frozen = evaluate(
            &label,
            &std::collections::BTreeMap::new(),
            ast,
            &HashMap::new(),
        )
        .unwrap();
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
            loupe_labels::CanonicalRepo::MAIN,
            loupe_labels::PackagePath::root(),
            "test.bzl",
        );
        let frozen = evaluate(
            &label,
            &std::collections::BTreeMap::new(),
            ast,
            &HashMap::new(),
        )
        .unwrap();
        let names: std::collections::BTreeSet<String> =
            frozen.names().map(|n| n.as_str().to_string()).collect();
        assert!(names.contains("my_aspect"), "names = {names:?}");
    }
}
