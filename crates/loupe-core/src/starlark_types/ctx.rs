//! Analysis-phase `ctx` value — the engine-essential half.
//!
//! Loupe follows bonanza's two-tier ctx pattern: the engine
//! exposes a *minimal* raw `ctx` here, and
//! [`builtins_core/wrappers.bzl`](../../builtins/bzl/builtins_core/wrappers.bzl)'s
//! `_wrap_rule_ctx` wraps it in a Starlark struct that adds derived
//! fields (`build_file_path`, `genfiles_dir`, `expand_location`,
//! `expand_make_variables`, `resolve_command`,
//! `coverage_instrumented`, `target_platform_has_constraint`,
//! `configuration`, `fragments`, `var`, `version_file`, `info_file`,
//! `disabled_features`, `features`). The engine's
//! `analysis::run_analysis` calls `invoke_rule(impl, raw_ctx)`, which
//! does the wrap before the impl runs. Adding a new derived field is
//! a `wrappers.bzl` change, not a Rust change.
//!
//! Engine-essential fields live here because they're tied to DICE /
//! the heap / the analysis driver:
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
//! - `ctx.bin_dir` — `bazel-out/<config>/bin` prefix string. The
//!   `genfiles_dir` alias is added by the wrapper.
//! - `ctx.runfiles(...)` — factory returning a
//!   [`Runfiles`](super::runfiles::Runfiles).
//! - `ctx.exec_groups` — STUB; goes away with the WildcardStruct
//!   removal in the bonanza-pattern Phase 5.

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
    /// `T in ctx.toolchains` go through this map; misses fall back to
    /// a `WildcardStruct` so introspection chains don't halt.
    pub toolchains: V,
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
            attr: self.attr.freeze(freezer)?,
            files: self.files.freeze(freezer)?,
            file: self.file.freeze(freezer)?,
            executable: self.executable.freeze(freezer)?,
            outputs: self.outputs.freeze(freezer)?,
            toolchains: self.toolchains.freeze(freezer)?,
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
            "label" => Some(heap.alloc(crate::starlark_types::compat::LabelValue::parse(
                &self.label_str,
            ))),
            "workspace_name" => Some(heap.alloc(self.workspace_name.as_str())),
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
            // `ToolchainInfo` provider fields. On miss, returns a
            // `WildcardStruct` so introspection chains don't halt.
            // Documented divergence: real Bazel raises on miss.
            "toolchains" => Some(self.toolchains.to_value()),
            // `fragments`, `var`, `configuration`, `build_file_path`,
            // `version_file`, `info_file`, `genfiles_dir`,
            // `disabled_features`, `features`, `coverage_instrumented`,
            // `expand_location`, `expand_make_variables`,
            // `resolve_command`, `target_platform_has_constraint` are
            // Starlark-derived now — see `builtins_core/wrappers.bzl`'s
            // `_wrap_rule_ctx`. The wrapper struct overlays them on
            // top of the raw ctx fields below before the impl sees it.
            // `ctx.outputs.<name>` — implicit-output handle. Built in
            // `analysis::build_outputs_struct` from the rule's
            // `attr.output(...)` / `attr.output_list(...)` attrs and
            // their materialised values. `Output` attrs surface as a
            // `File` (or `None` when the BUILD side didn't supply
            // one); `OutputList` attrs surface as a `list[File]`.
            "outputs" => Some(self.outputs.to_value()),
            // `ctx.bin_dir.path` mirrors what Bazel returns: the
            // `bazel-out/<config>/bin` prefix as a string. Bazel's
            // value is a struct with a `.path` attr; the simple
            // string version is enough for the rulesets that just
            // string-concat it.
            // STUB: returns just the string, not a struct-with-path.
            "bin_dir" => Some(heap.alloc(format!("bazel-out/{}/bin", self.config_segment))),
            // `genfiles_dir` is wrapper-side now — it just aliases
            // `bin_dir` (Bazel's legacy genfiles tree was merged into
            // bin/), and the wrapper provides it as a Starlark field.
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        // Engine-essential fields only. The Starlark-derived ctx
        // surface (build_file_path, fragments, var, configuration,
        // genfiles_dir, …) lives on the wrapper struct
        // `_wrap_rule_ctx` builds in `builtins_core/wrappers.bzl`.
        [
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
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(ctx_methods)
    }
}

#[starlark_module]
#[allow(clippy::too_many_arguments)] // Bazel surface; matches upstream
fn ctx_methods(builder: &mut MethodsBuilder) {
    // `target_platform_has_constraint`, `expand_location`,
    // `resolve_command` migrated to user-space Starlark in
    // `builtins_core/wrappers.bzl`'s `_wrap_rule_ctx`. `exec_groups`
    // wildcard stub stays here for now (Phase 5 deletes it as part
    // of the WildcardStruct removal).

    /// `ctx.exec_groups[name]`. STUB.
    fn exec_groups<'v>(
        #[starlark(this)] _this: Value<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        Ok(heap.alloc(crate::starlark_types::compat::WildcardStruct))
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
        heap: Heap<'v>,
    ) -> anyhow::Result<Runfiles> {
        let _ = (collect_default, collect_data);
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
}

/// Flatten a `transitive_files`-shaped value into the destination
/// `Vec<File>`. Accepts a Starlark `list[File]`, a `depset(File)`
/// (`Depset<'v>` or `FrozenDepset`), or a `None` no-op; rejects
/// anything else with an error.
fn collect_files_from<'v>(v: Value<'v>, heap: Heap<'v>, out: &mut Vec<File>) -> anyhow::Result<()> {
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
