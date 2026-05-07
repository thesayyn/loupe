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
use crate::dice_keys::{
    AnalysisKey, ModuleGraphKey, PackageKey, SetBzlmodLockfilePath, SetExternalRepoRoot,
    SetWorkspaceRoot,
};
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
        EngineBuilder::new(workspace_root, config).build()
    }

    /// Builder for configuring optional fields (external repo root, …)
    /// without breaking the two-arg [`Engine::new`] callers.
    pub fn builder(workspace_root: PathBuf, config: Configuration) -> EngineBuilder {
        EngineBuilder::new(workspace_root, config)
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

    /// Walk every package in the workspace, list every target, and
    /// run `analyze_target` on each. Targets whose `kind` is in
    /// `skip_kinds` are recorded in `skipped` and not analyzed (e.g.
    /// `oci_image_index` uses Starlark config transitions, which v1
    /// doesn't model). Per-target and per-package errors are
    /// collected, not propagated — the whole-workspace walk completes
    /// regardless so callers can see the full failure surface.
    #[instrument(skip(self, skip_kinds))]
    pub fn analyze_workspace(&self, skip_kinds: &[&str]) -> Result<WorkspaceAnalysis> {
        let skip: std::collections::BTreeSet<&str> = skip_kinds.iter().copied().collect();
        let packages = self.discover_packages()?;
        let mut out = WorkspaceAnalysis::default();
        for pkg in packages {
            let package = match self.query_package(&pkg) {
                Ok(p) => p,
                Err(e) => {
                    out.package_errors.push(PackageError {
                        package: pkg,
                        error: format!("{e:#}"),
                    });
                    continue;
                }
            };
            for target in package.targets.values() {
                if skip.contains(target.kind.as_str()) {
                    out.skipped.push(SkippedTarget {
                        label: target.label.clone(),
                        kind: target.kind.clone(),
                    });
                    continue;
                }
                match self.analyze_target(&target.label) {
                    Ok(at) => out.analyzed.push(at),
                    Err(e) => out.failed.push(FailedTarget {
                        label: target.label.clone(),
                        kind: target.kind.clone(),
                        error: format!("{e:#}"),
                    }),
                }
            }
        }
        Ok(out)
    }
}

/// Result of [`Engine::analyze_workspace`]. All four buckets are
/// populated; ordering follows package then target traversal order.
#[derive(Debug, Default)]
pub struct WorkspaceAnalysis {
    pub analyzed: Vec<Arc<AnalyzedTarget>>,
    pub skipped: Vec<SkippedTarget>,
    pub failed: Vec<FailedTarget>,
    pub package_errors: Vec<PackageError>,
}

#[derive(Debug, Clone)]
pub struct SkippedTarget {
    pub label: Label,
    pub kind: String,
}

#[derive(Debug, Clone)]
pub struct FailedTarget {
    pub label: Label,
    pub kind: String,
    pub error: String,
}

#[derive(Debug, Clone)]
pub struct PackageError {
    pub package: PackageId,
    pub error: String,
}

async fn open_txn(dice: &Arc<Dice>) -> DiceTransaction {
    dice.updater().commit().await
}

/// Builder for [`Engine`] — preserves the simple `Engine::new` API
/// while letting callers opt into the external-repo root needed for
/// loading `.bzl`s from vendored bzlmod deps.
pub struct EngineBuilder {
    workspace_root: PathBuf,
    config: Configuration,
    external_repo_root: Option<PathBuf>,
    bzlmod_lockfile_path: Option<PathBuf>,
}

impl EngineBuilder {
    pub fn new(workspace_root: PathBuf, config: Configuration) -> Self {
        EngineBuilder {
            workspace_root,
            config,
            external_repo_root: None,
            bzlmod_lockfile_path: None,
        }
    }

    /// Path to the directory holding vendored bzlmod deps. Each
    /// canonical repo lives at `<root>/<canonical_name>/`. Typical
    /// values: `<workspace>/vendor` (output of `bazel mod vendor`) or
    /// `<bazel info output_base>/external`.
    pub fn external_repo_root(mut self, root: PathBuf) -> Self {
        self.external_repo_root = Some(root);
        self
    }

    /// Path to a `MODULE.bazel.lock` that carries the per-repo
    /// `recordedInputs` records. Bazel 7 writes the rich lockfile
    /// (`lockFileVersion: 26`) to `<output_base>/MODULE.bazel.lock`,
    /// not the workspace copy (which is `lockFileVersion: 11` and
    /// missing the records). When the workspace copy is rich enough
    /// (Bazel 8+), this override isn't needed — the default reads
    /// `<workspace>/MODULE.bazel.lock`.
    pub fn bzlmod_lockfile_path(mut self, path: PathBuf) -> Self {
        self.bzlmod_lockfile_path = Some(path);
        self
    }

    pub fn build(self) -> Result<Engine> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let mut builder = Dice::builder();
        builder.set_workspace_root(self.workspace_root.clone());
        builder.set_external_repo_root(self.external_repo_root);
        builder.set_bzlmod_lockfile_path(self.bzlmod_lockfile_path);
        let dice = builder.build(dice::DetectCycles::Disabled);
        let engine = Engine {
            workspace_root: self.workspace_root,
            dice,
            runtime,
            config: self.config,
        };
        engine.module_graph()?;
        Ok(engine)
    }
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

    fn write_workspace_with_skipped_and_failing(dir: &std::path::Path) {
        // Two rule kinds: `hello` (analyzes cleanly) and `image_index`
        // (the rule we'll ask the bulk driver to skip). A third target
        // uses `hello` but with a non-existent label dep to force a
        // per-target failure.
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

def _broken_impl(ctx):
    fail("on purpose")

hello = rule(
    implementation = _hello_impl,
    attrs = {},
)

broken = rule(
    implementation = _broken_impl,
    attrs = {},
)

image_index = rule(
    implementation = _hello_impl,
    attrs = {},
)
"#,
        )
        .unwrap();
        fs::write(
            dir.join("BUILD.bazel"),
            r#"
load(":defs.bzl", "hello", "broken", "image_index")
hello(name = "ok")
broken(name = "broken")
image_index(name = "skipme")
"#,
        )
        .unwrap();
    }

    #[test]
    fn analyze_workspace_buckets_results() {
        let dir = tempfile::tempdir().unwrap();
        write_workspace_with_skipped_and_failing(dir.path());
        let engine = Engine::new(dir.path().to_path_buf(), Configuration::default()).unwrap();
        let report = engine.analyze_workspace(&["image_index"]).unwrap();
        let analyzed: Vec<_> = report
            .analyzed
            .iter()
            .map(|a| a.label.to_string())
            .collect();
        assert_eq!(analyzed, vec!["@@//:ok"]);
        let skipped: Vec<_> = report
            .skipped
            .iter()
            .map(|s| (s.label.to_string(), s.kind.clone()))
            .collect();
        assert_eq!(
            skipped,
            vec![("@@//:skipme".to_string(), "image_index".to_string())]
        );
        let failed: Vec<_> = report.failed.iter().map(|f| f.label.to_string()).collect();
        assert_eq!(failed, vec!["@@//:broken"]);
        assert!(report.package_errors.is_empty());
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
