//! DICE keys and the workspace data they read from.
//!
//! Phase 2 wiring: three keys, all real (no path-only stubs in the
//! compute fns), but `BzlLoadKey` is degenerate — it returns an empty
//! payload because the BUILD evaluator builds its own per-load stub
//! `FrozenModule`s inline. Phase 3 makes `BzlLoadKey` return a real
//! frozen module evaluated from `.bzl` source.
//!
//! All compute fns are deterministic. Errors propagate as
//! `Result<Arc<T>, Arc<String>>` rather than panics so the engine
//! surface stays Result-shaped.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use allocative::Allocative;
use async_trait::async_trait;
use dice::{
    CancellationContext, DiceComputations, DiceData, DiceDataBuilder, Key, NoValueSerialize,
    ValueSerialize,
};
use dupe::Dupe;
use pagable::{Pagable, pagable_typetag};

use crate::build_eval;
use crate::label::{Label, PackageId};
use crate::module_graph::{self, ModuleGraph};
use crate::package::Package;

/// Either an `Arc<T>` or a stringified error. Both arms are cheap to
/// clone, so the value satisfies DICE's `Dupe` bound without copying
/// the underlying data.
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub enum SharedResult<T: Allocative + 'static> {
    Ok(Arc<T>),
    Err(Arc<String>),
}

impl<T: Allocative + 'static> Dupe for SharedResult<T> where T: Clone {}

impl<T: Allocative + 'static> SharedResult<T> {
    pub fn err(msg: impl Into<String>) -> Self {
        SharedResult::Err(Arc::new(msg.into()))
    }
    pub fn ok(value: T) -> Self {
        SharedResult::Ok(Arc::new(value))
    }
    pub fn into_anyhow(self) -> anyhow::Result<Arc<T>> {
        match self {
            SharedResult::Ok(v) => Ok(v),
            SharedResult::Err(e) => Err(anyhow::anyhow!("{e}")),
        }
    }
}

// ---- Workspace root injected into DiceData ---------------------------

/// Workspace-root marker held in `DiceData` so compute fns can read
/// disk. Path is canonicalised at engine-construction time.
#[derive(Debug, Clone, Allocative)]
pub struct WorkspaceRoot(PathBuf);

impl WorkspaceRoot {
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

pub trait HasWorkspaceRoot {
    fn workspace_root(&self) -> &Path;
}

impl HasWorkspaceRoot for DiceData {
    fn workspace_root(&self) -> &Path {
        self.get::<WorkspaceRoot>()
            .expect("WorkspaceRoot was not set on DiceData")
            .as_path()
    }
}

pub trait SetWorkspaceRoot {
    fn set_workspace_root(&mut self, root: PathBuf);
}

impl SetWorkspaceRoot for DiceDataBuilder {
    fn set_workspace_root(&mut self, root: PathBuf) {
        self.set(WorkspaceRoot(root));
    }
}

// ---- ModuleGraphKey --------------------------------------------------

#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("ModuleGraphKey")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct ModuleGraphKey;

#[async_trait]
impl Key for ModuleGraphKey {
    type Value = SharedResult<ModuleGraph>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        let root = ctx.global_data().workspace_root().to_path_buf();
        match module_graph::parse(&root) {
            Ok(g) => SharedResult::ok(g),
            Err(e) => SharedResult::err(format!("{e:#}")),
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

// ---- PackageKey ------------------------------------------------------

#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("PackageKey({_0})")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct PackageKey(pub PackageId);

#[async_trait]
impl Key for PackageKey {
    type Value = SharedResult<Package>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        // Force MODULE.bazel parsing to record the dep edge — even
        // though Phase 2's BUILD evaluator doesn't yet consult the
        // module graph, future phases will.
        match ctx.compute(&ModuleGraphKey).await {
            Ok(SharedResult::Err(e)) => return SharedResult::Err(e),
            Err(e) => return SharedResult::err(format!("ModuleGraphKey failed: {e}")),
            Ok(SharedResult::Ok(_)) => {}
        }
        let root = ctx.global_data().workspace_root().to_path_buf();
        match build_eval::evaluate(&root, self.0.clone()) {
            Ok(pkg) => SharedResult::ok(pkg),
            Err(e) => SharedResult::err(format!("{e:#}")),
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

// ---- BzlLoadKey ------------------------------------------------------

/// STUB: in Phase 2 the BUILD evaluator builds its own per-load
/// `FrozenModule` stubs inline — `BzlLoadKey` exists only to plumb the
/// dimension. Phase 3 returns a real loaded module here, after which
/// `build_eval::evaluate` will call `ctx.compute(&BzlLoadKey(...))`
/// for each load statement.
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BzlLoadKey({_0})")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct BzlLoadKey(pub Label);

#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub struct LoadedModuleStub;

impl Dupe for LoadedModuleStub {}

#[async_trait]
impl Key for BzlLoadKey {
    type Value = SharedResult<LoadedModuleStub>;

    async fn compute(&self, _ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        // STUB: returns an empty marker. Phase 3 will read
        // `self.0` from disk and produce a real `FrozenModule`.
        SharedResult::ok(LoadedModuleStub)
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}
