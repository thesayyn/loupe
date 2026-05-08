//! Analysis-phase `ctx` value.
//!
//! The synthesized argument every rule impl receives. v1 ships:
//!
//! - `ctx.attr` — struct of resolved attribute values.
//! - `ctx.label` — configured target's label, as a string for now
//!   (a real Starlark `Label` value is a future-phase swap).
//! - `ctx.workspace_name` — main module name from `MODULE.bazel`.
//! - `ctx.actions` — recording surface, see
//!   [`actions`](super::actions).
//! - `ctx.runfiles(...)` — factory returning a
//!   [`Runfiles`](super::runfiles::Runfiles).
//!
//! The remaining Bazel surface lands as `// STUB:` shims:
//!
//! - `ctx.files` / `ctx.file` / `ctx.executable` — empty until label
//!   deps are wired (`attr.label` / `attr.label_list` resolution +
//!   recursive `AnalysisKey` are the prerequisites).
//! - `ctx.toolchains` — empty dict.
//! - `ctx.fragments` — empty struct.
//! - `ctx.var` — empty dict.
//! - `ctx.bin_dir` / `ctx.genfiles_dir` — strings derived from the
//!   active configuration.
//! - `ctx.configuration` — `None` (real Configuration value type
//!   lands when transitions do).
//! - `ctx.expand_location(s, targets=[])` — passthrough.
//! - `ctx.resolve_command(...)` — empty `(inputs, args, env)`.
//!
//! Each STUB has a `// STUB:` comment so a `grep STUB:` shows the
//! full inventory of what's still in placeholder form.

use allocative::Allocative;
use anyhow::anyhow;
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::starlark_complex_values;
use starlark::starlark_module;
use starlark::values::dict::{AllocDict, UnpackDictEntries};
use starlark::values::list::UnpackList;
use starlark::values::structs::AllocStruct;
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
            // STUB: empty fragments struct. Fragments are the typed
            // `--config`-style configuration data; Phase 5+ piece.
            "fragments" => Some(heap.alloc(AllocStruct::EMPTY)),
            // STUB: empty var dict. Bazel populates this from
            // `--define` and toolchain-provided make-vars.
            "var" => Some(heap.alloc(AllocDict(SmallMap::<&str, Value<'v>>::new()))),
            // STUB: configuration value. Bazel's `ctx.configuration`
            // exposes platform / `--config` data; we expose `None`
            // until real Configuration plumbing arrives.
            "configuration" => Some(Value::new_none()),
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
            // STUB: same shape as `bin_dir`. Bazel's `genfiles_dir`
            // points at the legacy genfiles tree which has been
            // merged into `bin/`; we just point both at `bin/`.
            "genfiles_dir" => Some(heap.alloc(format!("bazel-out/{}/bin", self.config_segment))),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        [
            "attr",
            "label",
            "workspace_name",
            "actions",
            "files",
            "file",
            "executable",
            "toolchains",
            "fragments",
            "var",
            "configuration",
            "outputs",
            "bin_dir",
            "genfiles_dir",
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
    /// `ctx.target_platform_has_constraint(constraint_value)` —
    /// Bazel returns whether the target platform has the given
    /// constraint. STUB: always False (v1 doesn't model platforms).
    fn target_platform_has_constraint<'v>(
        #[starlark(this)] _this: Value<'v>,
        #[starlark(require = pos)] _constraint_value: Value<'v>,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }

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

    /// `ctx.expand_location(input, targets=[])`. v1 passthrough —
    /// real `$(location ...)` substitution needs the dep graph.
    /// STUB: ignores `targets`.
    fn expand_location<'v>(
        #[starlark(this)] _this: Value<'v>,
        #[starlark(require = pos)] input: &str,
        #[starlark(require = named)] targets: Option<Value<'v>>,
    ) -> anyhow::Result<String> {
        // STUB: does not perform any substitution. A future-phase
        // implementation parses `$(location <label>)` and resolves
        // it against the configured-target graph.
        let _ = targets;
        Ok(input.to_string())
    }

    /// `ctx.resolve_command(...)`. STUB: returns `([], [], {})`.
    #[allow(clippy::too_many_arguments)] // matches Bazel's surface
    fn resolve_command<'v>(
        #[starlark(this)] _this: Value<'v>,
        #[starlark(require = named)] command: Option<&str>,
        #[starlark(require = named)] attribute: Option<&str>,
        #[starlark(require = named)] expand_locations: Option<bool>,
        #[starlark(require = named)] make_variables: Option<Value<'v>>,
        #[starlark(require = named)] tools: Option<Value<'v>>,
        #[starlark(require = named)] label_dict: Option<Value<'v>>,
        #[starlark(require = named)] execution_requirements: Option<Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (
            command,
            attribute,
            expand_locations,
            make_variables,
            tools,
            label_dict,
            execution_requirements,
        );
        // STUB: Bazel returns `(inputs, command, environment)`; the
        // real implementation interprets `$(execpath ...)`-style
        // substitutions and locates `tools`. v1 returns the empty
        // tuple-shape so impls that destructure don't crash.
        let empty_inputs: Vec<Value<'v>> = Vec::new();
        let empty_args: Vec<Value<'v>> = Vec::new();
        let env: SmallMap<&str, Value<'v>> = SmallMap::new();
        let inputs = heap.alloc(empty_inputs);
        let args = heap.alloc(empty_args);
        let env = heap.alloc(AllocDict(env));
        Ok(heap.alloc((inputs, args, env)))
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
