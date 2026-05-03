//! Public engine API.
//!
//! Phase 2: only `list_package` is implemented. `analyze` and
//! `analyze_transitive` arrive in Phase 5. The `Configuration` field
//! is plumbed through even though v1 has a single config — DESIGN.md
//! mandates plumbing every dimension that future phases will need.
//!
//! Internally, queries route through DICE: `list_package` blocks on a
//! `PackageKey` compute, which transitively forces `ModuleGraphKey`.
//! `BzlLoadKey` is degenerate in Phase 2 (the BUILD evaluator stubs
//! its own loaded modules) but is a real DICE key so Phase 3 can swap
//! its compute fn without touching Engine.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use dice::{Dice, DiceTransaction};
use tokio::runtime::Runtime;
use tracing::instrument;

use crate::analysis::{AnalyzedTarget, Configuration};
use crate::dice_keys::{AnalysisKey, ModuleGraphKey, PackageKey, SetWorkspaceRoot};
use crate::label::{Label, PackageId};
use crate::module_graph::ModuleGraph;
use crate::package::Package;
use crate::workspace;

/// Top-level analysis engine. See `DESIGN.md` for the public surface.
pub struct Engine {
    workspace_root: PathBuf,
    dice: Arc<Dice>,
    runtime: Runtime,
    #[allow(dead_code)]
    config: Configuration,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("workspace_root", &self.workspace_root)
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Construct an engine rooted at `workspace_root`. Eagerly forces a
    /// `ModuleGraphKey` compute so a malformed workspace fails fast
    /// rather than at first query time.
    #[instrument(skip(config))]
    pub fn new(workspace_root: PathBuf, config: Configuration) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let mut builder = Dice::builder();
        builder.set_workspace_root(workspace_root.clone());
        let dice = builder.build(dice::DetectCycles::Disabled);
        let engine = Engine {
            workspace_root,
            dice,
            runtime,
            config,
        };
        // Force MODULE.bazel parse now to surface errors at construction.
        engine.module_graph()?;
        Ok(engine)
    }

    pub fn workspace_root(&self) -> &std::path::Path {
        &self.workspace_root
    }

    /// Force `ModuleGraphKey` and return the parsed graph.
    #[instrument(skip(self))]
    pub fn module_graph(&self) -> Result<Arc<ModuleGraph>> {
        let dice = Arc::clone(&self.dice);
        self.runtime.block_on(async move {
            let mut txn = open_txn(&dice).await;
            let v = txn
                .compute(&ModuleGraphKey)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            v.into_anyhow()
        })
    }

    /// Force `PackageKey` and return the targets in stable
    /// (sorted-by-name) order — the Phase 2 deliverable.
    #[instrument(skip(self), fields(workspace = %self.workspace_root.display()))]
    pub fn list_package(&self, package: &PackageId) -> Result<Vec<Label>> {
        Ok(self.query_package(package)?.labels())
    }

    /// Force `PackageKey` and return the full evaluated `Package`
    /// (target shapes, not just labels). Phase 3 callers use this to
    /// inspect rule kinds and recorded attrs.
    #[instrument(skip(self), fields(workspace = %self.workspace_root.display()))]
    pub fn query_package(&self, package: &PackageId) -> Result<Arc<Package>> {
        let dice = Arc::clone(&self.dice);
        let key = PackageKey(package.clone());
        self.runtime.block_on(async move {
            let mut txn = open_txn(&dice).await;
            let v = txn
                .compute(&key)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            v.into_anyhow()
        })
    }

    /// All packages in the workspace, sorted. (Filesystem walk — not
    /// itself a DICE compute in v1; Phase 6+ will key it.)
    #[instrument(skip(self))]
    pub fn discover_packages(&self) -> Result<Vec<PackageId>> {
        workspace::discover_packages(&self.workspace_root)
    }

    /// Configured analysis of one target. Forces `AnalysisKey` —
    /// transitively forcing `PackageKey`, `BzlLoadKey`, and (Phase
    /// 5+) any label-dep `AnalysisKey`s — and returns the
    /// `AnalyzedTarget` carrying captured providers.
    #[instrument(skip(self))]
    pub fn analyze_target(&self, label: &Label) -> Result<Arc<AnalyzedTarget>> {
        let dice = Arc::clone(&self.dice);
        let key = AnalysisKey(label.clone(), self.config.clone());
        self.runtime.block_on(async move {
            let mut txn = open_txn(&dice).await;
            let v = txn
                .compute(&key)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            v.into_anyhow()
        })
    }

    /// Bazel-style `cquery <label>` default output: one line per
    /// resolved label in canonical form. Matches what
    /// `bazel cquery //:world` prints by default.
    #[instrument(skip(self))]
    pub fn cquery(&self, label: &Label) -> Result<String> {
        let analyzed = self.analyze_target(label)?;
        Ok(analyzed.label.to_string())
    }
}

async fn open_txn(dice: &Arc<Dice>) -> DiceTransaction {
    dice.updater().commit().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::label::{CanonicalRepo, PackagePath};
    use std::fs;

    fn write_workspace(dir: &std::path::Path) {
        fs::write(
            dir.join("MODULE.bazel"),
            r#"module(name = "demo", version = "0.0.0")"#,
        )
        .unwrap();
        fs::write(
            dir.join("defs.bzl"),
            r#"
def _hello_impl(ctx):
    return []

hello = rule(
    implementation = _hello_impl,
    attrs = {"greeting": attr.string(default = "hello")},
)
"#,
        )
        .unwrap();
        fs::write(
            dir.join("BUILD.bazel"),
            r#"
load(":defs.bzl", "hello")
hello(name = "world", greeting = "hi")
hello(name = "alpha")
"#,
        )
        .unwrap();
    }

    #[test]
    fn list_package_returns_targets_sorted() {
        let dir = tempfile::tempdir().unwrap();
        write_workspace(dir.path());
        let engine = Engine::new(dir.path().to_path_buf(), Configuration::default()).unwrap();
        let labels = engine
            .list_package(&PackageId::new(CanonicalRepo::MAIN, PackagePath::root()))
            .unwrap();
        assert_eq!(
            labels.iter().map(|l| l.to_string()).collect::<Vec<_>>(),
            vec!["@@//:alpha", "@@//:world"],
        );
    }

    #[test]
    fn engine_construction_fails_on_bad_module_bazel() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("MODULE.bazel"), "").unwrap();
        let err = Engine::new(dir.path().to_path_buf(), Configuration::default()).unwrap_err();
        assert!(err.to_string().contains("did not call module"));
    }

    #[test]
    fn discover_packages_walks_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        write_workspace(dir.path());
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/BUILD"), "").unwrap();
        let engine = Engine::new(dir.path().to_path_buf(), Configuration::default()).unwrap();
        let pkgs = engine.discover_packages().unwrap();
        assert_eq!(
            pkgs.iter()
                .map(|p| p.path.as_str().to_string())
                .collect::<Vec<_>>(),
            vec!["", "sub"],
        );
    }

    #[test]
    fn module_graph_records_main_module_name() {
        let dir = tempfile::tempdir().unwrap();
        write_workspace(dir.path());
        let engine = Engine::new(dir.path().to_path_buf(), Configuration::default()).unwrap();
        let graph = engine.module_graph().unwrap();
        assert_eq!(graph.main.name, "demo");
    }
}
