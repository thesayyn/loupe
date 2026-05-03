//! DICE keys and the workspace data they read from.
//!
//! Phase 3: `BzlLoadKey` is real — its compute fn parses the target
//! `.bzl`, recursively forces `BzlLoadKey` for every transitive `load`,
//! and returns a `FrozenModule`. `PackageKey` does the same for the
//! BUILD file: forces `BzlLoadKey` for each load statement, then runs
//! the synchronous BUILD evaluator with the resolved modules in hand.
//! `ModuleGraphKey` parses `MODULE.bazel`; results plus the v1 repo
//! mapping flow into label resolution at every load.
//!
//! Errors propagate as `SharedResult<T> = Ok(Arc<T>) | Err(Arc<String>)`
//! — `anyhow::Error` doesn't implement `Allocative`, and
//! `SharedResult<T>` is the smallest shape that satisfies DICE's
//! `Value: Allocative + Dupe + Send + Sync + 'static` bound while
//! still letting the engine surface stay `anyhow::Result<T>` shaped.

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
use starlark::environment::FrozenModule;

use crate::build_eval;
use crate::bzl_eval;
use crate::label::{Label, PackageId};
use crate::module_graph::{self, ModuleGraph};
use crate::package::Package;

/// `Ok(Arc<T>)` or a stringified error. Both arms are cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub enum SharedResult<T: Allocative + 'static> {
    Ok(Arc<T>),
    Err(Arc<String>),
}

impl<T: Allocative + Clone + 'static> Dupe for SharedResult<T> {}

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

// ---- BzlLoadKey ------------------------------------------------------

/// What `BzlLoadKey` returns. Carries an `Arc<FrozenModule>` so DICE's
/// `Dupe` bound is cheap; equality is pointer identity (a recompute
/// produces a fresh frozen heap, so downstream invalidates).
#[derive(Debug, Clone, Allocative)]
pub struct LoadedBzl {
    pub module: Arc<FrozenModule>,
}

impl PartialEq for LoadedBzl {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.module, &other.module)
    }
}
impl Eq for LoadedBzl {}
impl Dupe for LoadedBzl {}

#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("BzlLoadKey({_0})")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct BzlLoadKey(pub Label);

#[async_trait]
impl Key for BzlLoadKey {
    type Value = SharedResult<LoadedBzl>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        let graph = match force_module_graph(ctx).await {
            Ok(g) => g,
            Err(e) => return SharedResult::err(e),
        };
        match compute_bzl_load(ctx, &self.0, graph.as_ref()).await {
            Ok(lb) => SharedResult::Ok(Arc::new(lb)),
            Err(e) => SharedResult::err(e),
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
        let graph = match force_module_graph(ctx).await {
            Ok(g) => g,
            Err(e) => return SharedResult::err(e),
        };
        match compute_package(ctx, &self.0, graph.as_ref()).await {
            Ok(pkg) => SharedResult::ok(pkg),
            Err(e) => SharedResult::err(e),
        }
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        x == y
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

// ---- Helpers ---------------------------------------------------------

async fn force_module_graph(ctx: &mut DiceComputations<'_>) -> Result<Arc<ModuleGraph>, String> {
    match ctx.compute(&ModuleGraphKey).await {
        Ok(SharedResult::Ok(g)) => Ok(g),
        Ok(SharedResult::Err(e)) => Err((*e).clone()),
        Err(e) => Err(format!("ModuleGraphKey: {e}")),
    }
}

async fn force_bzl_load(
    ctx: &mut DiceComputations<'_>,
    label: Label,
) -> Result<Arc<LoadedBzl>, String> {
    match ctx.compute(&BzlLoadKey(label)).await {
        Ok(SharedResult::Ok(v)) => Ok(v),
        Ok(SharedResult::Err(e)) => Err((*e).clone()),
        Err(e) => Err(format!("BzlLoadKey: {e}")),
    }
}

async fn compute_bzl_load(
    ctx: &mut DiceComputations<'_>,
    label: &Label,
    graph: &ModuleGraph,
) -> Result<LoadedBzl, String> {
    let root = ctx.global_data().workspace_root().to_path_buf();
    let bzl_path = bzl_disk_path(&root, label)?;
    let ast = bzl_eval::parse(&bzl_path).map_err(|e| format!("{e:#}"))?;
    let load_stmts = build_eval::extract_loads(&ast);
    let mut loaded: std::collections::HashMap<String, FrozenModule> =
        std::collections::HashMap::new();
    for stmt in &load_stmts {
        let target =
            resolve_load(&stmt.module_id, &label.package, graph).map_err(|e| format!("{e:#}"))?;
        let lb = Box::pin(force_bzl_load(ctx, target)).await?;
        loaded.insert(stmt.module_id.clone(), (*lb.module).clone());
    }
    let frozen = bzl_eval::evaluate(ast, &loaded).map_err(|e| format!("{e:#}"))?;
    Ok(LoadedBzl {
        module: Arc::new(frozen),
    })
}

async fn compute_package(
    ctx: &mut DiceComputations<'_>,
    pkg: &PackageId,
    graph: &ModuleGraph,
) -> Result<Package, String> {
    let root = ctx.global_data().workspace_root().to_path_buf();
    let (_, ast) = build_eval::parse_build(&root, pkg).map_err(|e| format!("{e:#}"))?;
    let load_stmts = build_eval::extract_loads(&ast);
    let mut loaded: std::collections::HashMap<String, FrozenModule> =
        std::collections::HashMap::new();
    for stmt in &load_stmts {
        let target = resolve_load(&stmt.module_id, pkg, graph).map_err(|e| format!("{e:#}"))?;
        let lb = force_bzl_load(ctx, target).await?;
        loaded.insert(stmt.module_id.clone(), (*lb.module).clone());
    }
    build_eval::evaluate(&root, pkg.clone(), ast, &load_stmts, &loaded)
        .map_err(|e| format!("{e:#}"))
}

fn bzl_disk_path(workspace_root: &Path, label: &Label) -> Result<PathBuf, String> {
    if !label.repo().is_main() {
        return Err(format!(
            "v1 only loads `.bzl`s from the main repo (got `{}`)",
            label
        ));
    }
    if !label.name().ends_with(".bzl") {
        return Err(format!(
            "BzlLoadKey label must end in .bzl (got `{}`)",
            label
        ));
    }
    let mut path = workspace_root.to_path_buf();
    if !label.package_path().is_root() {
        path.push(label.package_path().as_str());
    }
    path.push(label.name());
    Ok(path)
}

fn resolve_load(
    load_str: &str,
    current_pkg: &PackageId,
    graph: &ModuleGraph,
) -> anyhow::Result<Label> {
    Label::resolve(load_str, current_pkg, &graph.repo_mapping)
        .map_err(|e| anyhow::anyhow!("resolving load `{load_str}` from {current_pkg}: {e}"))
}
