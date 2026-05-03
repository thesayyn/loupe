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

use crate::starlark_types;

/// Evaluate a parsed `.bzl` AST and return the frozen module.
pub fn evaluate(ast: AstModule, loaded: &HashMap<String, FrozenModule>) -> Result<FrozenModule> {
    let globals = bzl_globals();
    Module::with_temp_heap(|module| -> Result<FrozenModule> {
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
    })
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
    GlobalsBuilder::standard()
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
        let frozen = evaluate(ast, &HashMap::new()).unwrap();
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
        let frozen = evaluate(ast, &HashMap::new()).unwrap();
        let names: std::collections::BTreeSet<String> =
            frozen.names().map(|n| n.as_str().to_string()).collect();
        assert!(names.contains("my_aspect"), "names = {names:?}");
    }
}
