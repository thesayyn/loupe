//! Analyses the pinned aspect-build/rules_js source tree end-to-end.
//!
//! `build.rs` clones rules_js into `OUT_DIR` and exports its path
//! through `RULES_JS_SOURCE_PATH`. This test drives
//! `Engine::analyze_workspace` against that tree, then asserts a
//! sensible baseline: at least one target analyzed successfully and
//! no load-level package errors (a load failure means we couldn't even
//! parse a BUILD — much earlier than the target-level gaps the probe
//! exists to surface).

use std::path::PathBuf;

use loupe_core::Configuration;
use loupe_core::engine::EngineBuilder;

fn rules_js_source() -> PathBuf {
    PathBuf::from(env!("RULES_JS_SOURCE_PATH"))
}

/// Install a tracing subscriber that respects `RUST_LOG` (defaulting
/// to a filter loud enough to surface loupe-level spans + BCR fetches
/// but quiet enough to keep DICE's internal bookkeeping out of the
/// way). Idempotent — `try_init` so re-runs in the same process don't
/// panic.
///
/// Spans are useless without events: `#[instrument]` creates a span
/// but doesn't emit a line unless the formatter is told to. Enabling
/// `FmtSpan::NEW | FmtSpan::CLOSE` makes each instrumented compute
/// log on entry and on exit-with-elapsed-time, so a slow DICE key
/// surfaces as a long `time.busy` on its close line.
fn init_tracing() {
    use tracing_subscriber::fmt::format::FmtSpan;
    use tracing_subscriber::{EnvFilter, fmt};
    // DICE logs every key transition at debug; with hundreds of keys
    // it drowns out our own spans. Default it to warn while keeping
    // loupe crates at debug.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,loupe_core=debug,loupe_bzlmod=debug,loupe_labels=debug,dice=warn")
    });
    let _ = fmt()
        .with_env_filter(filter)
        .with_test_writer()
        .with_target(true)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .try_init();
}

#[test]
#[ignore = "manual probe: build.rs git-clones rules_js and the assertions need full \
            bzlmod/analysis support + live BCR access; run with `--ignored`"]
fn analyse_rules_js_source_tree() {
    init_tracing();

    let workspace = rules_js_source();
    assert!(
        workspace.join("MODULE.bazel").exists(),
        "rules_js clone at {} missing MODULE.bazel",
        workspace.display()
    );

    // Mirror the CLI's `--bzlmod` default: cache under `<workspace>/.loupe/bzlmod`,
    // resolve `bazel_dep`s through BCR. Without these the in-process
    // resolver never registers transitive specs, so loading any
    // `@@bazel_skylib+//:bzl_library.bzl`-style label fails at the
    // package-load stage.
    let cache = workspace.join(".loupe").join("bzlmod");
    // Default target platform `@@platforms//host:host`, matching the
    // CLI. Without it, rules that call
    // `ctx.target_platform_has_constraint(...)` (rules_js's
    // `utf8_environment.bzl`, many toolchain-aware rules) fail
    // analysis with "requires a configured target platform".
    let host_platform = loupe_core::Label::resolve(
        "@@platforms//host:host",
        &loupe_labels::PackageId::new(
            loupe_labels::CanonicalRepo::main(),
            loupe_labels::PackagePath::root(),
        ),
        &Default::default(),
    )
    .expect("resolving @@platforms//host:host");
    let config = Configuration::default().with_target_platform(host_platform);
    let engine = EngineBuilder::new(workspace.clone(), config)
        .with_bzlmod_cache(cache)
        .with_bcr()
        .build()
        .expect("Engine::build on rules_js source");

    let result = engine
        .analyze_workspace(&[])
        .expect("analyze_workspace on rules_js source");

    let analyzed = result.analyzed.len();
    let skipped = result.skipped.len();
    let failed = result.failed.len();
    let pkg_errors = result.package_errors.len();
    eprintln!(
        "rules_js probe: {analyzed} analyzed, {skipped} skipped, {failed} failed, \
         {pkg_errors} package errors"
    );

    // Show the first handful of each error bucket so a regression
    // surfaces actionable detail without dumping the whole vec.
    {
        // Bucket analysis failures by the first line of their error so
        // the dominant gaps surface without dumping thousands of lines.
        let mut buckets: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for fe in &result.failed {
            let head = fe.error.lines().next().unwrap_or("").to_string();
            *buckets.entry(head).or_default() += 1;
        }
        let mut sorted: Vec<_> = buckets.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
        eprintln!("  --- failure buckets ---");
        for (head, n) in sorted.iter().take(25) {
            eprintln!("  {n:>5}  {head}");
        }
        // Full error text for the top buckets' first matching failure.
        // Sample deeply (not just the top 3) so every remaining bucket's
        // traceback is visible as the failure count shrinks toward zero.
        eprintln!("  --- sample full errors (top buckets) ---");
        for (head, _) in sorted.iter().take(15) {
            if let Some(fe) = result
                .failed
                .iter()
                .find(|fe| fe.error.lines().next().unwrap_or("") == head)
            {
                eprintln!("  >>> {} :\n{}", fe.label, fe.error);
            }
        }
    }
    for pe in result.package_errors.iter().take(10) {
        eprintln!("  package {}: {}", pe.package, pe.error);
    }

    assert!(
        analyzed > 0,
        "no targets analysed — the engine failed end-to-end on rules_js",
    );
    assert_eq!(
        pkg_errors, 0,
        "package load errors should not occur (no BUILD/.bzl should fail to load)",
    );
    // Full end-to-end goal: every configured target analyses without error.
    // Gating on this (not just tracking it) makes the probe job fail while
    // analysis gaps remain — the failure is the signal that drives the next
    // engine fix. The failure buckets printed above name what's missing.
    assert_eq!(
        failed, 0,
        "analysis failures must be zero for full e2e (see failure buckets above)",
    );
}
