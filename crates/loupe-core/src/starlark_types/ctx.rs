//! Analysis-phase `ctx` value — fully native.
//!
//! The rule context is native: `analysis::run_analysis` dispatches the
//! impl through `invoke_rule(impl, ctx)` (now a pass-through), and the
//! impl receives this `CtxGen` directly — there is no `_wrap_rule_ctx`
//! Starlark overlay. Every field the wrapper used to synthesise is
//! native here or on the native `Actions` / `Runfiles` /
//! `configuration` types. Adding ctx surface is a Rust change.
//!
//! Fields and their sources:
//!
//! - `ctx.attr` — struct of resolved attribute values.
//! - `ctx.label` — configured target's label, as a `Label` value.
//! - `ctx.workspace_name` — main module name from `MODULE.bazel`.
//! - `ctx.actions` — recording surface, see
//!   [`actions`](super::actions).
//! - `ctx.files` / `ctx.file` / `ctx.executable` — flattened dep
//!   default files / single dep file / executable handle.
//! - `ctx.outputs` — implicit-output handle struct.
//! - `ctx.toolchains` — resolved toolchain map.
//! - `ctx.bin_dir` / `ctx.genfiles_dir` — `bazel-out/<config>/bin`
//!   root struct (`genfiles_dir` aliases `bin_dir`).
//! - `ctx.runfiles(...)` — factory returning a
//!   [`Runfiles`](super::runfiles::Runfiles).
//! - `ctx.configuration` / `ctx.fragments.<name>` — native views built
//!   off the analyzed target's `Configuration`, see
//!   [`configuration`](super::configuration). `fragments` is bound only
//!   when the rule declared `fragments=`.
//! - `ctx.exec_groups` — STUB; returns an empty dict (v1 doesn't model
//!   exec groups).

use allocative::Allocative;
use anyhow::anyhow;
use starlark::any::ProvidesStaticType;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::starlark_complex_values;
use starlark::starlark_module;
use starlark::values::dict::UnpackDictEntries;
use starlark::values::list::UnpackList;
use starlark::values::{
    Freeze, FreezeResult, Freezer, FrozenValue, Heap, NoSerialize, StarlarkValue, Trace, Value,
    ValueLifetimeless, ValueLike, starlark_value,
};

use super::actions::Actions;
use super::file::File;
use super::runfiles::Runfiles;

#[derive(Debug, Trace, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[repr(C)]
#[display("<ctx for {label_str}>")]
pub struct CtxGen<V: ValueLifetimeless> {
    /// Configured target label as a string. Phase 5 stop-gate scope:
    /// keep it a string so the impl can stringify/compare; a future
    /// phase swaps in a real `Label` Starlark value.
    #[trace(static)]
    pub label_str: String,
    /// Main module name from `MODULE.bazel`. Bazel's
    /// `ctx.workspace_name`.
    #[trace(static)]
    pub workspace_name: String,
    /// `<config>` segment used in `bazel-out/<config>/bin/...` paths.
    #[trace(static)]
    pub config_segment: String,
    /// `--compilation_mode` for this configuration (`fastbuild`/`opt`/
    /// `dbg`). Feeds the `COMPILATION_MODE` make variable. Sourced from
    /// the real `Configuration`, not a hardcoded `"fastbuild"` — the
    /// rule context owns config-derived values natively.
    #[trace(static)]
    pub compilation_mode: String,
    /// `--cpu` for this configuration. Feeds the `TARGET_CPU` make
    /// variable. Sourced from `Configuration`, not a hardcoded `"k8"`.
    #[trace(static)]
    pub cpu: String,
    /// Whether this target is analyzed in the exec (tool) configuration.
    /// Drives the native `ctx.configuration.is_tool_configuration()`.
    #[trace(static)]
    pub is_exec: bool,
    /// Configuration-fragment names the rule declared via
    /// `rule(fragments=[…])`. Drives the native `ctx.fragments` view
    /// (`starlark_types/configuration.rs`): each name becomes a
    /// `ctx.fragments.<name>` built off the live `Configuration`, with
    /// no `@bazel_tools//fragments` target analysed. Empty ⇒
    /// `ctx.fragments` is unbound (`hasattr` is `False`).
    #[trace(static)]
    pub fragment_names: Vec<String>,
    /// Struct whose fields are the resolved attrs: `ctx.attr.<name>`
    /// returns `attr.get_attr(name)`. Built in the analysis heap via
    /// `AllocStruct`.
    pub attr: V,
    /// Struct keyed by label-typed attr name → `[File]`. Each entry
    /// is the flattened `DefaultInfo.files` of every dep in that
    /// attr. `ctx.files.<srcs>` reads it.
    pub files: V,
    /// Struct keyed by `attr.label` attr name → `File` (the dep's
    /// first default file). `ctx.file.<src>` reads it. Empty for
    /// `attr.label_list` attrs.
    pub file: V,
    /// Struct keyed by `attr.label(executable=True)` attr name →
    /// the dep's `DefaultInfo.executable`. `ctx.executable.<tool>`
    /// reads it.
    pub executable: V,
    /// Struct keyed by every `attr.output(...)` / `attr.output_list(...)`
    /// attr name on the rule. For `Output`: a `File` for the
    /// supplied label or `None` when omitted. For `OutputList`: a
    /// `list[File]` (possibly empty). `ctx.outputs.<name>` reads it.
    /// Built once per analysis from `materialised_attrs` + the rule's
    /// attrs schema.
    pub outputs: V,
    /// Resolved toolchain map. Populated in `compute_analysis` by
    /// walking `ModuleGraph::registered_toolchains` for every
    /// toolchain-type the rule declared via `rule(toolchains=[...])`,
    /// force-analyzing the matching toolchain impl, and storing the
    /// result keyed by canonical type label. Lookups via `[T]` /
    /// `T in ctx.toolchains` go through this map; a miss raises
    /// (real-or-fail), matching Bazel — see [`super::toolchains`].
    pub toolchains: V,
    /// `ctx.build_setting_value` — Bazel's read-side of a
    /// `rule(build_setting=…)`-typed rule. Set to the configured
    /// value when this target's rule declares a build_setting,
    /// `None` otherwise. The configured value is the
    /// `build_setting_default` attr unless an override has been
    /// applied via CLI / transition. Mirrors
    /// `pkg/model/analysis/configured_target.go::"build_setting_value"`
    /// case — the impl side reads through this primitive so future
    /// override modeling can land in one place. STUB: Loupe doesn't
    /// model overrides yet, so the value always equals
    /// `build_setting_default`.
    pub build_setting_value: V,
    /// `ctx.rule` — bound only on an *aspect* ctx. A struct
    /// `{attr: <base target's attr struct>, kind: <base rule kind>}`.
    /// `Value::new_none()` (unbound) on a rule ctx, matching Bazel
    /// (`hasattr(ctx, "rule")` is `False` inside a rule impl).
    pub rule: V,
}

pub type Ctx<'v> = CtxGen<Value<'v>>;
pub type FrozenCtx = CtxGen<FrozenValue>;

starlark_complex_values!(Ctx);

impl<'v> Freeze for Ctx<'v> {
    type Frozen = FrozenCtx;
    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(CtxGen {
            label_str: self.label_str,
            workspace_name: self.workspace_name,
            config_segment: self.config_segment,
            compilation_mode: self.compilation_mode,
            cpu: self.cpu,
            is_exec: self.is_exec,
            fragment_names: self.fragment_names,
            attr: self.attr.freeze(freezer)?,
            files: self.files.freeze(freezer)?,
            file: self.file.freeze(freezer)?,
            executable: self.executable.freeze(freezer)?,
            outputs: self.outputs.freeze(freezer)?,
            toolchains: self.toolchains.freeze(freezer)?,
            build_setting_value: self.build_setting_value.freeze(freezer)?,
            rule: self.rule.freeze(freezer)?,
        })
    }
}

#[starlark_value(type = "ctx")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for CtxGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "attr" => Some(self.attr.to_value()),
            // `ctx.label` — a real `Label` value with `.name`,
            // `.package`, `.workspace_name`. Bazel rules read these
            // pervasively (e.g. `ctx.label.name` for output naming).
            "label" => Some(heap.alloc(loupe_labels::LabelValue::parse(&self.label_str))),
            // Bazel's `ctx.workspace_name` is the repo's runfiles
            // prefix — `"_main"` for the main repo under bzlmod (the
            // `_wrap_rule_ctx` wrapper hardcoded this; native now).
            // STUB: external-repo targets get their canonical prefix in
            // Bazel; v1 is main-repo-focused and returns `"_main"`. The
            // `self.workspace_name` field (module name) is retained for
            // when external-repo prefixes are modelled.
            "workspace_name" => Some(heap.alloc("_main")),
            "actions" => Some(heap.alloc(Actions)),
            // `ctx.files.<attr>` — flattened `DefaultInfo.files` of
            // every dep in that label-list / label attr. Built in
            // `run_analysis` from the pre-resolved `ResolvedDep`s.
            "files" => Some(self.files.to_value()),
            // `ctx.file.<attr>` — single `File` for the dep in a
            // `attr.label` attr (the dep's first default file).
            // Empty for `attr.label_list` attrs.
            "file" => Some(self.file.to_value()),
            // `ctx.executable.<attr>` — `File` of the dep's
            // `DefaultInfo.executable` for `attr.label(executable=True)`.
            // Empty otherwise.
            "executable" => Some(self.executable.to_value()),
            // `ctx.toolchains[<type>]` — real lookup against the
            // resolver's map. On hit, returns a struct of the impl's
            // `ToolchainInfo` provider fields. On miss, the lookup
            // raises (real-or-fail), matching Bazel.
            "toolchains" => Some(self.toolchains.to_value()),
            // `ctx.build_setting_value` — current configured value of
            // this target's build_setting (default unless overridden).
            // `None` for non-build_setting rules.
            "build_setting_value" => Some(self.build_setting_value.to_value()),
            // `ctx.rule` — only on an aspect ctx; a `{attr, kind}`
            // struct describing the base target the aspect runs over.
            // Unbound (None) on a rule ctx so `hasattr(ctx, "rule")` is
            // `False` there.
            "rule" => {
                let v = self.rule.to_value();
                if v.is_none() { None } else { Some(v) }
            }
            // `build_file_path`, `genfiles_dir`, `disabled_features`,
            // `features`, `configuration`, `fragments` are native
            // `get_attr` arms below; `var`, `coverage_instrumented`,
            // `expand_location`, `expand_make_variables`,
            // `resolve_command`, `target_platform_has_constraint` are
            // native methods in `ctx_methods`. None go through a
            // Starlark wrapper — the impl sees this `CtxGen` directly.
            // `ctx.outputs.<name>` — implicit-output handle. Built in
            // `analysis::build_outputs_struct` from the rule's
            // `attr.output(...)` / `attr.output_list(...)` attrs and
            // their materialised values. `Output` attrs surface as a
            // `File` (or `None` when the BUILD side didn't supply
            // one); `OutputList` attrs surface as a `list[File]`.
            "outputs" => Some(self.outputs.to_value()),
            // `ctx.bin_dir` — Bazel returns a `root` object with a
            // `.path` attr (the `bazel-out/<config>/bin` prefix). v1
            // surfaces a small struct with `path` so both `ctx.bin_dir`
            // (string-concat usage) and `ctx.bin_dir.path` (rules_js's
            // `expand_variables.bzl`) resolve. The struct also stringifies
            // to the path so existing `ctx.bin_dir + "/x"` consumers
            // keep working.
            // `ctx.bin_dir` / `ctx.genfiles_dir` — both surface the
            // `bazel-out/<config>/bin` root as a `{path}` struct that
            // also stringifies to the path. `genfiles_dir` aliases
            // `bin_dir` (Bazel merged the legacy genfiles tree into
            // bin/); it's native now — the rule context is migrating
            // back to Tier-1 so its derived fields stop being Starlark
            // literals on the wrapper struct.
            "bin_dir" | "genfiles_dir" => {
                let path = format!("bazel-out/{}/bin", self.config_segment);
                let pairs: starlark::collections::SmallMap<&str, starlark::values::Value<'v>> =
                    [("path", heap.alloc(path.as_str()))].into_iter().collect();
                Some(heap.alloc(starlark::values::structs::AllocStruct(pairs)))
            }
            // `ctx.build_file_path` — Bazel's `<package>/BUILD`. Pure
            // derivation off the (native) label; moved off the wrapper.
            "build_file_path" => {
                let pkg = loupe_labels::LabelValue::parse(&self.label_str).package;
                Some(heap.alloc(format!("{pkg}/BUILD")))
            }
            // Config-derived make-variable inputs, native off the real
            // `Configuration` (not hardcoded on the wrapper).
            "compilation_mode" => Some(heap.alloc(self.compilation_mode.as_str())),
            "cpu" => Some(heap.alloc(self.cpu.as_str())),
            // `ctx.features` / `ctx.disabled_features` — v1 models no
            // per-target feature flags; empty lists (native, was
            // wrapper-derived).
            "features" | "disabled_features" => {
                Some(heap.alloc(Vec::<starlark::values::Value<'v>>::new()))
            }
            // `ctx.configuration` / `ctx.fragments` — built natively in
            // Rust off the analyzed target's `Configuration` (see
            // `starlark_types/configuration.rs`). There is no
            // `@bazel_tools//fragments` target to analyse and no
            // `FragmentInfo` provider: the engine owns this surface
            // because the values derive from engine-side config state.
            //
            // `configuration` always resolves (Bazel makes it available
            // to every rule). `fragments` is bound only when the rule
            // declared `fragments=` — empty ⇒ `None` here so
            // `hasattr(ctx, "fragments")` is `False`, matching Bazel.
            //
            // These MUST be `get_attr` arms, not `#[starlark(attribute)]`
            // in `ctx_methods`: a method-builder attribute would shadow
            // `StarlarkValue::get_attr` and win unconditionally.
            "configuration" => Some(heap.alloc(
                crate::starlark_types::configuration::CtxConfiguration {
                    is_exec: self.is_exec,
                },
            )),
            "fragments" => {
                if self.fragment_names.is_empty() {
                    return None;
                }
                let pairs: Vec<(String, Value<'v>)> = self
                    .fragment_names
                    .iter()
                    .map(|name| {
                        (
                            name.clone(),
                            heap.alloc(crate::starlark_types::configuration::CtxFragment {
                                name: name.clone(),
                                compilation_mode: self.compilation_mode.clone(),
                            }),
                        )
                    })
                    .collect();
                Some(heap.alloc(starlark::values::structs::AllocStruct(pairs)))
            }
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        // Native ctx surface. `configuration` and `fragments` are built
        // natively off the `Configuration` (see
        // `starlark_types/configuration.rs`); `fragments` is listed only
        // when the rule declared `fragments=`, matching the `get_attr`
        // arm that leaves it unbound otherwise.
        let mut names: Vec<String> = [
            "attr",
            "label",
            "workspace_name",
            "actions",
            "files",
            "file",
            "executable",
            "toolchains",
            "outputs",
            "bin_dir",
            "genfiles_dir",
            "build_file_path",
            "features",
            "disabled_features",
            "build_setting_value",
            "target_platform_has_constraint",
            "configuration",
            "version_file",
            "info_file",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        if !self.fragment_names.is_empty() {
            names.push("fragments".to_string());
        }
        names
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(ctx_methods)
    }
}

#[starlark_module]
#[allow(clippy::too_many_arguments)] // Bazel surface; matches upstream
fn ctx_methods(builder: &mut MethodsBuilder) {
    // Loupe used to ship lying stubs for `target_platform_has_constraint`
    // (always False), `expand_make_variables` (passthrough),
    // `configuration` (empty `default_shell_env`), and
    // `version_file` / `info_file` (fake paths). They all silently
    // produced wrong analysis results — e.g. a rule that selects a
    // command line based on `target_platform_has_constraint(windows)`
    // would always pick the Unix branch, regardless of the configured
    // platform. Real Bazel populates these from host machinery; if
    // Loupe doesn't model the underlying state, the access should
    // raise so the gap is visible. See task #25.

    /// `ctx.exec_groups` — dict keyed by exec-group name. v1 doesn't
    /// model exec groups; an empty dict matches what the vendored
    /// `_wrap_rule_ctx` in `builtins_core/wrappers.bzl` checks for
    /// (`"" in ctx.exec_groups`). Marked as an attribute (not a
    /// method) so the access reads as a value, not a bound method.
    #[starlark(attribute)]
    fn exec_groups<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let _ = this;
        Ok(heap.alloc(starlark::collections::SmallMap::<Value<'v>, Value<'v>>::new()))
    }

    /// `ctx.version_file` — File at `bazel-out/volatile-status.txt`.
    /// Real Bazel emits this from `--workspace_status_command`'s
    /// "volatile" output. Loupe doesn't run the command, so the File
    /// is a path-only handle suitable for rules reading
    /// `ctx.version_file.path` (e.g. rules_js's `expand_variables`).
    #[starlark(attribute)]
    fn version_file<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let _ = this;
        Ok(heap.alloc(File::build_status(
            loupe_labels::CanonicalRepo::MAIN,
            "volatile-status.txt",
        )))
    }

    /// `ctx.info_file` — File at `bazel-out/stable-status.txt`. Sibling
    /// of `version_file`; same shape, different basename.
    #[starlark(attribute)]
    fn info_file<'v>(this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let _ = this;
        Ok(heap.alloc(File::build_status(
            loupe_labels::CanonicalRepo::MAIN,
            "stable-status.txt",
        )))
    }

    /// `ctx.target_platform_has_constraint(constraint_value)` — true
    /// iff the configured target platform satisfies
    /// `constraint_value`. `constraint_value` is the Target/struct
    /// value from `ConstraintValueInfo` (carrying a `label` field).
    ///
    /// Raises when no target platform is configured — matches Bazel:
    /// the call is undefined without a `--platforms`.
    fn target_platform_has_constraint<'v>(
        #[starlark(this)] _this: Value<'v>,
        constraint_value: Value<'v>,
        eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<bool> {
        use crate::analysis::AnalysisContext;
        let actx = eval
            .extra
            .ok_or_else(|| anyhow!("ctx.target_platform_has_constraint called outside analysis"))?
            .downcast_ref::<AnalysisContext>()
            .ok_or_else(|| anyhow!("internal: Evaluator::extra is not an AnalysisContext"))?;
        let constraints = actx.target_platform_constraints.as_ref().ok_or_else(|| {
            anyhow!(
                "ctx.target_platform_has_constraint requires a configured target platform \
                 (`--platforms`); none was set on the engine"
            )
        })?;
        // `constraint_value` may be a `ConstraintValueInfo` provider
        // instance (live or frozen) or the struct synthesised by
        // Loupe for native `constraint_value` targets. Either way it
        // exposes a `label` field whose value is the CV's canonical
        // label string.
        let label_v = constraint_value
            .get_attr("label", eval.heap())
            .map_err(|e| anyhow!("reading .label on constraint_value arg: {e}"))?
            .ok_or_else(|| anyhow!("constraint_value argument lacks a `label` field"))?;
        let label_str = label_v
            .unpack_str()
            .map(|s| s.to_string())
            .unwrap_or_else(|| label_v.to_string());
        // The constraint set is canonical labels; parse and compare.
        let parsed = loupe_labels::Label::resolve(
            &label_str,
            &actx.package,
            &loupe_labels::RepoMapping::empty(),
        )
        .map_err(|e| anyhow!("parsing constraint_value label {label_str:?}: {e}"))?;
        Ok(constraints.contains(&parsed))
    }

    /// `ctx.runfiles(files=[], transitive_files=None, symlinks={},
    /// root_symlinks={})`.
    ///
    /// `transitive_files` accepts either a flat `list[File]` (v1
    /// shortcut, kept for backward compatibility) or a `depset(File)`
    /// (Bazel's true shape — what rules_oci passes). Either way the
    /// File set is flattened on construction; the depset's tree
    /// structure isn't preserved on the `Runfiles` value (STUB).
    fn runfiles<'v>(
        #[starlark(this)] _this: Value<'v>,
        files: Option<UnpackList<&'v File>>,
        #[starlark(require = named)] transitive_files: Option<Value<'v>>,
        #[starlark(require = named)] symlinks: Option<UnpackDictEntries<String, &'v File>>,
        #[starlark(require = named)] root_symlinks: Option<UnpackDictEntries<String, &'v File>>,
        // STUB: Bazel's `collect_default=True` adds `data` deps' default
        // runfiles into the result. v1 doesn't model the rule's `data`
        // attr at this site (callers using merge() roll it themselves);
        // accept-and-ignore so `ctx.runfiles(transitive_files=…,
        // collect_default=True)` (rules_shell sh_library) doesn't trip.
        #[starlark(require = named)] collect_default: Option<bool>,
        // STUB: `collect_data` is the legacy alias of `collect_default`
        // for old rules. Accept-and-ignore.
        #[starlark(require = named)] collect_data: Option<bool>,
        // STUB: `skip_conflict_checking` disables Bazel's runfiles
        // symlink-conflict diagnostic; v1 doesn't conflict-check.
        // Accept-and-ignore (rules_js passes it).
        #[starlark(require = named)] skip_conflict_checking: Option<bool>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Runfiles> {
        // Returns the native Rust `Runfiles` — the canonical runfiles
        // provider. `DefaultInfo.default_runfiles` and the global
        // `runfiles(...)` constructor produce the same type, so
        // `ctx.runfiles(...).merge_all([dep[DefaultInfo].default_runfiles])`
        // is all one type.
        let _ = (collect_default, collect_data, skip_conflict_checking);
        let mut all: Vec<File> = files
            .map(|l| l.items.into_iter().cloned().collect())
            .unwrap_or_default();
        if let Some(t) = transitive_files {
            collect_files_from(t, heap, &mut all)?;
        }
        let symlinks_map = symlinks
            .map(|d| d.entries.into_iter().map(|(k, v)| (k, v.clone())).collect())
            .unwrap_or_default();
        let root_symlinks_map = root_symlinks
            .map(|d| d.entries.into_iter().map(|(k, v)| (k, v.clone())).collect())
            .unwrap_or_default();
        Ok(Runfiles {
            files: all,
            symlinks: symlinks_map,
            root_symlinks: root_symlinks_map,
        })
    }

    /// `ctx.coverage_instrumented(target=None)` — whether coverage
    /// instrumentation is enabled for `target` (or this rule). v1
    /// doesn't model `--collect_code_coverage`, so always `False`
    /// (matches Bazel with coverage off). Native now (was a closure on
    /// the `_wrap_rule_ctx` struct).
    fn coverage_instrumented<'v>(
        #[starlark(this)] _this: Value<'v>,
        target: Option<Value<'v>>,
    ) -> anyhow::Result<bool> {
        let _ = target;
        Ok(false)
    }

    /// `ctx.resolve_command(command="", make_variables={…}, …)` —
    /// returns `(input_manifests, argv, manifests)`. Loupe lowers it to
    /// `/bin/bash -c <make-expanded command>` with empty manifests.
    /// Native port of the `_wrap_rule_ctx` closure: a pure string
    /// algorithm, no engine state. The extra Bazel kwargs
    /// (`attribute`, `expand_locations`, `tools`, `label_dict`,
    /// `execution_requirements`) are accepted and ignored, matching the
    /// previous wrapper behaviour.
    #[allow(clippy::too_many_arguments)]
    fn resolve_command<'v>(
        #[starlark(this)] _this: Value<'v>,
        #[starlark(default = "")] command: &str,
        #[starlark(require = named)] attribute: Option<Value<'v>>,
        #[starlark(require = named)] expand_locations: Option<Value<'v>>,
        #[starlark(require = named)] make_variables: Option<UnpackDictEntries<String, String>>,
        #[starlark(require = named)] tools: Option<Value<'v>>,
        #[starlark(require = named)] label_dict: Option<Value<'v>>,
        #[starlark(require = named)] execution_requirements: Option<Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (
            attribute,
            expand_locations,
            tools,
            label_dict,
            execution_requirements,
        );
        let vars: std::collections::HashMap<String, String> = make_variables
            .map(|d| d.entries.into_iter().collect())
            .unwrap_or_default();
        let expanded = expand_make_variables(command, |name| {
            vars.get(name)
                .cloned()
                .ok_or_else(|| anyhow!("make variable $({name}) not found in make_variables"))
        })?;
        let empty_in = heap.alloc(Vec::<Value<'v>>::new());
        let argv = heap.alloc(vec![
            heap.alloc("/bin/bash"),
            heap.alloc("-c"),
            heap.alloc(expanded.as_str()),
        ]);
        let empty_manifests = heap.alloc(Vec::<Value<'v>>::new());
        Ok(heap.alloc((empty_in, argv, empty_manifests)))
    }

    /// `ctx.var` — the make-variable dict (built-ins + toolchain
    /// `TemplateVariableInfo`). Native now (was built on the
    /// `_wrap_rule_ctx` struct).
    #[starlark(attribute)]
    fn var<'v>(#[starlark(this)] this: Value<'v>, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        // A real dict (not a struct): rules read `ctx.var["X"]` and
        // `ctx.var.keys()`.
        let map = build_make_var_map(this, heap)?;
        let entries: Vec<(Value<'v>, Value<'v>)> = map
            .iter()
            .map(|(k, v)| (heap.alloc(k.as_str()), heap.alloc(v.as_str())))
            .collect();
        Ok(heap.alloc(starlark::values::dict::AllocDict(entries)))
    }

    /// `ctx.expand_make_variables(attribute_name, command,
    /// additional_substitutions)` — expand `$(VAR)`/`$VAR` over `ctx.var`
    /// plus `additional_substitutions` (which take precedence). Native
    /// port of the `_wrap_rule_ctx` closure. Raises on an unknown
    /// variable, matching the wrapper's dict access.
    fn expand_make_variables<'v>(
        #[starlark(this)] this: Value<'v>,
        #[starlark(require = pos)] attribute_name: Value<'v>,
        #[starlark(require = pos)] command: &str,
        #[starlark(require = pos)] additional_substitutions: UnpackDictEntries<String, String>,
        eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<String> {
        let _ = attribute_name;
        let extra: std::collections::HashMap<String, String> =
            additional_substitutions.entries.into_iter().collect();
        let var = build_make_var_map(this, eval.heap())?;
        expand_make_variables(command, |name| {
            if let Some(v) = extra.get(name) {
                return Ok(v.clone());
            }
            var.get(name)
                .cloned()
                .ok_or_else(|| anyhow!("make variable $({name}) is not defined"))
        })
    }

    /// `ctx.expand_location(input, targets=[])` — expand
    /// `$(location|execpath|rootpath[s] LABEL)` directives against the
    /// rule's `targets` deps and its declared outputs. Native port of
    /// the `_wrap_rule_ctx` closure. `execpath`/`location` resolve to
    /// the exec-relative `File.path`; `rootpath` to the
    /// runfiles-relative `File.short_path`. Plural `…s` forms space-join
    /// every file a label provides; singular forms require exactly one.
    /// Unknown directives pass through unexpanded (matches the wrapper).
    fn expand_location<'v>(
        #[starlark(this)] this: Value<'v>,
        input: &str,
        targets: Option<UnpackList<Value<'v>>>,
        eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<String> {
        use crate::analysis::AnalysisContext;
        use std::collections::HashMap;
        let heap = eval.heap();
        let actx = eval
            .extra
            .ok_or_else(|| anyhow!("ctx.expand_location called outside analysis"))?
            .downcast_ref::<AnalysisContext>()
            .ok_or_else(|| anyhow!("internal: Evaluator::extra is not an AnalysisContext"))?;
        let mapping = loupe_labels::RepoMapping::empty();
        let pkg = actx.package.clone();
        // Canonicalise any label-bearing value to a `Label` so target,
        // output, and directive keys compare through one resolver.
        let to_label = |v: Value<'v>| -> anyhow::Result<loupe_labels::Label> {
            let raw = match v.downcast_ref::<loupe_labels::LabelValue>() {
                Some(lv) => lv.raw.clone(),
                None => v.to_string(),
            };
            loupe_labels::Label::resolve(&raw, &pkg, &mapping)
                .map_err(|e| anyhow!("expand_location: parsing label {raw:?}: {e}"))
        };

        let mut all: HashMap<loupe_labels::Label, Vec<File>> = HashMap::new();

        // targets: keyed by `original_label`. `[executable]` when the
        // dep is executable and doesn't provide exactly one file, else
        // its full file list (mirrors the wrapper).
        for t in targets.map(|l| l.items).unwrap_or_default() {
            let orig = t
                .get_attr("original_label", heap)
                .map_err(|e| anyhow!("expand_location: reading .original_label: {e}"))?
                .ok_or_else(|| anyhow!("expand_location: target lacks original_label"))?;
            let key = to_label(orig)?;
            let mut files: Vec<File> = Vec::new();
            if let Some(fv) = t.get_attr("files", heap).ok().flatten() {
                collect_files_from(fv, heap, &mut files)?;
            }
            let exec: Option<File> = t
                .get_attr("files_to_run", heap)
                .ok()
                .flatten()
                .and_then(|ftr| ftr.get_attr("executable", heap).ok().flatten())
                .filter(|e| !e.is_none())
                .and_then(|e| File::from_value(e).cloned());
            let entry = match exec {
                Some(e) if files.len() != 1 => vec![e],
                _ => files,
            };
            all.insert(key, entry);
        }

        // outputs: each declared output `File` keyed by its own label.
        if let Some(outputs) = this.get_attr("outputs", heap).ok().flatten()
            && let Some(s) = starlark::values::structs::StructRef::from_value(outputs)
        {
            let insert_file = |fv: Value<'v>,
                               all: &mut HashMap<loupe_labels::Label, Vec<File>>|
             -> anyhow::Result<()> {
                if let Some(f) = File::from_value(fv)
                    && let Some(lbl) = fv.get_attr("label", heap).ok().flatten()
                {
                    all.insert(to_label(lbl)?, vec![f.clone()]);
                }
                Ok(())
            };
            for (_name, ov) in s.iter() {
                if let Some(list) = starlark::values::list::ListRef::from_value(ov) {
                    for o in list.iter() {
                        insert_file(o, &mut all)?;
                    }
                } else {
                    insert_file(ov, &mut all)?;
                }
            }
        }

        // Directive state machine. `prefix`es are longest-first so
        // `rootpaths ` isn't shadowed by `rootpath `.
        const PREFIXES: [(&str, bool, bool); 6] = [
            ("rootpaths ", true, true),
            ("rootpath ", true, false),
            ("execpaths ", false, true),
            ("execpath ", false, false),
            ("locations ", false, true),
            ("location ", false, false),
        ];
        let mut result = String::new();
        let mut result_until = String::new();
        let mut state = 0u8;
        let mut directive = String::new();
        for c in input.chars() {
            if c == '$' {
                result_until = result.clone();
                result.push(c);
                state = 1;
            } else if c == '(' && state == 1 {
                directive.clear();
                result.push(c);
                state = 2;
            } else if c == ')' && state == 2 {
                let mut expanded: Option<String> = None;
                for (prefix, use_short, plural) in PREFIXES {
                    if let Some(arg) = directive.strip_prefix(prefix) {
                        let l = loupe_labels::Label::resolve(arg, &pkg, &mapping)
                            .map_err(|e| anyhow!("expand_location: label {arg:?}: {e}"))?;
                        let files = all.get(&l).ok_or_else(|| {
                            anyhow!("expand_location: $({directive}) references {arg}, not a declared dependency or output")
                        })?;
                        if !plural && files.len() != 1 {
                            anyhow::bail!(
                                "$({directive}) expands to {} files; the singular form requires exactly one",
                                files.len()
                            );
                        }
                        let parts: Vec<String> = files
                            .iter()
                            .map(|f| if use_short { f.short_path() } else { f.path() })
                            .collect();
                        expanded = Some(parts.join(" "));
                        break;
                    }
                }
                match expanded {
                    Some(e) => {
                        result = result_until.clone();
                        result.push_str(&e);
                    }
                    None => result.push(c),
                }
                state = 0;
            } else {
                directive.push(c);
                result.push(c);
            }
        }
        Ok(result)
    }
}

/// Build `ctx.var`: the make-variable map. Built-ins Bazel synthesises
/// (`BINDIR`/`GENDIR` from `bin_dir`, `COMPILATION_MODE`/`TARGET_CPU`
/// from the native config fields) merged with each resolved toolchain's
/// `TemplateVariableInfo.variables` (`toolchain[TemplateVariableInfo]`
/// via the `__default_toolchains` hidden attr + the instance-level
/// `ctx.attr.toolchains`). Native port of the `_wrap_rule_ctx` `var`
/// construction; toolchain entries override built-ins, matching the
/// wrapper's `var |= …`.
fn build_make_var_map<'v>(
    this: Value<'v>,
    heap: Heap<'v>,
) -> anyhow::Result<starlark::collections::SmallMap<String, String>> {
    let mut map: starlark::collections::SmallMap<String, String> =
        starlark::collections::SmallMap::new();
    let read_str = |attr: &str| -> String {
        this.get_attr(attr, heap)
            .ok()
            .flatten()
            .and_then(|v| v.unpack_str().map(|s| s.to_string()))
            .unwrap_or_default()
    };
    let bin_path = this
        .get_attr("bin_dir", heap)
        .ok()
        .flatten()
        .and_then(|d| d.get_attr("path", heap).ok().flatten())
        .and_then(|p| p.unpack_str().map(|s| s.to_string()))
        .unwrap_or_default();
    map.insert("BINDIR".to_string(), bin_path.clone());
    map.insert("GENDIR".to_string(), bin_path);
    map.insert("COMPILATION_MODE".to_string(), read_str("compilation_mode"));
    map.insert("TARGET_CPU".to_string(), read_str("cpu"));

    // Toolchain `TemplateVariableInfo` merge. The native `Target[T]`
    // returns an empty `TemplateVariableInfo` struct on a miss (see
    // `target.rs`), so reading `.variables` is a no-op for toolchains
    // that don't provide make variables — no explicit `in` guard needed.
    let (tvi_fv, _module) = crate::builtins::template_variable_info_provider()?;
    let tvi = tvi_fv.to_value();
    if let Some(attr) = this.get_attr("attr", heap).ok().flatten() {
        let mut toolchains: Vec<Value<'v>> = Vec::new();
        for name in ["__default_toolchains", "toolchains"] {
            if let Some(list_v) = attr.get_attr(name, heap).ok().flatten()
                && let Some(list) = starlark::values::list::ListRef::from_value(list_v)
            {
                toolchains.extend(list.iter());
            }
        }
        for tc in toolchains {
            let Ok(tvi_inst) = tc.at(tvi, heap) else {
                continue;
            };
            let Some(vars) = tvi_inst.get_attr("variables", heap).ok().flatten() else {
                continue;
            };
            if let Some(dict) = starlark::values::dict::DictRef::from_value(vars) {
                for (k, v) in dict.iter() {
                    if let (Some(k), Some(v)) = (k.unpack_str(), v.unpack_str()) {
                        map.insert(k.to_string(), v.to_string());
                    }
                }
            }
        }
    }
    Ok(map)
}

/// Bazel `$(VAR)` / `$VAR` make-variable expansion over a command
/// string, mirroring `_expand_make_variables` in
/// `builtins_core/wrappers.bzl` (the rule context is migrating native).
/// `get_value` resolves a variable name and raises on an unknown name
/// (matching the Starlark dict access the wrapper used). `$$` is a
/// literal `$`.
fn expand_make_variables(
    command: &str,
    get_value: impl Fn(&str) -> anyhow::Result<String>,
) -> anyhow::Result<String> {
    // 0 = literal, 1 = saw `$`, 2 = inside `$(…)`.
    let mut result = String::new();
    let mut state = 0u8;
    let mut var_name = String::new();
    for c in command.chars() {
        match state {
            0 => {
                if c == '$' {
                    state = 1;
                } else {
                    result.push(c);
                }
            }
            1 => {
                if c == '(' {
                    state = 2;
                } else if c == '$' {
                    result.push('$');
                    state = 0;
                } else {
                    result.push_str(&get_value(&c.to_string())?);
                    state = 0;
                }
            }
            _ => {
                if c == ')' {
                    result.push_str(&get_value(&var_name)?);
                    var_name.clear();
                    state = 0;
                } else {
                    var_name.push(c);
                }
            }
        }
    }
    if state != 0 {
        anyhow::bail!("command terminates in the middle of a $ sequence");
    }
    Ok(result)
}

/// Flatten a `transitive_files`-shaped value into the destination
/// `Vec<File>`. Accepts a Starlark `list[File]`, a `depset(File)`
/// (`Depset<'v>` or `FrozenDepset`), or a `None` no-op; rejects
/// anything else with an error.
pub(crate) fn collect_files_from<'v>(
    v: Value<'v>,
    heap: Heap<'v>,
    out: &mut Vec<File>,
) -> anyhow::Result<()> {
    use crate::starlark_types::depset::{Depset, FrozenDepset};

    if v.is_none() {
        return Ok(());
    }

    // depset(File) — flatten via the type's iterate_collect.
    let depset_items: Option<Vec<Value<'v>>> = if let Some(d) = v.downcast_ref::<Depset<'v>>() {
        Some(
            d.iterate_collect(heap)
                .map_err(|e| anyhow!("depset flatten failed: {e}"))?,
        )
    } else if let Some(d) = v.downcast_ref::<FrozenDepset>() {
        Some(
            d.iterate_collect(heap)
                .map_err(|e| anyhow!("depset flatten failed: {e}"))?,
        )
    } else {
        None
    };
    if let Some(items) = depset_items {
        for item in items {
            let f = item.downcast_ref::<File>().ok_or_else(|| {
                anyhow!(
                    "transitive_files depset element is not a File: {}",
                    item.get_type()
                )
            })?;
            out.push(f.clone());
        }
        return Ok(());
    }

    // Flat list shortcut — `for f in v: out.push(f)`.
    let iter = v
        .iterate(heap)
        .map_err(|e| anyhow!("transitive_files: not a depset, list, or None: {e}"))?;
    for item in iter {
        let f = item.downcast_ref::<File>().ok_or_else(|| {
            anyhow!(
                "transitive_files list element is not a File: {}",
                item.get_type()
            )
        })?;
        out.push(f.clone());
    }
    Ok(())
}
