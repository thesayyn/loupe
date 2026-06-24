//! `ctx.actions` — the recording API rule impls call.
//!
//! This is the Starlark-facing surface. The actual recording happens
//! through the [`AnalysisContext`](crate::analysis::AnalysisContext)
//! sitting in `Evaluator::extra`, mirroring the
//! `BuildContext`-on-`extra` pattern from BUILD evaluation.
//!
//! v1 ships the four flavors fixture-style ruleset code reaches for:
//!
//! - `ctx.actions.declare_file(name, sibling=None)` → [`File`]
//! - `ctx.actions.write(output, content, is_executable=False)`
//! - `ctx.actions.run(executable, arguments=[], inputs=[], outputs=[],
//!    env={}, mnemonic=None, progress_message=None)`
//! - `ctx.actions.run_shell(command, arguments=[], inputs=[],
//!    outputs=[], env={}, mnemonic=None, progress_message=None)`
//! - `ctx.actions.symlink(output, target_file)`
//!
//! Bazel's full surface (`expand_template`, `args()`, `param_file`,
//! `declare_directory`, `declare_symlink`, …) is out of scope until a
//! ruleset surfaces a need.
//!
//! Function signatures use starlark-rust's typed unpack helpers
//! (`&File`, [`UnpackList`], [`UnpackDictEntries`]) so the macro
//! handles type conversion at the call boundary — bad arg types
//! produce starlark-shaped errors rather than `unpack_*` panics.

use std::collections::BTreeMap;
use std::sync::Mutex;

use allocative::Allocative;
use anyhow::anyhow;
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::dict::{AllocDict, UnpackDictEntries};
use starlark::values::list::UnpackList;
use starlark::values::none::NoneType;
use starlark::values::{
    Heap, NoSerialize, StarlarkValue, UnpackValue, Value, ValueLike, starlark_value,
};

use crate::action::{Action, ActionKind};
use crate::analysis::AnalysisContext;
use crate::starlark_types::file::File;

/// `ctx.actions` value. Stateless — recording flows through
/// `Evaluator::extra → AnalysisContext`.
#[derive(Debug, Clone, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("<ctx.actions>")]
pub struct Actions;

starlark_simple_value!(Actions);

#[starlark_value(type = "actions")]
impl<'v> StarlarkValue<'v> for Actions {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(actions_methods)
    }
}

starlark_simple_value!(Action);

/// Starlark surface of [`Action`] — what `target.actions[i]` reads.
/// Bazel surface: `mnemonic`, `inputs`, `outputs`, `argv`, `env`,
/// plus `content` for Write only. `inputs` and `outputs` are flat
/// lists (STUB: should be `depset(File)`).
#[starlark_value(type = "Action")]
impl<'v> StarlarkValue<'v> for Action {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "mnemonic" => Some(heap.alloc(self.mnemonic.as_deref().unwrap_or(""))),
            "inputs" => {
                let xs: Vec<Value<'v>> =
                    self.inputs.iter().map(|f| heap.alloc(f.clone())).collect();
                Some(heap.alloc(xs))
            }
            "outputs" => {
                let xs: Vec<Value<'v>> =
                    self.outputs.iter().map(|f| heap.alloc(f.clone())).collect();
                Some(heap.alloc(xs))
            }
            "argv" => {
                let argv: Vec<&str> = match &self.kind {
                    ActionKind::Run { arguments, .. } | ActionKind::RunShell { arguments, .. } => {
                        arguments.iter().map(|s| s.as_str()).collect()
                    }
                    ActionKind::Write { .. }
                    | ActionKind::Symlink { .. }
                    | ActionKind::ExpandTemplate { .. } => Vec::new(),
                };
                Some(heap.alloc(argv))
            }
            "env" => {
                let entries: SmallMap<&str, &str> = self
                    .env
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                Some(heap.alloc(AllocDict(entries)))
            }
            "execution_requirements" => {
                // Recorded per-action `execution_requirements` — the
                // `ctx.actions.run(execution_requirements=...)` kwarg.
                // Surfaced as a Starlark dict so aquery-style consumers
                // can read the per-action overrides off
                // `target.actions[i].execution_requirements`. Bazel's
                // own `Action.execution_info` exposes the same shape.
                let entries: SmallMap<&str, &str> = self
                    .execution_requirements
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                Some(heap.alloc(AllocDict(entries)))
            }
            "content" => match &self.kind {
                ActionKind::Write { content, .. } => Some(heap.alloc(content.as_str())),
                _ => None,
            },
            // Bazel surfaces this through `ctx.actions.run(...).use_default_shell_env`
            // — i.e. it's a property of the recorded action, not a separate
            // ctx call. aquery / cquery consumers read it to reconstruct
            // the REAPI Command env map.
            "uses_default_shell_env" => Some(heap.alloc(self.uses_default_shell_env)),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        let mut v = vec![
            "mnemonic",
            "inputs",
            "outputs",
            "argv",
            "env",
            "execution_requirements",
            "uses_default_shell_env",
        ];
        if matches!(self.kind, ActionKind::Write { .. }) {
            v.push("content");
        }
        v.iter().map(|s| s.to_string()).collect()
    }
}

/// `ctx.actions.args()` value — Bazel's lazy command-line builder.
///
/// Bazel models `Args` as a deferred sequence: `args.add(...)` and
/// `args.add_all(...)` enqueue work, and the action driver realizes
/// the final argv only when the action runs. v1 simplifies to **eager
/// materialization**: each method appends ready-made strings to an
/// internal `RefCell<Vec<String>>`. `arguments=[args]` on
/// `ctx.actions.run` flattens the Args into the recorded `argv`.
///
/// What this gives up vs Bazel: lazy `expand_directories` of tree
/// artifacts, `format_each` deferred over depsets, `use_param_file` /
/// param-file generation. None of that affects the cquery comparison
/// for v1 — the recorded argv matches what an executor would receive.
#[derive(Debug, NoSerialize, ProvidesStaticType, derive_more::Display, Allocative)]
#[display("<Args>")]
pub struct Args {
    /// `Mutex` rather than `RefCell` because `starlark_simple_value!`
    /// requires `Sync`. Contention is impossible — `Args` is built
    /// inside a single rule impl and never crosses threads — so the
    /// mutex is purely a structural workaround.
    #[allocative(skip)]
    pub pieces: Mutex<Vec<String>>,
}

impl Args {
    pub fn new() -> Self {
        Args {
            pieces: Mutex::new(Vec::new()),
        }
    }

    pub fn snapshot(&self) -> Vec<String> {
        self.pieces.lock().expect("Args mutex poisoned").clone()
    }

    fn push(&self, s: String) {
        self.pieces.lock().expect("Args mutex poisoned").push(s);
    }
}

impl Default for Args {
    fn default() -> Self {
        Args::new()
    }
}

starlark_simple_value!(Args);

#[starlark_value(type = "Args")]
impl<'v> StarlarkValue<'v> for Args {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(args_methods)
    }
}

#[starlark_module]
fn args_methods(builder: &mut MethodsBuilder) {
    /// `Args.add(arg_name_or_value, value=None, format=None)` —
    /// matches Bazel's two-form signature:
    /// - `add(value)` pushes one stringified arg.
    /// - `add(flag, value)` pushes both, in that order. `format`
    ///   (when set) applies to `value`, not `flag`.
    fn add<'v>(
        #[starlark(this)] this: &Args,
        #[starlark(require = pos)] arg_name_or_value: Value<'v>,
        #[starlark(require = pos)] value: Option<Value<'v>>,
        #[starlark(require = named)] format: Option<&str>,
    ) -> anyhow::Result<NoneType> {
        match value {
            None => {
                // One-positional form: stringify and push.
                let s = stringify_arg(arg_name_or_value)?;
                this.push(match format {
                    Some(fmt) => fmt.replace("%s", &s),
                    None => s,
                });
            }
            Some(v) => {
                // Two-positional form: push the flag literal first,
                // then the (optionally formatted) value. `format`
                // applies to `value`, not `flag` — matches Bazel.
                let flag = stringify_arg(arg_name_or_value)?;
                let val = stringify_arg(v)?;
                this.push(flag);
                this.push(match format {
                    Some(fmt) => fmt.replace("%s", &val),
                    None => val,
                });
            }
        }
        Ok(NoneType)
    }

    /// `Args.add_all(values, format_each=None, before_each=None,
    /// map_each=None, omit_if_empty=True, uniquify=False,
    /// expand_directories=True, terminate_with=None,
    /// allow_closure=False)`.
    ///
    /// `format_each` and `before_each` are honoured. The rest are
    /// accept-and-ignore: v1 records argv structure for analysis-time
    /// inspection but doesn't execute actions, so map_each transforms,
    /// directory expansion, dedupe, and the terminate-with sentinel
    /// would only matter at execution time. They're surface kwargs the
    /// rules_oci / aspect_bazel_lib chain reaches for; without them
    /// `add_all(map_each=...)` errors out at the call site even though
    /// the analysis result wouldn't differ.
    #[allow(clippy::too_many_arguments)] // matches Bazel's surface
    fn add_all<'v>(
        #[starlark(this)] this: &Args,
        // Bazel's `add_all` has two positional forms:
        //   `add_all(values, …)`            — `arg_or_values` is the list
        //   `add_all(arg_name, values, …)`  — `arg_or_values` is a
        //                                      literal string prepended
        //                                      once, `values` is the list
        // We model that by taking two positionals and disambiguating on
        // whether the second was supplied. rules_js's
        // `npm_package_store.bzl` uses the 2-arg form
        // (`args.add_all("--directory", [dir], …)`).
        #[starlark(require = pos)] arg_or_values: Value<'v>,
        #[starlark(require = pos)] values: Option<Value<'v>>,
        #[starlark(require = named)] format_each: Option<&str>,
        #[starlark(require = named)] before_each: Option<&str>,
        #[starlark(require = named)] map_each: Option<Value<'v>>,
        #[starlark(require = named)] omit_if_empty: Option<bool>,
        #[starlark(require = named)] uniquify: Option<bool>,
        #[starlark(require = named)] expand_directories: Option<bool>,
        #[starlark(require = named)] terminate_with: Option<&str>,
        #[starlark(require = named)] allow_closure: Option<bool>,
    ) -> anyhow::Result<NoneType> {
        let _ = (
            map_each,
            omit_if_empty,
            uniquify,
            expand_directories,
            allow_closure,
        );
        let (arg_name, list_value) = match values {
            Some(v) => (arg_or_values.unpack_str().map(str::to_string), v),
            None => (None, arg_or_values),
        };
        if let Some(name) = arg_name {
            this.push(name);
        }
        // Bazel's `add_all` accepts a list/tuple OR a `depset`
        // (js_image_layer passes a depset of runfiles). Try list first;
        // fall back to flattening a depset.
        let items: Vec<Value<'v>> = match UnpackList::<Value<'v>>::unpack_value(list_value)
            .ok()
            .flatten()
        {
            Some(l) => l.items,
            None => crate::starlark_types::depset::flatten_to_values(list_value).map_err(|_| {
                anyhow::anyhow!(
                    "Args.add_all: values must be a list or depset, got {}",
                    list_value.get_type()
                )
            })?,
        };
        for v in items {
            if let Some(prefix) = before_each {
                this.push(prefix.to_string());
            }
            let s = stringify_arg(v)?;
            this.push(match format_each {
                Some(fmt) => fmt.replace("%s", &s),
                None => s,
            });
        }
        if let Some(term) = terminate_with {
            this.push(term.to_string());
        }
        Ok(NoneType)
    }

    /// `Args.add_joined(values, join_with, format_each=None)` —
    /// joins each formatted value with `join_with` and adds the result
    /// as a single argument.
    /// `Args.add_joined(values, join_with, format_each=None,
    /// format_joined=None, map_each=None, omit_if_empty=True,
    /// uniquify=False, expand_directories=True, allow_closure=False)`.
    /// Joins formatted values with `join_with`, optionally wrapping
    /// the joined result via `format_joined` (`"%s"` → joined
    /// string). Other kwargs are accept-and-ignore (same v1 reasoning
    /// as `add_all` — no execution means map_each / dedupe / dir
    /// expansion are no-ops).
    #[allow(clippy::too_many_arguments)] // matches Bazel's surface
    fn add_joined<'v>(
        #[starlark(this)] this: &Args,
        // Bazel's `add_joined` accepts either a `list` or a `depset`
        // for `values` (rules_js's `js_run_devserver` passes a depset).
        // Take the raw Value and iterate — `iterate` flattens both.
        #[starlark(require = pos)] values: Value<'v>,
        #[starlark(require = named)] join_with: &str,
        #[starlark(require = named)] format_each: Option<&str>,
        #[starlark(require = named)] format_joined: Option<&str>,
        #[starlark(require = named)] map_each: Option<Value<'v>>,
        #[starlark(require = named)] omit_if_empty: Option<bool>,
        #[starlark(require = named)] uniquify: Option<bool>,
        #[starlark(require = named)] expand_directories: Option<bool>,
        #[starlark(require = named)] allow_closure: Option<bool>,
        heap: Heap<'v>,
    ) -> anyhow::Result<NoneType> {
        let _ = (
            map_each,
            omit_if_empty,
            uniquify,
            expand_directories,
            allow_closure,
        );
        let mut formatted: Vec<String> = Vec::new();
        let iter = values
            .iterate(heap)
            .map_err(|e| anyhow!("add_joined: values not iterable: {e}"))?;
        for v in iter {
            let s = stringify_arg(v)?;
            formatted.push(match format_each {
                Some(fmt) => fmt.replace("%s", &s),
                None => s,
            });
        }
        let joined = formatted.join(join_with);
        let final_arg = match format_joined {
            Some(fmt) => fmt.replace("%s", &joined),
            None => joined,
        };
        this.push(final_arg);
        Ok(NoneType)
    }

    /// `Args.use_param_file(format, use_always=False)`. STUB —
    /// recorded but ignored; v1 doesn't generate param files. Returns
    /// the receiver so Bazel's chaining (`args().use_param_file(...)`)
    /// works.
    fn use_param_file<'v>(
        #[starlark(this)] this: Value<'v>,
        #[starlark(require = pos)] _format: &str,
        #[starlark(require = named, default = false)] _use_always: bool,
    ) -> anyhow::Result<Value<'v>> {
        Ok(this)
    }

    /// `Args.set_param_file_format(format)`. STUB. Returns the receiver
    /// for chaining — Bazel's `Args` mutators are all fluent, and
    /// `ctx.actions.args().set_param_file_format(...)` relies on it.
    fn set_param_file_format<'v>(
        #[starlark(this)] this: Value<'v>,
        #[starlark(require = pos)] _format: &str,
    ) -> anyhow::Result<Value<'v>> {
        Ok(this)
    }
}

/// Convert a single `arguments` element to its argv string. Strings
/// pass through; `File`s become their `path`. Other types stringify
/// via `Value::to_str`.
fn stringify_arg<'v>(v: Value<'v>) -> anyhow::Result<String> {
    if let Some(s) = v.unpack_str() {
        return Ok(s.to_string());
    }
    if let Some(f) = v.downcast_ref::<File>() {
        return Ok(f.path());
    }
    Ok(v.to_str())
}

/// Flatten a heterogeneous `arguments=[...]` list into the argv vec
/// recorded on the action. Each element is either a string, a `File`,
/// or an `Args` (whose contents get spliced in order).
fn flatten_arguments<'v>(items: Vec<Value<'v>>) -> anyhow::Result<Vec<String>> {
    let mut out: Vec<String> = Vec::with_capacity(items.len());
    for v in items {
        if let Some(args) = v.downcast_ref::<Args>() {
            out.extend(args.snapshot());
        } else {
            out.push(stringify_arg(v)?);
        }
    }
    Ok(out)
}

fn ctx_from_eval<'a, 'v>(eval: &'a Evaluator<'v, '_, '_>) -> anyhow::Result<&'a AnalysisContext> {
    eval.extra
        .ok_or_else(|| anyhow!("internal: ctx.actions called outside an AnalysisContext"))?
        .downcast_ref::<AnalysisContext>()
        .ok_or_else(|| anyhow!("internal: Evaluator::extra is not an AnalysisContext"))
}

/// Convert an unpacked list of `&File` references into owned `File`
/// values for storage in an [`Action`].
fn own_files(items: Option<UnpackList<&File>>) -> Vec<File> {
    items
        .map(|l| l.items.into_iter().cloned().collect())
        .unwrap_or_default()
}

/// Bazel's `inputs=` / `tools=` accept either `list[File]` or
/// `depset(File)`. v1 flattens both into `Vec<File>`. STUB: depset
/// shape isn't preserved on the recorded `Action`.
fn flatten_files_arg<'v>(v: Option<Value<'v>>, heap: Heap<'v>) -> anyhow::Result<Vec<File>> {
    let v = match v {
        Some(v) if !v.is_none() => v,
        _ => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    // Iterate (works for both list and depset).
    let iter = v
        .iterate(heap)
        .map_err(|e| anyhow!("inputs/tools: not iterable: {e}"))?;
    for item in iter {
        if let Some(f) = item.downcast_ref::<File>() {
            out.push(f.clone());
        }
        // STUB: silently drops non-File entries; real Bazel rejects.
    }
    Ok(out)
}

/// Unpack the `executable=` argument to `actions.run` into a `File`.
/// Accepts:
/// - `File` (the canonical Bazel shape).
/// - `str` — wrapped in a synthetic source-rooted `File` with the
///   string as `short_path`. The vendored wrappers.bzl rewrites
///   `actions.run_shell(command=…)` to
///   `actions.run(executable="/bin/bash", arguments=["-c", command,
///   "", *args])`; the recorded action keeps the `/bin/bash` path.
///
/// Other shapes — including `None` — raise. Real Bazel rejects them
/// too, and silently accepting a `None` executable would mask rule
/// bugs (an action with no executable can't run).
fn unpack_executable_arg<'v>(
    v: Value<'v>,
    actx: &crate::analysis::AnalysisContext,
) -> anyhow::Result<File> {
    if let Some(f) = v.downcast_ref::<File>() {
        return Ok(f.clone());
    }
    if let Some(s) = v.unpack_str() {
        return Ok(File::source(actx.package.clone(), s)
            .with_layout(actx.configuration.repository_layout()));
    }
    Err(anyhow!(
        "actions.run: executable must be a File or str; got {}",
        v.get_type()
    ))
}

fn unpack_str_dict<'v>(v: Option<Value<'v>>) -> Option<UnpackDictEntries<String, String>> {
    let v = v?;
    if v.is_none() {
        return None;
    }
    let dict = starlark::values::dict::DictRef::from_value(v)?;
    let mut entries = Vec::with_capacity(dict.len());
    for (k, val) in dict.iter() {
        let key = k.unpack_str()?.to_string();
        let value = val.unpack_str()?.to_string();
        entries.push((key, value));
    }
    Some(UnpackDictEntries { entries })
}

fn own_env(env: Option<UnpackDictEntries<String, String>>) -> BTreeMap<String, String> {
    env.map(|d| d.entries.into_iter().collect())
        .unwrap_or_default()
}

#[starlark_module]
#[allow(clippy::too_many_arguments)] // generated ctor; Bazel surface
fn actions_methods(builder: &mut MethodsBuilder) {
    /// `ctx.actions.declare_file(name, sibling=None)` → `File`.
    /// Allocates a generated-bin `File` rooted in the analyzed
    /// target's package. `sibling` (Bazel: place the new file next
    /// to an existing one) is accepted but only its package is
    /// honored in v1 — full path-rebasing is a follow-up.
    ///
    /// Bazel has no `executable=` kwarg here — the REAPI
    /// `FileNode.is_executable` bit on outputs is set only by
    /// `ctx.actions.write(..., is_executable=True)` /
    /// `ctx.actions.expand_template(..., is_executable=True)`, which
    /// rewrites the recorded output File. Files declared here always
    /// start with `is_executable = false`; the producing action flips
    /// the bit when it runs.
    fn declare_file<'v>(
        #[starlark(this)] _this: &Actions,
        #[starlark(require = pos)] name: &str,
        #[starlark(require = named, default = NoneType)] sibling: starlark::values::Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<File> {
        let actx = ctx_from_eval(eval)?;
        // STUB: `sibling` only contributes its package today. Full
        // Bazel semantics also rewrite the short_path's parent dir
        // to match the sibling's. Lands when a ruleset needs it.
        let _ = sibling;
        let file = File::generated_bin(
            actx.package.clone(),
            name.to_string(),
            actx.configuration.segment(),
        )
        .with_layout(actx.configuration.repository_layout());
        actx.declared_files
            .borrow_mut()
            .insert(file.short_path(), file.clone());
        Ok(file)
    }

    /// `ctx.actions.declare_shareable_artifact(path, artifact_root=None)`
    /// → `File`. Native port of the `_wrap_actions` closure: strips the
    /// `<workspace_root>/<package>/` prefix off `path` and declares the
    /// remainder as a generated-bin file. STUB: `artifact_root` is
    /// accepted and ignored (only `bin_dir` is modelled), and the
    /// external-repo `workspace_root` isn't modelled (main repo = `""`).
    /// Used only by the vendored cc/objc/python rules, not the v1
    /// `rules_oci`/`rules_js` targets.
    fn declare_shareable_artifact<'v>(
        #[starlark(this)] _this: &Actions,
        #[starlark(require = pos)] path: &str,
        #[starlark(require = pos, default = NoneType)] artifact_root: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<File> {
        let _ = artifact_root;
        let actx = ctx_from_eval(eval)?;
        let pkg = actx.package.path.as_str();
        let mut prefix = String::from("/");
        if !pkg.is_empty() {
            prefix.push_str(pkg);
            prefix.push('/');
        }
        let rel = path.strip_prefix(&prefix).ok_or_else(|| {
            anyhow!("declare_shareable_artifact: path {path:?} does not start with {prefix:?}")
        })?;
        let file = File::generated_bin(
            actx.package.clone(),
            rel.to_string(),
            actx.configuration.segment(),
        )
        .with_layout(actx.configuration.repository_layout());
        actx.declared_files
            .borrow_mut()
            .insert(file.short_path(), file.clone());
        Ok(file)
    }

    /// `ctx.actions.args()` → `Args`. Bazel's lazy command-line
    /// builder; v1 materializes eagerly (see [`Args`] docs).
    fn args<'v>(#[starlark(this)] _this: &Actions, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        Ok(heap.alloc(Args::new()))
    }

    /// `ctx.actions.transform_version_file(transform_func, template,
    /// output_file_name)` → `File`. Bazel's build-stamp helper, used by
    /// the cc toolchain's `bazel_cc_build_info` rule to turn the
    /// workspace-status/version file into a generated header. Loupe is
    /// read-only and never runs the transform, so it just declares the
    /// output artifact (a generated-bin `File` named `output_file_name`);
    /// `transform_func` and `template` are inert.
    fn transform_version_file<'v>(
        #[starlark(this)] _this: &Actions,
        #[starlark(require = named, default = NoneType)] transform_func: Value<'v>,
        #[starlark(require = named, default = NoneType)] template: Value<'v>,
        #[starlark(require = named)] output_file_name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<File> {
        let _ = (transform_func, template);
        let actx = ctx_from_eval(eval)?;
        let file = File::generated_bin(
            actx.package.clone(),
            output_file_name.to_string(),
            actx.configuration.segment(),
        )
        .with_layout(actx.configuration.repository_layout());
        actx.declared_files
            .borrow_mut()
            .insert(file.short_path(), file.clone());
        Ok(file)
    }

    /// `ctx.actions.transform_info_file(transform_func, template,
    /// output_file_name)` → `File`. Sibling of `transform_version_file`
    /// for the non-volatile (workspace-status info) build-stamp header.
    /// Same read-only treatment: declare the output, inert transform.
    fn transform_info_file<'v>(
        #[starlark(this)] _this: &Actions,
        #[starlark(require = named, default = NoneType)] transform_func: Value<'v>,
        #[starlark(require = named, default = NoneType)] template: Value<'v>,
        #[starlark(require = named)] output_file_name: &str,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<File> {
        let _ = (transform_func, template);
        let actx = ctx_from_eval(eval)?;
        let file = File::generated_bin(
            actx.package.clone(),
            output_file_name.to_string(),
            actx.configuration.segment(),
        )
        .with_layout(actx.configuration.repository_layout());
        actx.declared_files
            .borrow_mut()
            .insert(file.short_path(), file.clone());
        Ok(file)
    }

    /// `ctx.actions.declare_directory(name, sibling=None)` → `File`.
    /// Tree artifact — a generated path Bazel treats as a directory at
    /// runtime. Loupe records the declaration; we don't model the
    /// contents (STUB). Consumers see `File.is_directory == True`.
    ///
    /// Same `executable=` absence as `declare_file` — Bazel doesn't
    /// expose the kwarg here, so the tree artifact's `is_executable`
    /// stays false. Individual files inside the tree pick up their
    /// modes from the producing action at execution time; nothing
    /// the analysis surface can express.
    fn declare_directory<'v>(
        #[starlark(this)] _this: &Actions,
        #[starlark(require = pos)] name: &str,
        #[starlark(require = named, default = NoneType)] sibling: starlark::values::Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<File> {
        let actx = ctx_from_eval(eval)?;
        let _ = sibling; // STUB: same as declare_file — sibling ignored.
        let file = File::generated_bin_dir(
            actx.package.clone(),
            name.to_string(),
            actx.configuration.segment(),
        )
        .with_layout(actx.configuration.repository_layout());
        actx.declared_files
            .borrow_mut()
            .insert(file.short_path(), file.clone());
        Ok(file)
    }

    /// `ctx.actions.declare_symlink(name, sibling=None)` → `File`.
    /// Declares an unresolved-symlink output. rules_js's
    /// `npm_package_store.bzl` declares one per linked package then
    /// `ctx.actions.symlink(output=…, target_path=…)`s into it. v1
    /// records the declaration as a regular generated `File` (we don't
    /// model the symlink-vs-regular distinction on outputs); the
    /// producing `symlink` action carries the target.
    fn declare_symlink<'v>(
        #[starlark(this)] _this: &Actions,
        #[starlark(require = pos)] name: &str,
        #[starlark(require = named, default = NoneType)] sibling: starlark::values::Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<File> {
        let actx = ctx_from_eval(eval)?;
        let _ = sibling; // STUB: same as declare_file — sibling ignored.
        let file = File::generated_bin(
            actx.package.clone(),
            name.to_string(),
            actx.configuration.segment(),
        )
        .with_layout(actx.configuration.repository_layout());
        actx.declared_files
            .borrow_mut()
            .insert(file.short_path(), file.clone());
        Ok(file)
    }

    /// `ctx.actions.write(output, content, is_executable=False)`.
    /// Records a [`Write`](ActionKind::Write) action. `output` /
    /// `content` accept positional or named binding to match Bazel's
    /// surface — `aspect_bazel_lib`'s `mtree_spec` calls
    /// `ctx.actions.write(out, content = content)`. `content` is
    /// accepted as either `str` or `Args` (Bazel allows the latter
    /// to dump the formatted argv into a param file); v1 records the
    /// joined argv and treats it as static content.
    fn write<'v>(
        #[starlark(this)] _this: &Actions,
        output: &'v File,
        content: Value<'v>,
        #[starlark(require = named, default = false)] is_executable: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let content_str = if let Some(s) = content.unpack_str() {
            s.to_string()
        } else if let Some(args) = content.downcast_ref::<Args>() {
            args.snapshot().join("\n")
        } else {
            return Err(anyhow!(
                "ctx.actions.write content must be str or Args, got {}",
                content.get_type()
            ));
        };
        let actx = ctx_from_eval(eval)?;
        // Flow the rule-author's `is_executable=True` knob onto the
        // output File so REAPI AC-key derivation reads the right
        // `FileNode.is_executable` varint. The Action carries the
        // flag too (so executors/aquery still see it), but the File
        // is the canonical source for the merkle tree.
        let output_file = output.clone().with_is_executable(is_executable);
        actx.actions.borrow_mut().push(Action {
            kind: ActionKind::Write {
                content: content_str,
                is_executable,
            },
            mnemonic: Some("FileWrite".to_string()),
            progress_message: None,
            inputs: Vec::new(),
            outputs: vec![output_file],
            env: BTreeMap::new(),
            // `ctx.actions.write` doesn't take `execution_requirements`
            // in Bazel; the recorded shape just carries an empty map.
            execution_requirements: BTreeMap::new(),
            // Write doesn't shell out — the bit is meaningless here.
            uses_default_shell_env: false,
        });
        Ok(NoneType)
    }

    /// `ctx.actions.run(executable, arguments=[], inputs=[],
    /// outputs=[], tools=[], env={}, mnemonic=None,
    /// progress_message=None, execution_requirements={})`.
    ///
    /// `arguments` accepts a heterogeneous list of strings, `File`s,
    /// and `Args` objects; `Args` get flattened in place. `tools` adds
    /// extra executable inputs (Bazel models them separately so the
    /// executor can stage runfiles for each tool — v1 just folds them
    /// into `inputs`). `execution_requirements` is recorded onto the
    /// `Action` and surfaced through `Action.execution_requirements`;
    /// consumers (aquery, AC-key emitters) overlay it on top of the
    /// configuration's `Platform.properties` — see
    /// [`crate::action::Action::execution_requirements`].
    #[allow(clippy::too_many_arguments)] // matches Bazel's surface
    fn run<'v>(
        #[starlark(this)] _this: &Actions,
        // Accepts `File`, a bare string (treated as an absolute path
        // — the vendored wrappers.bzl rewrites `actions.run_shell` to
        // `actions.run(executable="/bin/bash", …)`), or `None`
        // (rules_js's `js_image_layer` passes `None` for layers
        // that use a custom commandline). Synthesised into a File so
        // the recorded action still carries an `executable` value
        // downstream consumers (`target.actions[N].argv[0]`) can read.
        #[starlark(require = named)] executable: Value<'v>,
        #[starlark(require = named)] arguments: Option<UnpackList<Value<'v>>>,
        // Bazel accepts both list[File] and depset(File) for these.
        // v1 unpacks both via `flatten_files_arg` (STUB-flat).
        #[starlark(require = named)] inputs: Option<Value<'v>>,
        #[starlark(require = named)] outputs: Option<UnpackList<&'v File>>,
        #[starlark(require = named)] tools: Option<Value<'v>>,
        #[starlark(require = named)] env: Option<Value<'v>>,
        #[starlark(require = named)] mnemonic: Option<Value<'v>>,
        #[starlark(require = named)] progress_message: Option<Value<'v>>,
        #[starlark(require = named)] execution_requirements: Option<Value<'v>>,
        // STUB: accepted, ignored. Real toolchain resolution would
        // route via toolchain= here.
        #[starlark(require = named)] toolchain: Option<Value<'v>>,
        // STUB: accepted, ignored. resource_set is a callback Bazel
        // uses for action-scheduling cost; v1 doesn't schedule.
        #[starlark(require = named)] resource_set: Option<Value<'v>>,
        // STUB: accepted, ignored. `unused_inputs_list` is an output
        // File declaring which inputs went unused (a pruning hint);
        // rules_js's `js_image_layer` splitter passes it. v1 doesn't
        // prune inputs.
        #[starlark(require = named)] unused_inputs_list: Option<Value<'v>>,
        #[starlark(require = named)] use_default_shell_env: Option<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (toolchain, resource_set, unused_inputs_list);
        let actx = ctx_from_eval(eval)?;
        let env = unpack_str_dict(env);
        // `execution_requirements` is Bazel's per-action override /
        // addition on top of the configuration's `Platform.properties`.
        // Recorded onto the Action; consumers (aquery, AC-key emitters)
        // overlay it against `Configuration::exec_properties`.
        let exec_req = unpack_str_dict(execution_requirements);
        let argv = arguments
            .map(|l| flatten_arguments(l.items))
            .transpose()?
            .unwrap_or_default();
        let mut all_inputs = flatten_files_arg(inputs, eval.heap())?;
        all_inputs.extend(flatten_files_arg(tools, eval.heap())?);
        let mnemonic_str = mnemonic.and_then(|v| v.unpack_str().map(str::to_string));
        let progress_str = progress_message.and_then(|v| v.unpack_str().map(str::to_string));
        let executable_file = unpack_executable_arg(executable, actx)?;
        actx.actions.borrow_mut().push(Action {
            kind: ActionKind::Run {
                executable: executable_file,
                arguments: argv,
            },
            mnemonic: mnemonic_str,
            progress_message: progress_str,
            inputs: all_inputs,
            outputs: own_files(outputs),
            env: own_env(env),
            execution_requirements: own_env(exec_req),
            uses_default_shell_env: use_default_shell_env.unwrap_or(false),
        });
        Ok(NoneType)
    }

    /// `ctx.actions.run_shell(command, arguments=[], inputs=[],
    /// outputs=[], tools=[], env={}, mnemonic=None,
    /// progress_message=None, execution_requirements={})`.
    #[allow(clippy::too_many_arguments)] // matches Bazel's surface
    fn run_shell<'v>(
        #[starlark(this)] _this: &Actions,
        #[starlark(require = named)] command: &str,
        #[starlark(require = named)] arguments: Option<UnpackList<Value<'v>>>,
        // Bazel accepts both list[File] and depset(File) for these.
        // v1 unpacks both via `flatten_files_arg` (STUB-flat).
        #[starlark(require = named)] inputs: Option<Value<'v>>,
        #[starlark(require = named)] outputs: Option<UnpackList<&'v File>>,
        #[starlark(require = named)] tools: Option<Value<'v>>,
        #[starlark(require = named)] env: Option<Value<'v>>,
        #[starlark(require = named)] mnemonic: Option<Value<'v>>,
        #[starlark(require = named)] progress_message: Option<Value<'v>>,
        #[starlark(require = named)] execution_requirements: Option<Value<'v>>,
        // STUB: accepted, ignored. Real toolchain resolution would
        // route via toolchain= here.
        #[starlark(require = named)] toolchain: Option<Value<'v>>,
        // STUB: accepted, ignored. resource_set is a callback Bazel
        // uses for action-scheduling cost; v1 doesn't schedule.
        #[starlark(require = named)] resource_set: Option<Value<'v>>,
        #[starlark(require = named)] use_default_shell_env: Option<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (toolchain, resource_set);
        let actx = ctx_from_eval(eval)?;
        let env = unpack_str_dict(env);
        let exec_req = unpack_str_dict(execution_requirements);
        // Lower to `run(executable="/bin/bash", …)` exactly as the
        // vendored `wrappers.bzl::actions_run_shell` did (Bazel's
        // `StarlarkActionFactory.runShell`): argv is `[-c, command]`,
        // and `["", *arguments]` is appended only when positional
        // `arguments` are present — the empty `$0` slot Bazel emits to
        // forward args, matching REAPI `Command.arguments` byte-for-byte.
        let user_args = arguments
            .map(|l| flatten_arguments(l.items))
            .transpose()?
            .unwrap_or_default();
        let mut argv = vec!["-c".to_string(), command.to_string()];
        if !user_args.is_empty() {
            argv.push(String::new());
            argv.extend(user_args);
        }
        let mut all_inputs = flatten_files_arg(inputs, eval.heap())?;
        all_inputs.extend(flatten_files_arg(tools, eval.heap())?);
        let mnemonic_str = mnemonic.and_then(|v| v.unpack_str().map(str::to_string));
        let progress_str = progress_message.and_then(|v| v.unpack_str().map(str::to_string));
        let bash = unpack_executable_arg(eval.heap().alloc("/bin/bash"), actx)?;
        actx.actions.borrow_mut().push(Action {
            kind: ActionKind::Run {
                executable: bash,
                arguments: argv,
            },
            mnemonic: mnemonic_str,
            progress_message: progress_str,
            inputs: all_inputs,
            outputs: own_files(outputs),
            env: own_env(env),
            execution_requirements: own_env(exec_req),
            uses_default_shell_env: use_default_shell_env.unwrap_or(false),
        });
        Ok(NoneType)
    }

    /// `ctx.actions.expand_template(template, output, substitutions,
    /// is_executable=False)`. Records the template + substitution
    /// dict. STUB: v1 doesn't actually expand the substitutions at
    /// analysis time; the recorded action carries them so an executor
    /// (or aquery consumer) can.
    fn expand_template<'v>(
        #[starlark(this)] _this: &Actions,
        #[starlark(require = named)] template: &'v File,
        #[starlark(require = named)] output: &'v File,
        // Bazel substitutions are `dict[str, str]`, but in practice
        // rules pass `File`s, `Args`, and toolchain-stub values too —
        // they get stringified at execute time. v1 stringifies eagerly
        // here so toolchain payload misuse doesn't fail analysis. STUB.
        #[starlark(require = named)] substitutions: Option<
            UnpackDictEntries<String, starlark::values::Value<'v>>,
        >,
        #[starlark(require = named, default = false)] is_executable: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let actx = ctx_from_eval(eval)?;
        let subs: BTreeMap<String, String> = substitutions
            .map(|d| {
                d.entries
                    .into_iter()
                    .map(|(k, v)| (k, stringify_arg(v).unwrap_or_default()))
                    .collect()
            })
            .unwrap_or_default();
        // Same File-side propagation as `ctx.actions.write` — the
        // `is_executable=True` kwarg flips the REAPI bit on the
        // expanded output file, not just on the recorded Action.
        let output_file = output.clone().with_is_executable(is_executable);
        actx.actions.borrow_mut().push(Action {
            kind: ActionKind::ExpandTemplate {
                template: template.clone(),
                substitutions: subs,
                is_executable,
            },
            mnemonic: Some("ExpandTemplate".to_string()),
            progress_message: None,
            inputs: vec![template.clone()],
            outputs: vec![output_file],
            env: BTreeMap::new(),
            // `ctx.actions.expand_template` doesn't accept
            // `execution_requirements` in Bazel; empty is correct.
            execution_requirements: BTreeMap::new(),
            uses_default_shell_env: false,
        });
        Ok(NoneType)
    }

    /// `ctx.actions.symlink(output, target_file=None, target_path=None,
    /// target_type=None, …)`. Two mutually-exclusive forms:
    ///   - `target_file` — symlink to a `File` in the action graph;
    ///     recorded with the target as an input so the chain walker
    ///     resolves it.
    ///   - `target_path` — unresolved symlink to an arbitrary path
    ///     string (rules_js's `npm_package_store` links node_modules
    ///     entries this way). No `File` target exists, so we record a
    ///     placeholder self-target; the chain walker treats it as
    ///     opaque (it has cycle detection), which is correct — the
    ///     symlink content isn't an action-graph artifact.
    fn symlink<'v>(
        #[starlark(this)] _this: &Actions,
        #[starlark(require = named)] output: &'v File,
        #[starlark(require = named)] target_file: Option<&'v File>,
        #[starlark(require = named)] target_path: Option<&str>,
        #[starlark(require = named)] target_type: Option<&str>,
        #[starlark(require = named)] is_executable: Option<bool>,
        #[starlark(require = named)] progress_message: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (target_type, is_executable, progress_message);
        let actx = ctx_from_eval(eval)?;
        let (target, inputs) = match (target_file, target_path) {
            (Some(tf), _) => (tf.clone(), vec![tf.clone()]),
            (None, Some(_)) => (output.clone(), Vec::new()),
            (None, None) => {
                return Err(anyhow!(
                    "ctx.actions.symlink: exactly one of `target_file` / `target_path` required"
                ));
            }
        };
        actx.actions.borrow_mut().push(Action {
            kind: ActionKind::Symlink { target },
            mnemonic: Some("Symlink".to_string()),
            progress_message: None,
            inputs,
            outputs: vec![output.clone()],
            env: BTreeMap::new(),
            execution_requirements: BTreeMap::new(),
            uses_default_shell_env: false,
        });
        Ok(NoneType)
    }
}
