//! Compatibility no-op stubs for Bazel/Starlark APIs Loupe doesn't
//! model but rules and macros call defensively (or for visibility /
//! diagnostics that v1 doesn't enforce).
//!
//! Each entry here is a pragmatic STUB. Adding one is preferable to
//! teaching the loader to skip the call, because Starlark's evaluator
//! reports the error at the call site (where the user can see it),
//! while a loader-level skip would silently drop semantics.

use starlark::environment::GlobalsBuilder;
use starlark::starlark_module;
use starlark::values::Heap;
use starlark::values::Value;
use starlark::values::none::NoneType;

#[starlark_module]
pub fn compat_globals(b: &mut GlobalsBuilder) {
    /// `_loupe_elems_to_list(s)` — workaround helper for starlark-rust's
    /// `str.elems()` returning an iterator that doesn't support `[::]`
    /// slicing. Bazel's `str.elems()` returns a list; vendored .bzl
    /// code (rules_python's `parse_requirements_txt.bzl`) does
    /// `content.elems()[:]` and friends. We rewrite those call sites
    /// to `_loupe_elems_to_list(content)` during BzlLoad source-prep,
    /// and this function builds the real list of single-character
    /// strings the vendored code expects.
    fn _loupe_elems_to_list<'v>(
        #[starlark(require = pos)] s: &str,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let chars: Vec<Value<'v>> = s.chars().map(|c| heap.alloc(c.to_string())).collect();
        Ok(heap.alloc(chars))
    }

    /// `_cc_internal.actions2ctx_cheat(actions)` — rules_cc's
    /// `compile.bzl`/`link.bzl` recover the rule context from the
    /// `actions` object via this internal back-door. Loupe's `actions`
    /// is a stateless ZST (recording flows through `Evaluator::extra →
    /// AnalysisContext`), so we rebuild the slice of `ctx` those paths
    /// read — `label`, `configuration`, `fragments.cpp`, `bin_dir`,
    /// `genfiles_dir` — directly off the live `AnalysisContext`. Mirrors
    /// the same five fields `CtxGen::get_attr` surfaces.
    fn _loupe_actions2ctx_cheat<'v>(
        #[starlark(require = pos)] _actions: Value<'v>,
        eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        use starlark::values::structs::AllocStruct;
        let actx = eval
            .extra
            .ok_or_else(|| anyhow::anyhow!("actions2ctx_cheat called outside an AnalysisContext"))?
            .downcast_ref::<crate::analysis::AnalysisContext>()
            .ok_or_else(|| {
                anyhow::anyhow!("actions2ctx_cheat: Evaluator::extra is not an AnalysisContext")
            })?;
        let heap = eval.heap();
        let label = heap.alloc(loupe_labels::LabelValue::parse(
            &actx.target_label.to_string(),
        ));
        let configuration = heap.alloc(crate::starlark_types::configuration::CtxConfiguration {
            is_exec: actx.configuration.is_exec(),
        });
        // Only `cpp` is read on the cc compile/link paths; cc rules
        // always declare it. Add others here if a path surfaces them.
        let cpp = heap.alloc(crate::starlark_types::configuration::CtxFragment {
            name: "cpp".to_string(),
            compilation_mode: actx.configuration.compilation_mode(),
        });
        let fragments = heap.alloc(AllocStruct([("cpp", cpp)]));
        let bin_path = format!("bazel-out/{}/bin", actx.configuration.segment());
        let bin_dir = heap.alloc(AllocStruct([("path", heap.alloc(bin_path.as_str()))]));
        let actions = heap.alloc(crate::starlark_types::actions::Actions);
        // The link finalizer probes `ctx.exec_groups` / `ctx.toolchains`
        // for membership and reads `ctx.workspace_name`. Loupe collapses
        // exec onto the target platform (no exec groups) and the cheat-ctx
        // carries no resolved toolchain map here, so both are empty —
        // `actions.run(toolchain=None, exec_group=None)`, which Loupe
        // records inertly. `workspace_name` mirrors `CtxGen` (`"_main"`).
        let empty = heap.alloc(starlark::values::dict::AllocDict(Vec::<(
            Value<'v>,
            Value<'v>,
        )>::new()));
        Ok(heap.alloc(AllocStruct([
            ("label", label),
            ("configuration", configuration),
            ("fragments", fragments),
            ("bin_dir", bin_dir),
            ("genfiles_dir", bin_dir),
            ("actions", actions),
            ("workspace_name", heap.alloc("_main")),
            ("exec_groups", empty),
            ("toolchains", empty),
        ])))
    }

    /// `_loupe_make_args(list_of_strings)` — build an `Args` value whose
    /// pieces are exactly the given strings, in order. The cc shim's
    /// Starlark command-line expander (`exports.bzl`) computes the linker
    /// argv as a plain string list, then wraps it here so that
    /// `finalize_link_action.bzl`'s `actions.run(arguments=[link_args])`
    /// records it (an `Args` is spliced in place by `flatten_arguments`,
    /// whereas a bare list would stringify wrongly). The first element is
    /// the tool path, so the recorded `argv[0]` matches Bazel's.
    fn _loupe_make_args<'v>(
        #[starlark(require = pos)] items: starlark::values::list::UnpackList<&'v str>,
        eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let args = crate::starlark_types::actions::Args::new();
        {
            let mut pieces = args.pieces.lock().expect("Args mutex poisoned");
            for s in items.items {
                pieces.push(s.to_string());
            }
        }
        Ok(eval.heap().alloc(args))
    }

    /// `visibility(["//some:label", "public", ...])` — declares
    /// load-time visibility for the current `.bzl`. Loupe doesn't
    /// enforce visibility; accepted-and-ignored so vendored / user
    /// `.bzl` files top-of-module-call this without error. Registered
    /// here (not in `native_rules`) because `visibility(...)` is
    /// callable from `.bzl` as well as BUILD context.
    fn visibility<'v>(
        _args: &starlark::eval::Arguments<'v, '_>,
        _eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        Ok(starlark::values::none::NoneType)
    }

    /// `repository_rule(...)` — module-extension / WORKSPACE-time
    /// repo declarator. Loupe doesn't fetch external repos
    /// (workspaces are vendored), so the constructor is a no-op
    /// returning a marker the caller can pass around without us
    /// running the rule.
    fn repository_rule<'v>(
        _args: &starlark::eval::Arguments<'v, '_>,
        _eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        Ok(starlark::values::none::NoneType)
    }

    /// `module_extension(...)` — bzlmod extension declarator. Same
    /// no-op story as `repository_rule`: Loupe doesn't run
    /// extensions, just records `bazel_dep`s from `MODULE.bazel`.
    fn module_extension<'v>(
        _args: &starlark::eval::Arguments<'v, '_>,
        _eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        Ok(starlark::values::none::NoneType)
    }

    /// `tag_class(...)` — module-extension tag-class declarator.
    fn tag_class<'v>(
        _args: &starlark::eval::Arguments<'v, '_>,
        _eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        Ok(starlark::values::none::NoneType)
    }

    /// `subrule(implementation, fragments=[], attrs={}, …)` — Bazel 7+
    /// subrule constructor. Returns a callable [`Subrule`] that, when
    /// invoked inside a rule/subrule impl, runs `implementation` with a
    /// minimal subrule ctx (the declared fragments) and the declared
    /// attrs passed as `None`. See `starlark_types/subrule.rs`.
    fn subrule<'v>(
        implementation: Value<'v>,
        #[starlark(require = named, default = NoneType)] fragments: Value<'v>,
        #[starlark(require = named, default = NoneType)] attrs: Value<'v>,
        #[starlark(require = named, default = NoneType)] subrules: Value<'v>,
        #[starlark(require = named, default = NoneType)] toolchains: Value<'v>,
        eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (subrules, toolchains);
        let fragments_list: Vec<String> =
            match starlark::values::list::ListRef::from_value(fragments) {
                Some(list) => list
                    .iter()
                    .filter_map(|v| v.unpack_str().map(str::to_string))
                    .collect(),
                None => Vec::new(),
            };
        let attr_names: Vec<String> = match starlark::values::dict::DictRef::from_value(attrs) {
            Some(dict) => dict
                .iter()
                .filter_map(|(k, _)| k.unpack_str().map(str::to_string))
                .collect(),
            None => Vec::new(),
        };
        Ok(eval.heap().alloc(crate::starlark_types::subrule::Subrule {
            implementation,
            attr_names,
            fragments: fragments_list,
        }))
    }

    /// `transition(implementation, inputs, outputs)` — user
    /// transition declarator. Loupe records the value but doesn't
    /// fire transitions at v1; the cfg path captures `inputs` /
    /// `outputs` directly via `_TransitionInfo` reads instead.
    /// Returning `None` here is the "transitions disabled" sentinel.
    fn transition<'v>(
        _args: &starlark::eval::Arguments<'v, '_>,
        _eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        Ok(starlark::values::none::NoneType)
    }

    /// `analysis_test_transition(...)` — test-time transition
    /// declarator. v1 doesn't run analysis tests.
    fn analysis_test_transition<'v>(
        _args: &starlark::eval::Arguments<'v, '_>,
        _eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        Ok(starlark::values::none::NoneType)
    }

    /// `exec_group(...)` — multi-exec-platform exec group
    /// declarator. v1 collapses exec onto target platform.
    fn exec_group<'v>(
        _args: &starlark::eval::Arguments<'v, '_>,
        _eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        Ok(starlark::values::none::NoneType)
    }
}
