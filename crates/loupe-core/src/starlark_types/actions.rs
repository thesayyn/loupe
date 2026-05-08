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
use starlark::values::{Heap, NoSerialize, StarlarkValue, Value, ValueLike, starlark_value};

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
            "content" => match &self.kind {
                ActionKind::Write { content, .. } => Some(heap.alloc(content.as_str())),
                _ => None,
            },
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        let mut v = vec!["mnemonic", "inputs", "outputs", "argv", "env"];
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
    fn add_all<'v>(
        #[starlark(this)] this: &Args,
        #[starlark(require = pos)] values: UnpackList<Value<'v>>,
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
        for v in values.items {
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
    fn add_joined<'v>(
        #[starlark(this)] this: &Args,
        #[starlark(require = pos)] values: UnpackList<Value<'v>>,
        #[starlark(require = named)] join_with: &str,
        #[starlark(require = named)] format_each: Option<&str>,
    ) -> anyhow::Result<NoneType> {
        let mut formatted: Vec<String> = Vec::with_capacity(values.items.len());
        for v in values.items {
            let s = stringify_arg(v)?;
            formatted.push(match format_each {
                Some(fmt) => fmt.replace("%s", &s),
                None => s,
            });
        }
        this.push(formatted.join(join_with));
        Ok(NoneType)
    }

    /// `Args.use_param_file(format, use_always=False)`. STUB —
    /// recorded but ignored; v1 doesn't generate param files.
    fn use_param_file<'v>(
        #[starlark(this)] _this: &Args,
        #[starlark(require = pos)] _format: &str,
        #[starlark(require = named, default = false)] _use_always: bool,
    ) -> anyhow::Result<NoneType> {
        Ok(NoneType)
    }

    /// `Args.set_param_file_format(format)`. STUB.
    fn set_param_file_format<'v>(
        #[starlark(this)] _this: &Args,
        #[starlark(require = pos)] _format: &str,
    ) -> anyhow::Result<NoneType> {
        Ok(NoneType)
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

fn own_env(env: Option<UnpackDictEntries<String, String>>) -> BTreeMap<String, String> {
    env.map(|d| d.entries.into_iter().collect())
        .unwrap_or_default()
}

#[starlark_module]
fn actions_methods(builder: &mut MethodsBuilder) {
    /// `ctx.actions.declare_file(name, sibling=None)` → `File`.
    /// Allocates a generated-bin `File` rooted in the analyzed
    /// target's package. `sibling` (Bazel: place the new file next
    /// to an existing one) is accepted but only its package is
    /// honored in v1 — full path-rebasing is a follow-up.
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
            actx.configuration.segment().to_string(),
        );
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

    /// `ctx.actions.declare_directory(name, sibling=None)` → `File`.
    /// Tree artifact — a generated path Bazel treats as a directory at
    /// runtime. Loupe records the declaration; we don't model the
    /// contents (STUB). Consumers see `File.is_directory == True`.
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
            actx.configuration.segment().to_string(),
        );
        actx.declared_files
            .borrow_mut()
            .insert(file.short_path(), file.clone());
        Ok(file)
    }

    /// `ctx.actions.write(output, content, is_executable=False)`.
    /// Records a [`Write`](ActionKind::Write) action.
    fn write<'v>(
        #[starlark(this)] _this: &Actions,
        #[starlark(require = pos)] output: &'v File,
        #[starlark(require = pos)] content: &str,
        #[starlark(require = named, default = false)] is_executable: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let actx = ctx_from_eval(eval)?;
        actx.actions.borrow_mut().push(Action {
            kind: ActionKind::Write {
                content: content.to_string(),
                is_executable,
            },
            mnemonic: Some("FileWrite".to_string()),
            progress_message: None,
            inputs: Vec::new(),
            outputs: vec![output.clone()],
            env: BTreeMap::new(),
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
    /// into `inputs`). `execution_requirements` is recorded but
    /// otherwise ignored (STUB).
    #[allow(clippy::too_many_arguments)] // matches Bazel's surface
    fn run<'v>(
        #[starlark(this)] _this: &Actions,
        #[starlark(require = named)] executable: &'v File,
        #[starlark(require = named)] arguments: Option<UnpackList<Value<'v>>>,
        // Bazel accepts both list[File] and depset(File) for these.
        // v1 unpacks both via `flatten_files_arg` (STUB-flat).
        #[starlark(require = named)] inputs: Option<Value<'v>>,
        #[starlark(require = named)] outputs: Option<UnpackList<&'v File>>,
        #[starlark(require = named)] tools: Option<Value<'v>>,
        #[starlark(require = named)] env: Option<UnpackDictEntries<String, String>>,
        #[starlark(require = named)] mnemonic: Option<&str>,
        #[starlark(require = named)] progress_message: Option<&str>,
        #[starlark(require = named)] execution_requirements: Option<
            UnpackDictEntries<String, String>,
        >,
        // STUB: accepted, ignored. Real toolchain resolution would
        // route via toolchain= here.
        #[starlark(require = named)] toolchain: Option<Value<'v>>,
        // STUB: accepted, ignored. resource_set is a callback Bazel
        // uses for action-scheduling cost; v1 doesn't schedule.
        #[starlark(require = named)] resource_set: Option<Value<'v>>,
        #[starlark(require = named)] use_default_shell_env: Option<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (toolchain, resource_set, use_default_shell_env);
        let actx = ctx_from_eval(eval)?;
        let _ = execution_requirements; // STUB: accepted, not surfaced.
        let argv = arguments
            .map(|l| flatten_arguments(l.items))
            .transpose()?
            .unwrap_or_default();
        let mut all_inputs = flatten_files_arg(inputs, eval.heap())?;
        all_inputs.extend(flatten_files_arg(tools, eval.heap())?);
        actx.actions.borrow_mut().push(Action {
            kind: ActionKind::Run {
                executable: executable.clone(),
                arguments: argv,
            },
            mnemonic: mnemonic.map(str::to_string),
            progress_message: progress_message.map(str::to_string),
            inputs: all_inputs,
            outputs: own_files(outputs),
            env: own_env(env),
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
        #[starlark(require = named)] env: Option<UnpackDictEntries<String, String>>,
        #[starlark(require = named)] mnemonic: Option<&str>,
        #[starlark(require = named)] progress_message: Option<&str>,
        #[starlark(require = named)] execution_requirements: Option<
            UnpackDictEntries<String, String>,
        >,
        // STUB: accepted, ignored. Real toolchain resolution would
        // route via toolchain= here.
        #[starlark(require = named)] toolchain: Option<Value<'v>>,
        // STUB: accepted, ignored. resource_set is a callback Bazel
        // uses for action-scheduling cost; v1 doesn't schedule.
        #[starlark(require = named)] resource_set: Option<Value<'v>>,
        #[starlark(require = named)] use_default_shell_env: Option<bool>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (toolchain, resource_set, use_default_shell_env);
        let actx = ctx_from_eval(eval)?;
        let _ = execution_requirements;
        let argv = arguments
            .map(|l| flatten_arguments(l.items))
            .transpose()?
            .unwrap_or_default();
        let mut all_inputs = flatten_files_arg(inputs, eval.heap())?;
        all_inputs.extend(flatten_files_arg(tools, eval.heap())?);
        actx.actions.borrow_mut().push(Action {
            kind: ActionKind::RunShell {
                command: command.to_string(),
                arguments: argv,
            },
            mnemonic: mnemonic.map(str::to_string),
            progress_message: progress_message.map(str::to_string),
            inputs: all_inputs,
            outputs: own_files(outputs),
            env: own_env(env),
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
        actx.actions.borrow_mut().push(Action {
            kind: ActionKind::ExpandTemplate {
                template: template.clone(),
                substitutions: subs,
                is_executable,
            },
            mnemonic: Some("ExpandTemplate".to_string()),
            progress_message: None,
            inputs: vec![template.clone()],
            outputs: vec![output.clone()],
            env: BTreeMap::new(),
        });
        Ok(NoneType)
    }

    /// `ctx.actions.symlink(output, target_file)`. Bazel also takes
    /// `target_path` for symlinks pointing outside the action graph;
    /// v1 only supports the target_file form.
    fn symlink<'v>(
        #[starlark(this)] _this: &Actions,
        #[starlark(require = named)] output: &'v File,
        #[starlark(require = named)] target_file: &'v File,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let actx = ctx_from_eval(eval)?;
        actx.actions.borrow_mut().push(Action {
            kind: ActionKind::Symlink {
                target: target_file.clone(),
            },
            mnemonic: Some("Symlink".to_string()),
            progress_message: None,
            inputs: vec![target_file.clone()],
            outputs: vec![output.clone()],
            env: BTreeMap::new(),
        });
        Ok(NoneType)
    }
}
