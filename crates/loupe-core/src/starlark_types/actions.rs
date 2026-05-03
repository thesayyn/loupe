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
use std::sync::Arc;

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
use starlark::values::{Heap, NoSerialize, StarlarkValue, Value, starlark_value};

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

/// Starlark view of one recorded [`Action`]. What `target.actions[i]`
/// returns. Read-only — `ctx.actions.*` is the only way to create one.
///
/// Bazel surface: `mnemonic`, `inputs`, `outputs`, `argv`, `env`, plus
/// `content` (Write only). v1 surfaces the same names; `inputs` and
/// `outputs` are flat lists, **not** `depset(File)` — this is a STUB,
/// good enough for ruleset code that iterates and bad for any code
/// that introspects the type.
#[derive(Debug, Clone, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("<action {}>", action.mnemonic.as_deref().unwrap_or("?"))]
pub struct ActionValue {
    pub action: Arc<Action>,
}

impl PartialEq for ActionValue {
    fn eq(&self, other: &Self) -> bool {
        *self.action == *other.action
    }
}
impl Eq for ActionValue {}

starlark_simple_value!(ActionValue);

#[starlark_value(type = "Action")]
impl<'v> StarlarkValue<'v> for ActionValue {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "mnemonic" => Some(heap.alloc(self.action.mnemonic.as_deref().unwrap_or(""))),
            // STUB: should be `depset(File)`. Flat list works for the
            // common `for f in act.inputs:` pattern; introspection
            // (`type(act.inputs)`) sees the difference.
            "inputs" => {
                let xs: Vec<Value<'v>> = self
                    .action
                    .inputs
                    .iter()
                    .map(|f| heap.alloc(f.clone()))
                    .collect();
                Some(heap.alloc(xs))
            }
            "outputs" => {
                let xs: Vec<Value<'v>> = self
                    .action
                    .outputs
                    .iter()
                    .map(|f| heap.alloc(f.clone()))
                    .collect();
                Some(heap.alloc(xs))
            }
            "argv" => {
                let argv: Vec<&str> = match &self.action.kind {
                    ActionKind::Run { arguments, .. } | ActionKind::RunShell { arguments, .. } => {
                        arguments.iter().map(|s| s.as_str()).collect()
                    }
                    ActionKind::Write { .. } | ActionKind::Symlink { .. } => Vec::new(),
                };
                Some(heap.alloc(argv))
            }
            "env" => {
                let entries: SmallMap<&str, &str> = self
                    .action
                    .env
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                Some(heap.alloc(AllocDict(entries)))
            }
            "content" => match &self.action.kind {
                ActionKind::Write { content, .. } => Some(heap.alloc(content.as_str())),
                _ => None,
            },
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        let mut v = vec!["mnemonic", "inputs", "outputs", "argv", "env"];
        if matches!(self.action.kind, ActionKind::Write { .. }) {
            v.push("content");
        }
        v.iter().map(|s| s.to_string()).collect()
    }
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

fn own_strings(items: Option<UnpackList<String>>) -> Vec<String> {
    items.map(|l| l.items).unwrap_or_default()
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
    /// outputs=[], env={}, mnemonic=None, progress_message=None)`.
    #[allow(clippy::too_many_arguments)] // matches Bazel's surface
    fn run<'v>(
        #[starlark(this)] _this: &Actions,
        #[starlark(require = named)] executable: &'v File,
        #[starlark(require = named)] arguments: Option<UnpackList<String>>,
        #[starlark(require = named)] inputs: Option<UnpackList<&'v File>>,
        #[starlark(require = named)] outputs: Option<UnpackList<&'v File>>,
        #[starlark(require = named)] env: Option<UnpackDictEntries<String, String>>,
        #[starlark(require = named)] mnemonic: Option<&str>,
        #[starlark(require = named)] progress_message: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let actx = ctx_from_eval(eval)?;
        actx.actions.borrow_mut().push(Action {
            kind: ActionKind::Run {
                executable: executable.clone(),
                arguments: own_strings(arguments),
            },
            mnemonic: mnemonic.map(str::to_string),
            progress_message: progress_message.map(str::to_string),
            inputs: own_files(inputs),
            outputs: own_files(outputs),
            env: own_env(env),
        });
        Ok(NoneType)
    }

    /// `ctx.actions.run_shell(command, arguments=[], inputs=[],
    /// outputs=[], env={}, mnemonic=None, progress_message=None)`.
    #[allow(clippy::too_many_arguments)] // matches Bazel's surface
    fn run_shell<'v>(
        #[starlark(this)] _this: &Actions,
        #[starlark(require = named)] command: &str,
        #[starlark(require = named)] arguments: Option<UnpackList<String>>,
        #[starlark(require = named)] inputs: Option<UnpackList<&'v File>>,
        #[starlark(require = named)] outputs: Option<UnpackList<&'v File>>,
        #[starlark(require = named)] env: Option<UnpackDictEntries<String, String>>,
        #[starlark(require = named)] mnemonic: Option<&str>,
        #[starlark(require = named)] progress_message: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let actx = ctx_from_eval(eval)?;
        actx.actions.borrow_mut().push(Action {
            kind: ActionKind::RunShell {
                command: command.to_string(),
                arguments: own_strings(arguments),
            },
            mnemonic: mnemonic.map(str::to_string),
            progress_message: progress_message.map(str::to_string),
            inputs: own_files(inputs),
            outputs: own_files(outputs),
            env: own_env(env),
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
