//! cquery script evaluator.
//!
//! Drives a user-provided Starlark script over the set of analyzed
//! targets in the workspace. The script must define
//! `def main(targets): -> Any`; Loupe calls it once with the full
//! list of `Target` values and stringifies the return.
//!
//! Surface choices are documented in
//! `~/.claude/plans/i-want-to-add-zesty-sundae.md`. In short: one
//! call over the whole list (not per-target), the live Starlark
//! values are reachable, and bazel cquery's `def format(target)`
//! convention is intentionally *not* mirrored.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::syntax::{AstModule, Dialect};

use crate::analysis::AnalyzedTarget;
use crate::engine::{FailedTarget, PackageError, SkippedTarget};
use crate::starlark_types::target::Target;

/// Outcome of a `cquery_script` run.
#[derive(Debug, Default)]
pub struct CqueryReport {
    /// `str(main(targets))` — the script's output.
    pub output: String,
    /// Targets that were successfully analyzed and made it into
    /// `targets`.
    pub analyzed: usize,
    /// Targets the caller asked Loupe to skip by rule kind.
    pub skipped: Vec<SkippedTarget>,
    /// Targets whose analysis failed; they did not appear in
    /// `targets`. Reuses the same shape `analyze_workspace` returns.
    pub failed: Vec<FailedTarget>,
    /// Packages that failed to load. No targets from these packages
    /// appear in `targets`.
    pub package_errors: Vec<PackageError>,
}

/// A parsed-and-frozen user script with its `main` entry point
/// resolved. The `FrozenModule` is held alongside the `FrozenValue`
/// so callers can `add_reference` it on the per-call heap.
#[derive(Debug)]
pub struct LoadedScript {
    main_fv: starlark::values::FrozenValue,
    owning_module: FrozenModule,
}

/// Parse and freeze the script at `path`, returning its `main`.
///
/// `main` must be a Starlark function. The script is evaluated against
/// a minimal post-analysis [`Globals`]: Starlark stdlib + the same
/// `json` namespace user `.bzl`s see. No `rule()` / `provider()` /
/// `ctx.*` / `load()` — this is a read-only consumer, not a loader.
pub fn load_script(path: &Path) -> Result<LoadedScript> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("reading cquery script `{}`: {e}", path.display()))?;
    let display = path.display().to_string();
    let ast = AstModule::parse(&display, src, &Dialect::Standard)
        .map_err(|e| anyhow!("parsing cquery script `{display}`: {e}"))?;
    let globals = cquery_globals();

    let frozen = Module::with_temp_heap(|module| -> Result<FrozenModule> {
        {
            let mut eval = Evaluator::new(&module);
            eval.eval_module(ast, &globals)
                .map_err(|e| anyhow!("evaluating cquery script `{display}`: {e}"))?;
        }
        module
            .freeze()
            .map_err(|e| anyhow!("freezing cquery script `{display}`: {e:?}"))
    })?;

    let owned = frozen
        .get_option("main")
        .map_err(|e| anyhow!("looking up `main` in cquery script `{display}`: {e:?}"))?
        .ok_or_else(|| anyhow!("cquery script `{display}` must define `def main(targets):`"))?;
    // SAFETY: `owned` came from `frozen`'s frozen heap, which we
    // return alongside the FrozenValue so callers can anchor it.
    let main_fv = unsafe { owned.unchecked_frozen_value() };
    Ok(LoadedScript {
        main_fv,
        owning_module: frozen,
    })
}

/// Call `script.main(targets)` and return `str(result)`.
///
/// Allocates one fresh `Module` for the call, anchors every analyzed
/// target's `analysis_heap` (so `FrozenValue`s reachable via
/// `target.actions[i].argv`, `target[Provider]`, etc. stay valid),
/// and builds the `targets` list in canonical-label order.
pub fn run_script(script: &LoadedScript, analyzed: &[Arc<AnalyzedTarget>]) -> Result<String> {
    Module::with_temp_heap(|module| -> Result<String> {
        module
            .frozen_heap()
            .add_reference(script.owning_module.frozen_heap());
        for at in analyzed {
            module
                .frozen_heap()
                .add_reference(at.analysis_heap.frozen_heap());
        }

        let target_values: Vec<starlark::values::Value<'_>> = analyzed
            .iter()
            .map(|at| module.heap().alloc(Target::new(Arc::clone(at))))
            .collect();
        let targets_list = module.heap().alloc(target_values);

        let mut eval = Evaluator::new(&module);
        let result = eval
            .eval_function(script.main_fv.to_value(), &[targets_list], &[])
            .map_err(|e| anyhow!("evaluating main(targets): {e}"))?;
        Ok(result.to_str())
    })
}

/// Evaluate a single Bazel-style `cquery --output=starlark --starlark:expr`
/// expression over one analyzed target.
///
/// Mirrors Bazel: the expression is evaluated with `target` bound to the
/// configured target, and the result is stringified (`str()` — a bare
/// string prints unquoted, a list/struct prints its `repr`-shaped form,
/// matching `--output=starlark`). The `target` value is the same
/// [`Target`] the script API exposes, so `target.label`, `target.files`,
/// `target.actions`, and `target[Provider]` all resolve off the analyzed
/// providers/actions — making loupe-vs-Bazel *output* fidelity directly
/// measurable.
pub fn eval_expr_over_target(at: &Arc<AnalyzedTarget>, expr: &str) -> Result<String> {
    // Wrap the expression so `target` is a parameter we can bind by
    // value (Starlark has no global rebinding mid-eval). The leading
    // `\n` keeps line numbers in any error message sensible.
    let src = format!("def loupe_cquery_expr(target):\n    return ({expr})\n");
    let ast = AstModule::parse("<cquery-expr>", src, &Dialect::Standard)
        .map_err(|e| anyhow!("parsing cquery expr `{expr}`: {e}"))?;
    let globals = cquery_globals();
    let frozen = Module::with_temp_heap(|module| -> Result<FrozenModule> {
        {
            let mut eval = Evaluator::new(&module);
            eval.eval_module(ast, &globals)
                .map_err(|e| anyhow!("evaluating cquery expr `{expr}`: {e}"))?;
        }
        module
            .freeze()
            .map_err(|e| anyhow!("freezing cquery expr `{expr}`: {e:?}"))
    })?;
    let owned = frozen
        .get_option("loupe_cquery_expr")
        .map_err(|e| anyhow!("looking up cquery expr fn: {e:?}"))?
        .ok_or_else(|| anyhow!("internal: cquery expr wrapper missing `loupe_cquery_expr`"))?;
    // SAFETY: `owned` came from `frozen`'s heap, anchored below.
    let expr_fv = unsafe { owned.unchecked_frozen_value() };

    Module::with_temp_heap(|module| -> Result<String> {
        module.frozen_heap().add_reference(frozen.frozen_heap());
        module
            .frozen_heap()
            .add_reference(at.analysis_heap.frozen_heap());
        let target_value = module.heap().alloc(Target::new(Arc::clone(at)));
        let mut eval = Evaluator::new(&module);
        let result = eval
            .eval_function(expr_fv.to_value(), &[target_value], &[])
            .map_err(|e| anyhow!("evaluating cquery expr `{expr}`: {e}"))?;
        Ok(result.to_str())
    })
}

/// `providers(target)` — Bazel cquery builtin returning the dict of a
/// target's providers, keyed by provider-id string. Loupe surfaces its
/// modeled providers; Bazel auto-attaches extra internal providers
/// (LicenseInfo, FileProvider, FilesToRunProvider, …) loupe doesn't
/// model, so the key set won't fully match — useful for surfacing
/// exactly which providers loupe carries vs Bazel.
#[starlark::starlark_module]
fn cquery_builtins(b: &mut GlobalsBuilder) {
    fn providers<'v>(
        #[starlark(require = pos)] target: starlark::values::Value<'v>,
        heap: starlark::values::Heap<'v>,
    ) -> anyhow::Result<starlark::values::Value<'v>> {
        use starlark::values::ValueLike;
        let t = target.downcast_ref::<Target>().ok_or_else(|| {
            anyhow!(
                "providers(target): expected a target, got {}",
                target.get_type()
            )
        })?;
        use starlark::collections::SmallMap;
        use starlark::values::dict::Dict;
        let mut sm: SmallMap<starlark::values::Value<'v>, starlark::values::Value<'v>> =
            SmallMap::new();
        for (key, inst) in t.enumerate_providers(heap) {
            let k = heap.alloc(key);
            let hashed = k
                .get_hashed()
                .map_err(|e| anyhow!("hashing provider key: {e}"))?;
            sm.insert_hashed(hashed, inst);
        }
        Ok(heap.alloc(Dict::new(sm)))
    }
}

fn cquery_globals() -> Globals {
    use starlark::environment::LibraryExtension::*;
    GlobalsBuilder::extended_by(&[StructType, Map, Filter, Print, Pprint, Pstr, Prepr, SetType])
        .with_namespace("json", crate::bzl_eval::bazel_json_namespace)
        .with(cquery_builtins)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn missing_main_errors() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "x = 1").unwrap();
        let err = load_script(f.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("must define `def main(targets):`"),
            "got: {err:#}"
        );
    }

    #[test]
    fn parse_failure_surfaces() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "def main(").unwrap();
        let err = load_script(f.path()).unwrap_err();
        assert!(format!("{err:#}").contains("parsing cquery script"));
    }

    #[test]
    fn empty_targets_works() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "def main(targets): return len(targets)").unwrap();
        let script = load_script(f.path()).unwrap();
        let out = run_script(&script, &[]).unwrap();
        assert_eq!(out, "0");
    }
}
