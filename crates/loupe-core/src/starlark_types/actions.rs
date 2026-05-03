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

use std::collections::BTreeMap;

use allocative::Allocative;
use anyhow::anyhow;
use starlark::any::ProvidesStaticType;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::dict::DictRef;
use starlark::values::list::ListRef;
use starlark::values::none::NoneType;
use starlark::values::{NoSerialize, StarlarkValue, Value, starlark_value};

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

fn ctx_from_eval<'a, 'v>(eval: &'a Evaluator<'v, '_, '_>) -> anyhow::Result<&'a AnalysisContext> {
    eval.extra
        .ok_or_else(|| anyhow!("internal: ctx.actions called outside an AnalysisContext"))?
        .downcast_ref::<AnalysisContext>()
        .ok_or_else(|| anyhow!("internal: Evaluator::extra is not an AnalysisContext"))
}

fn unpack_file<'v>(v: Value<'v>, ctx_label: &str) -> anyhow::Result<File> {
    File::from_value(v)
        .cloned()
        .ok_or_else(|| anyhow!("{ctx_label}: expected a File, got {}", v.get_type()))
}

fn unpack_file_list<'v>(v: Option<Value<'v>>, ctx_label: &str) -> anyhow::Result<Vec<File>> {
    let v = match v {
        Some(v) if !v.is_none() => v,
        _ => return Ok(Vec::new()),
    };
    let list = ListRef::from_value(v)
        .ok_or_else(|| anyhow!("{ctx_label}: expected a list of File, got {}", v.get_type()))?;
    list.iter()
        .map(|item| unpack_file(item, ctx_label))
        .collect()
}

fn unpack_string_list<'v>(v: Option<Value<'v>>, ctx_label: &str) -> anyhow::Result<Vec<String>> {
    let v = match v {
        Some(v) if !v.is_none() => v,
        _ => return Ok(Vec::new()),
    };
    let list = ListRef::from_value(v).ok_or_else(|| {
        anyhow!(
            "{ctx_label}: expected a list of strings, got {}",
            v.get_type()
        )
    })?;
    list.iter()
        .map(|item| {
            item.unpack_str()
                .map(|s| s.to_string())
                .ok_or_else(|| anyhow!("{ctx_label}: list entries must be strings"))
        })
        .collect()
}

fn unpack_env<'v>(
    v: Option<Value<'v>>,
    ctx_label: &str,
) -> anyhow::Result<BTreeMap<String, String>> {
    let v = match v {
        Some(v) if !v.is_none() => v,
        _ => return Ok(BTreeMap::new()),
    };
    let dict = DictRef::from_value(v).ok_or_else(|| {
        anyhow!(
            "{ctx_label}: env expects a dict[str, str], got {}",
            v.get_type()
        )
    })?;
    let mut out = BTreeMap::new();
    for (k, val) in dict.iter() {
        let key = k
            .unpack_str()
            .ok_or_else(|| anyhow!("{ctx_label}: env keys must be strings"))?
            .to_string();
        let value = val
            .unpack_str()
            .ok_or_else(|| anyhow!("{ctx_label}: env values must be strings"))?
            .to_string();
        out.insert(key, value);
    }
    Ok(out)
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
        #[starlark(require = named, default = NoneType)] sibling: Value<'v>,
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
        #[starlark(require = pos)] output: Value<'v>,
        #[starlark(require = pos)] content: &str,
        #[starlark(require = named, default = false)] is_executable: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let actx = ctx_from_eval(eval)?;
        let output_file = unpack_file(output, "ctx.actions.write(output=…)")?;
        actx.actions.borrow_mut().push(Action {
            kind: ActionKind::Write {
                content: content.to_string(),
                is_executable,
            },
            mnemonic: Some("FileWrite".to_string()),
            progress_message: None,
            inputs: Vec::new(),
            outputs: vec![output_file],
            env: BTreeMap::new(),
        });
        Ok(NoneType)
    }

    /// `ctx.actions.run(executable, arguments=[], inputs=[],
    /// outputs=[], env={}, mnemonic=None, progress_message=None)`.
    #[allow(clippy::too_many_arguments)] // matches Bazel's surface
    fn run<'v>(
        #[starlark(this)] _this: &Actions,
        #[starlark(require = named)] executable: Value<'v>,
        #[starlark(require = named)] arguments: Option<Value<'v>>,
        #[starlark(require = named)] inputs: Option<Value<'v>>,
        #[starlark(require = named)] outputs: Option<Value<'v>>,
        #[starlark(require = named)] env: Option<Value<'v>>,
        #[starlark(require = named)] mnemonic: Option<&str>,
        #[starlark(require = named)] progress_message: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let actx = ctx_from_eval(eval)?;
        let executable = unpack_file(executable, "ctx.actions.run(executable=…)")?;
        let arguments = unpack_string_list(arguments, "ctx.actions.run(arguments=…)")?;
        let inputs = unpack_file_list(inputs, "ctx.actions.run(inputs=…)")?;
        let outputs = unpack_file_list(outputs, "ctx.actions.run(outputs=…)")?;
        let env = unpack_env(env, "ctx.actions.run(env=…)")?;
        actx.actions.borrow_mut().push(Action {
            kind: ActionKind::Run {
                executable,
                arguments,
            },
            mnemonic: mnemonic.map(str::to_string),
            progress_message: progress_message.map(str::to_string),
            inputs,
            outputs,
            env,
        });
        Ok(NoneType)
    }

    /// `ctx.actions.run_shell(command, arguments=[], inputs=[],
    /// outputs=[], env={}, mnemonic=None, progress_message=None)`.
    #[allow(clippy::too_many_arguments)] // matches Bazel's surface
    fn run_shell<'v>(
        #[starlark(this)] _this: &Actions,
        #[starlark(require = named)] command: &str,
        #[starlark(require = named)] arguments: Option<Value<'v>>,
        #[starlark(require = named)] inputs: Option<Value<'v>>,
        #[starlark(require = named)] outputs: Option<Value<'v>>,
        #[starlark(require = named)] env: Option<Value<'v>>,
        #[starlark(require = named)] mnemonic: Option<&str>,
        #[starlark(require = named)] progress_message: Option<&str>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let actx = ctx_from_eval(eval)?;
        let arguments = unpack_string_list(arguments, "ctx.actions.run_shell(arguments=…)")?;
        let inputs = unpack_file_list(inputs, "ctx.actions.run_shell(inputs=…)")?;
        let outputs = unpack_file_list(outputs, "ctx.actions.run_shell(outputs=…)")?;
        let env = unpack_env(env, "ctx.actions.run_shell(env=…)")?;
        actx.actions.borrow_mut().push(Action {
            kind: ActionKind::RunShell {
                command: command.to_string(),
                arguments,
            },
            mnemonic: mnemonic.map(str::to_string),
            progress_message: progress_message.map(str::to_string),
            inputs,
            outputs,
            env,
        });
        Ok(NoneType)
    }

    /// `ctx.actions.symlink(output, target_file)`. Bazel also takes
    /// `target_path` for symlinks pointing outside the action graph;
    /// v1 only supports the target_file form.
    fn symlink<'v>(
        #[starlark(this)] _this: &Actions,
        #[starlark(require = named)] output: Value<'v>,
        #[starlark(require = named)] target_file: Value<'v>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let actx = ctx_from_eval(eval)?;
        let output_file = unpack_file(output, "ctx.actions.symlink(output=…)")?;
        let target = unpack_file(target_file, "ctx.actions.symlink(target_file=…)")?;
        actx.actions.borrow_mut().push(Action {
            kind: ActionKind::Symlink {
                target: target.clone(),
            },
            mnemonic: Some("Symlink".to_string()),
            progress_message: None,
            inputs: vec![target],
            outputs: vec![output_file],
            env: BTreeMap::new(),
        });
        Ok(NoneType)
    }
}
