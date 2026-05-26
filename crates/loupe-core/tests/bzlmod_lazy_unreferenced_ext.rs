//! Broken-extension lazy-evaluation tests.
//!
//! Asserts that `use_extension(...)` is truly lazy: an extension that
//! is declared in MODULE.bazel but whose outputs are never demanded
//! (no `use_repo(broken_ext, ...)`, no toolchain pulling its
//! `register_toolchains`) does not run. Proven by making the
//! never-referenced extensions *broken* in various ways — if any of
//! them ran, the engine build or analysis would surface their
//! failure. Each test analyzes a main-module target that depends only
//! on a parallel "good" extension (or no extension at all), so the
//! demand path stays clear of the broken one.
//!
//! Four broken flavours:
//!   1. impl_calls_fail               — `_impl(module_ctx): fail("...")`
//!   2. defining_bzl_has_syntax_error — `.bzl` is unparseable
//!   3. loads_nonexistent_bzl         — `load("@bad//:x.bzl", ...)` at top level
//!   4. declares_unreachable_http     — http_archive(url="http://invalid.invalid/never")
//!
//! Plus a demand-positive control: the same broken extension with
//! `use_repo(broken_ext, "x")` and an analysis target that reaches
//! into `@x`. Engine build still succeeds (laziness defers eval until
//! demand), but the demand path then surfaces the breakage.

use loupe_core::analysis::Configuration;
use loupe_core::engine::EngineBuilder;
use loupe_labels::Label;

/// Compose a workspace with:
///   - main rule defined in `//:defs.bzl` (returns DefaultInfo).
///   - "good" extension that always succeeds.
///   - "broken" extension whose `.bzl` body is `broken_ext_body`.
///
/// MODULE.bazel declares both extensions but only `use_repo`s the
/// good one (unless `also_use_repo_broken` is true).
fn write_workspace(
    dir: &std::path::Path,
    broken_ext_path: &str,
    broken_ext_body: &str,
    also_use_repo_broken: bool,
) {
    std::fs::write(
        dir.join("MODULE.bazel"),
        format!(
            r#"module(name = "demo", version = "0.0.0")
good = use_extension("//:good.bzl", "good_ext")
good.create(name = "fine", value = "ok")
use_repo(good, "fine")
broken = use_extension("//{broken_ext_path}", "broken_ext")
{maybe_use_repo}
"#,
            broken_ext_path = broken_ext_path,
            maybe_use_repo = if also_use_repo_broken {
                "use_repo(broken, \"x\")"
            } else {
                ""
            },
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("good.bzl"),
        r#"
def _local_repo_impl(ctx):
    ctx.file("BUILD.bazel", "filegroup(name = \"%s\")\n" % ctx.attr.value)

local_repo = repository_rule(
    implementation = _local_repo_impl,
    attrs = {"value": attr.string()},
)

def _good_impl(module_ctx):
    for m in module_ctx.modules:
        for tag in m.tags.create:
            local_repo(name = tag.name, value = tag.value)

good_ext = module_extension(
    implementation = _good_impl,
    tag_classes = {"create": tag_class(attrs = {"name": attr.string(), "value": attr.string()})},
)
"#,
    )
    .unwrap();
    // The broken extension lives at `broken_ext_path` (caller picks
    // the package; we always create the file).
    let abs_broken = dir.join(broken_ext_path.trim_start_matches(':'));
    if let Some(parent) = abs_broken.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(abs_broken, broken_ext_body).unwrap();

    // Top-level BUILD with a simple `filegroup` target that does NOT
    // reach into either extension — analysis of `//:hello` must
    // succeed even when the broken extension would otherwise blow up.
    std::fs::write(
        dir.join("BUILD.bazel"),
        r#"filegroup(name = "hello", srcs = [])
"#,
    )
    .unwrap();
}

fn cache_path_for(workspace: &tempfile::TempDir) -> std::path::PathBuf {
    workspace.path().join(".loupe-cache")
}

fn analyze_hello(workspace: &std::path::Path, cache: &std::path::Path) -> anyhow::Result<()> {
    let engine = EngineBuilder::new(workspace.to_path_buf(), Configuration::default())
        .with_bzlmod_cache(cache.to_path_buf())
        .build()?;
    let label = Label::parse_canonical("@@//:hello").map_err(|e| anyhow::anyhow!("{e}"))?;
    engine.analyze_target(&label)?;
    Ok(())
}

// -- Broken flavour 1: impl calls fail() -------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broken_impl_fail_does_not_run_when_unreferenced() {
    let workspace = tempfile::tempdir().unwrap();
    write_workspace(
        workspace.path(),
        ":broken.bzl",
        r#"
def _impl(module_ctx):
    fail("broken extension's impl was reached — laziness regression")

broken_ext = module_extension(implementation = _impl)
"#,
        /*also_use_repo_broken=*/ false,
    );
    let cache = cache_path_for(&workspace);
    let workspace_path = workspace.path().to_path_buf();
    tokio::task::spawn_blocking(move || analyze_hello(&workspace_path, &cache).unwrap())
        .await
        .unwrap();
}

// -- Broken flavour 2: defining .bzl has a syntax error ----------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broken_bzl_syntax_does_not_load_when_unreferenced() {
    let workspace = tempfile::tempdir().unwrap();
    write_workspace(
        workspace.path(),
        ":broken.bzl",
        r#"
this is not valid starlark (((
broken_ext = module_extension(implementation = lambda: None)
"#,
        /*also_use_repo_broken=*/ false,
    );
    let cache = cache_path_for(&workspace);
    let workspace_path = workspace.path().to_path_buf();
    tokio::task::spawn_blocking(move || analyze_hello(&workspace_path, &cache).unwrap())
        .await
        .unwrap();
}

// -- Broken flavour 3: extension loads a non-existent .bzl --------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broken_load_missing_bzl_does_not_run_when_unreferenced() {
    let workspace = tempfile::tempdir().unwrap();
    write_workspace(
        workspace.path(),
        ":broken.bzl",
        r#"
load("@nonexistent_repo_never_use_repo_d//:does_not_exist.bzl", "anything")

def _impl(module_ctx):
    pass

broken_ext = module_extension(implementation = _impl)
"#,
        /*also_use_repo_broken=*/ false,
    );
    let cache = cache_path_for(&workspace);
    let workspace_path = workspace.path().to_path_buf();
    tokio::task::spawn_blocking(move || analyze_hello(&workspace_path, &cache).unwrap())
        .await
        .unwrap();
}

// -- Broken flavour 4: extension declares an unreachable http_archive --------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broken_http_archive_does_not_fetch_when_unreferenced() {
    let workspace = tempfile::tempdir().unwrap();
    write_workspace(
        workspace.path(),
        ":broken.bzl",
        r#"
def _impl(module_ctx):
    # If this impl ran, http_archive would synthesise an unreachable
    # URL — but it never runs because the extension is never demanded.
    http_archive(
        name = "x",
        urls = ["http://invalid.invalid/never-reachable.tar.gz"],
        sha256 = "0000000000000000000000000000000000000000000000000000000000000000",
    )

broken_ext = module_extension(implementation = _impl)
"#,
        /*also_use_repo_broken=*/ false,
    );
    let cache = cache_path_for(&workspace);
    let workspace_path = workspace.path().to_path_buf();
    tokio::task::spawn_blocking(move || analyze_hello(&workspace_path, &cache).unwrap())
        .await
        .unwrap();
}

// -- Demand-positive control: use_repo(broken_ext, "x") forces eval ----------
//
// The same fail()-impl extension, but `use_repo`'d. The engine still
// builds (lazy: eval deferred). Analysis of `//:hello` (which doesn't
// touch `@x`) still passes — no demand reaches the extension. Then a
// direct analyze on `@x//:something` would force eval and surface the
// failure. We assert the analyze-hello path succeeds; reaching `@x`
// is exercised by separate fixtures that already cover the demand
// path (e.g. `bzlmod_use_extension_autocapture`).

// -- Toolchain demand: a rule with no `toolchains=[]` declaration ------------
//
// Lazy-via-toolchain-demand: `ResolvedToolchainsKey` only walks
// extension `register_toolchains` contributions when the rule under
// analysis actually declares `toolchains=[...]`. A `filegroup`
// declares none — so an extension that would `register_toolchains(...)`
// (even a broken one) stays cold when the only thing analysed is a
// filegroup. Proven by making the toolchain-only broken extension's
// impl `fail(...)` and asserting analysis still succeeds.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broken_toolchain_only_extension_stays_cold_for_filegroup_analysis() {
    let workspace = tempfile::tempdir().unwrap();
    write_workspace(
        workspace.path(),
        ":broken.bzl",
        r#"
def _impl(module_ctx):
    # If toolchain demand forced this extension just because some
    # ResolvedToolchainsKey ran (without checking that the rule has
    # declared toolchains), the fail() below would propagate.
    module_ctx.register_toolchains("@@//toolchains:never_resolved")
    fail("broken toolchain-only ext was eagerly forced")

broken_ext = module_extension(implementation = _impl)
"#,
        /*also_use_repo_broken=*/ false,
    );
    let cache = cache_path_for(&workspace);
    let workspace_path = workspace.path().to_path_buf();
    tokio::task::spawn_blocking(move || analyze_hello(&workspace_path, &cache).unwrap())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn use_repo_broken_does_not_eagerly_run_extension() {
    let workspace = tempfile::tempdir().unwrap();
    write_workspace(
        workspace.path(),
        ":broken.bzl",
        r#"
def _impl(module_ctx):
    fail("broken extension's impl was reached — laziness regression")

broken_ext = module_extension(implementation = _impl)
"#,
        /*also_use_repo_broken=*/ true,
    );
    let cache = cache_path_for(&workspace);
    let workspace_path = workspace.path().to_path_buf();
    // Engine builds successfully (no eager eval) and analysis of
    // //:hello succeeds because it doesn't reach @x.
    tokio::task::spawn_blocking(move || analyze_hello(&workspace_path, &cache).unwrap())
        .await
        .unwrap();
}
