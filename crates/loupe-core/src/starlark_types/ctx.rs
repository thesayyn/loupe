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
            "label" => Some(heap.alloc(self.label_str.as_str())),
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
            // STUB: empty toolchains map. Real toolchain resolution
            // (`config_setting`, `register_toolchains`) lands later.
            "toolchains" => Some(heap.alloc(AllocDict(SmallMap::<&str, Value<'v>>::new()))),
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
fn ctx_methods(builder: &mut MethodsBuilder) {
    /// `ctx.runfiles(files=[], transitive_files=None, symlinks={},
    /// root_symlinks={})`. v1 builds a flat
    /// [`Runfiles`](super::runfiles::Runfiles).
    fn runfiles<'v>(
        #[starlark(this)] _this: Value<'v>,
        #[starlark(require = named)] files: Option<UnpackList<&'v File>>,
        #[starlark(require = named)] transitive_files: Option<UnpackList<&'v File>>,
        #[starlark(require = named)] symlinks: Option<UnpackDictEntries<String, &'v File>>,
        #[starlark(require = named)] root_symlinks: Option<UnpackDictEntries<String, &'v File>>,
    ) -> anyhow::Result<Runfiles> {
        // STUB: `transitive_files` should accept a `depset(File)`.
        // v1 also accepts a flat list and treats both the same way —
        // good enough for ruleset code that hasn't started using
        // depset accumulation.
        let mut all: Vec<File> = files
            .map(|l| l.items.into_iter().cloned().collect())
            .unwrap_or_default();
        if let Some(t) = transitive_files {
            all.extend(t.items.into_iter().cloned());
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
