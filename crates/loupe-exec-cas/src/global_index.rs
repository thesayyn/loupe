//! DICE key that maps every workspace-reachable output path to the
//! `ActionRef` that produces it.
//!
//! Cross-target generated-input dependencies are the common case in
//! real rulesets: rules_js's `js_run_binary` consumes outputs of
//! `js_binary` targets in other packages, `oci_image` consumes outputs
//! of `js_image_layer` targets, etc. The local-only producer scan in
//! `ActionDigestKey::compute` (look at `self_target.actions`) misses
//! every such edge.
//!
//! Shape: walk `DiscoverPackagesKey` → `PackageKey` → `AnalysisKey`
//! across the whole workspace and, for every analyzed target's every
//! recorded action's every output, insert `(output.path(), ActionRef)`
//! into a `BTreeMap`. Memoised by DICE so the second key that consults
//! it pays nothing.
//!
//! Two outputs at the same path from different actions is a Bazel
//! analysis error in real life (`-> Conflicting output file`), so the
//! map's "last writer wins" behaviour on collision doesn't actually
//! lose information — collisions can't happen in well-formed graphs.
//! The empty-target / failed-target buckets surface as omitted
//! entries; consumers fall back to `UnknownGenerated` in that case.

use std::collections::BTreeMap;

use allocative::Allocative;
use async_trait::async_trait;
use dice::{CancellationContext, DiceComputations, Key, NoValueSerialize, ValueSerialize};
use pagable::{Pagable, pagable_typetag};

use loupe_core::analysis::Configuration;
use loupe_core::dice_keys::{AnalysisKey, DiscoverPackagesKey, PackageKey, SharedResult};

use crate::keys::ActionRef;

/// Singleton (per `Configuration`) DICE key. The map keys are output
/// paths exactly as `File::path()` produces them; the values record
/// which `(target, action_index)` writes that path.
#[derive(Clone, Debug, derive_more::Display, Eq, Hash, PartialEq, Allocative, Pagable)]
#[display("GlobalActionIndexKey({_0})")]
#[pagable_typetag(dice::DiceKeyDyn)]
pub struct GlobalActionIndexKey(pub Configuration);

/// One entry of the global producer index: which action emits a path,
/// and (for tree artifacts) what kind of output that path is so the
/// caller can branch into the Tree-lookup path.
#[derive(Clone, Debug, PartialEq, Eq, Allocative)]
pub struct ProducerEntry {
    pub action_ref: ActionRef,
    pub output_index: usize,
    /// True when the producer's output at this path is a tree
    /// artifact (`ctx.actions.declare_directory`). Downstream consumers
    /// that request a sub-path of a tree need to fetch the producer's
    /// `Tree` blob from CAS — flag here so the lookup branches.
    pub is_directory: bool,
}

#[async_trait]
impl Key for GlobalActionIndexKey {
    type Value = SharedResult<BTreeMap<String, ProducerEntry>>;

    async fn compute(&self, ctx: &mut DiceComputations, _: &CancellationContext) -> Self::Value {
        let pkgs_arc = match ctx.compute(&DiscoverPackagesKey).await {
            Ok(SharedResult::Ok(p)) => p,
            Ok(SharedResult::Err(e)) => return SharedResult::err((*e).clone()),
            Err(e) => return SharedResult::err(format!("DICE compute error: {e}")),
        };
        // Wave 1: load every package in parallel.
        let pkg_keys: Vec<_> = pkgs_arc.iter().cloned().collect();
        let pkg_results: Vec<_> = ctx
            .compute_join(pkg_keys, |c, pkg_id| {
                use futures::FutureExt;
                async move { (pkg_id.clone(), c.compute(&PackageKey(pkg_id)).await) }.boxed()
            })
            .await;
        // Collect every (label, config) needing analysis. Skip the
        // packages that failed to load — they can't contribute outputs
        // and a half-broken workspace shouldn't sink the whole index.
        let mut to_analyze: Vec<loupe_labels::Label> = Vec::new();
        for (_pkg_id, result) in pkg_results {
            if let Ok(SharedResult::Ok(pkg)) = result {
                for target in pkg.targets.values() {
                    to_analyze.push(target.label.clone());
                }
            }
        }
        // Wave 2: analyze every target in parallel. Like Wave 1, swallow
        // per-target failures — a target whose analysis errors out has
        // no actions to index. Errors here surface via the *direct*
        // AnalysisKey path the rest of the engine takes; the index
        // just doesn't list its outputs.
        let cfg = self.0.clone();
        let analyzed_results: Vec<(loupe_labels::Label, _)> = ctx
            .compute_join(to_analyze, |c, label| {
                use futures::FutureExt;
                let cfg = cfg.clone();
                async move {
                    let r = c.compute(&AnalysisKey(label.clone(), cfg)).await;
                    (label, r)
                }
                .boxed()
            })
            .await;

        let mut index: BTreeMap<String, ProducerEntry> = BTreeMap::new();
        for (label, result) in analyzed_results {
            let analyzed = match result {
                Ok(SharedResult::Ok(a)) => a,
                _ => continue,
            };
            for (action_index, action) in analyzed.actions.iter().enumerate() {
                let action_ref = ActionRef {
                    target: label.clone(),
                    config: analyzed.configuration.clone(),
                    action_index,
                };
                for (output_index, out) in action.outputs.iter().enumerate() {
                    index.insert(
                        out.path(),
                        ProducerEntry {
                            action_ref: action_ref.clone(),
                            output_index,
                            is_directory: out.is_directory(),
                        },
                    );
                }
            }
        }
        SharedResult::ok(index)
    }

    fn equality(x: &Self::Value, y: &Self::Value) -> bool {
        match (x, y) {
            (SharedResult::Ok(a), SharedResult::Ok(b)) => a == b,
            (SharedResult::Err(a), SharedResult::Err(b)) => a == b,
            _ => false,
        }
    }

    fn value_serialize() -> impl ValueSerialize<Value = Self::Value> {
        NoValueSerialize::<Self::Value>::new()
    }
}

/// Look up a producer for `path`. Handles two cases:
///
///   1. `path` is exactly an output path the index records — direct
///      hit (file artifact or tree-artifact root requested as a unit).
///   2. `path` has a tree-artifact output as a strict path prefix — the
///      consumer is asking for a file inside a tree artifact. Return
///      the tree's `ProducerEntry` plus the remaining sub-path so the
///      caller can fetch the `Tree` blob from CAS and walk it.
pub fn lookup<'a>(
    index: &'a BTreeMap<String, ProducerEntry>,
    path: &str,
) -> Option<ProducerLookup<'a>> {
    if let Some(entry) = index.get(path) {
        return Some(ProducerLookup::Direct(entry));
    }
    // Prefix match against tree-artifact outputs. The index is a
    // BTreeMap so we can walk from the longest plausible prefix
    // downwards — Bazel paths use `/` as the segment separator.
    let mut maybe_root = path;
    while let Some(slash) = maybe_root.rfind('/') {
        let candidate = &path[..slash];
        if let Some(entry) = index.get(candidate)
            && entry.is_directory
        {
            return Some(ProducerLookup::TreeSubPath {
                entry,
                sub_path: path[slash + 1..].to_string(),
            });
        }
        maybe_root = candidate;
    }
    None
}

#[derive(Debug)]
pub enum ProducerLookup<'a> {
    /// The full input path matches an output path in the index.
    Direct(&'a ProducerEntry),
    /// The input path is `<tree_artifact_root>/<sub_path>` — fetch the
    /// tree's CAS blob and walk to `sub_path` inside it.
    TreeSubPath {
        entry: &'a ProducerEntry,
        sub_path: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use loupe_labels::{CanonicalRepo, Label, PackageId, PackagePath};

    fn label(name: &str) -> Label {
        Label::in_package(
            PackageId::new(CanonicalRepo::MAIN, PackagePath::root()),
            name,
        )
    }

    fn aref(target: &Label, action_index: usize) -> ActionRef {
        ActionRef {
            target: target.clone(),
            config: Configuration::default(),
            action_index,
        }
    }

    fn entry(
        target: &Label,
        action_index: usize,
        output_index: usize,
        is_dir: bool,
    ) -> ProducerEntry {
        ProducerEntry {
            action_ref: aref(target, action_index),
            output_index,
            is_directory: is_dir,
        }
    }

    #[test]
    fn lookup_direct_hit() {
        // Bazel's own-target chain: the consumer asks for the exact
        // output path of a producer in the same workspace. Direct hit.
        let producer = label("p");
        let mut idx = BTreeMap::new();
        idx.insert(
            "bazel-out/k8/bin/pkg/out.bin".to_string(),
            entry(&producer, 0, 0, false),
        );
        match lookup(&idx, "bazel-out/k8/bin/pkg/out.bin") {
            Some(ProducerLookup::Direct(e)) => {
                assert_eq!(e.action_ref.target, producer);
                assert_eq!(e.output_index, 0);
            }
            other => panic!("expected Direct, got {other:?}"),
        }
    }

    #[test]
    fn lookup_misses_when_path_not_present() {
        let idx: BTreeMap<String, ProducerEntry> = BTreeMap::new();
        assert!(lookup(&idx, "bazel-out/k8/bin/pkg/missing").is_none());
    }

    #[test]
    fn lookup_does_not_match_non_directory_prefix() {
        // A regular generated file at `bazel-out/k8/bin/pkg/foo.txt`
        // must NOT be considered a producer of a hypothetical
        // `bazel-out/k8/bin/pkg/foo.txt/inner` — only declare_directory
        // outputs allow sub-path lookup.
        let producer = label("p");
        let mut idx = BTreeMap::new();
        idx.insert(
            "bazel-out/k8/bin/pkg/foo.txt".to_string(),
            entry(&producer, 0, 0, false),
        );
        assert!(lookup(&idx, "bazel-out/k8/bin/pkg/foo.txt/inner").is_none());
    }

    #[test]
    fn lookup_resolves_tree_subpath_to_root_entry() {
        // `ctx.actions.declare_directory("foo")` produces a tree
        // artifact at `bazel-out/k8/bin/pkg/foo`. A downstream input at
        // `bazel-out/k8/bin/pkg/foo/x.js` must resolve to the tree's
        // ProducerEntry with `sub_path = "x.js"`.
        let producer = label("p");
        let mut idx = BTreeMap::new();
        idx.insert(
            "bazel-out/k8/bin/pkg/foo".to_string(),
            entry(&producer, 0, 0, true),
        );
        match lookup(&idx, "bazel-out/k8/bin/pkg/foo/x.js") {
            Some(ProducerLookup::TreeSubPath { entry, sub_path }) => {
                assert_eq!(entry.action_ref.target, producer);
                assert_eq!(sub_path, "x.js");
                assert!(entry.is_directory);
            }
            other => panic!("expected TreeSubPath, got {other:?}"),
        }
    }

    #[test]
    fn lookup_resolves_nested_tree_subpath() {
        // The tree-artifact prefix can be any number of segments deep
        // into the path. `bazel-out/k8/bin/pkg/foo/nested/x.js` against
        // a tree at `bazel-out/k8/bin/pkg/foo` must return
        // `sub_path = "nested/x.js"`.
        let producer = label("p");
        let mut idx = BTreeMap::new();
        idx.insert(
            "bazel-out/k8/bin/pkg/foo".to_string(),
            entry(&producer, 0, 0, true),
        );
        match lookup(&idx, "bazel-out/k8/bin/pkg/foo/nested/x.js") {
            Some(ProducerLookup::TreeSubPath { sub_path, .. }) => {
                assert_eq!(sub_path, "nested/x.js");
            }
            other => panic!("expected TreeSubPath, got {other:?}"),
        }
    }

    #[test]
    fn lookup_prefers_direct_match_over_tree_subpath() {
        // If a path is BOTH a direct output entry AND a sub-path of a
        // tree-artifact prefix (degenerate but legal), direct match
        // wins.
        let p1 = label("p1");
        let p2 = label("p2");
        let mut idx = BTreeMap::new();
        idx.insert(
            "bazel-out/k8/bin/pkg/foo".to_string(),
            entry(&p1, 0, 0, true),
        );
        idx.insert(
            "bazel-out/k8/bin/pkg/foo/x.js".to_string(),
            entry(&p2, 0, 0, false),
        );
        match lookup(&idx, "bazel-out/k8/bin/pkg/foo/x.js") {
            Some(ProducerLookup::Direct(e)) => assert_eq!(e.action_ref.target, p2),
            other => panic!("expected Direct, got {other:?}"),
        }
    }
}
