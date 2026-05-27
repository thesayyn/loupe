//! End-to-end test for `use_repo_rule(...)` in MODULE.bazel.
//!
//! Bazel models a direct repo-rule call in MODULE.bazel as an "innate"
//! module extension. Loupe captures each `proxy(name=…, …)` invocation
//! and materialises it as a module-local `RepoSpec::StarlarkRule`
//! through the same lazy-demand path real `use_extension(...)`
//! extensions use — no static spec injection, no eager build-time
//! evaluation. This test drives that path: a `list_package` on the
//! generated repo's canonical triggers `maybe_materialise` →
//! `ExtMaterializedRepoKey` → the innate `ModuleExtensionKey` branch →
//! `materialize`.

use loupe_core::analysis::Configuration;
use loupe_core::engine::EngineBuilder;
use loupe_labels::CanonicalRepo;
use loupe_labels::PackageId;
use loupe_labels::PackagePath;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn use_repo_rule_materialises_module_local_repo() {
    let workspace = tempfile::tempdir().unwrap();
    // A local repository_rule: writes its BUILD/MODULE files plus a data
    // file derived from a call-site attribute. No download — exercises
    // the StarlarkRule materialisation, not the http_archive fast-path.
    std::fs::write(
        workspace.path().join("defs.bzl"),
        r#"
def _impl(ctx):
    ctx.file("MODULE.bazel", "module(name = \"%s\")\n" % ctx.name)
    ctx.file("BUILD.bazel", "filegroup(name = \"all\")\n")
    ctx.file("greeting.txt", ctx.attr.greeting)

my_repo = repository_rule(
    implementation = _impl,
    attrs = {
        "greeting": attr.string(),
    },
)
"#,
    )
    .unwrap();
    std::fs::write(
        workspace.path().join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")
my_repo = use_repo_rule("//:defs.bzl", "my_repo")
my_repo(name = "hello", greeting = "hi there")
"#,
    )
    .unwrap();
    std::fs::write(workspace.path().join("BUILD.bazel"), "").unwrap();

    let cache = tempfile::tempdir().unwrap();
    let workspace_path = workspace.path().to_path_buf();
    let cache_path = cache.path().to_path_buf();
    let engine = tokio::task::spawn_blocking(move || {
        EngineBuilder::new(workspace_path, Configuration::default())
            .with_bzlmod_cache(cache_path)
            .build()
    })
    .await
    .unwrap()
    .unwrap();

    // Main module's canonical is the empty string, so the innate repo's
    // canonical is `<sep><rule><sep><name>` = `+my_repo+hello`.
    let pkg = PackageId::new(CanonicalRepo::new("+my_repo+hello"), PackagePath::root());
    let labels = tokio::task::spawn_blocking(move || engine.list_package(&pkg))
        .await
        .unwrap()
        .unwrap();

    let names: Vec<String> = labels.iter().map(|l| l.to_string()).collect();
    assert_eq!(names, vec!["@@+my_repo+hello//:all"]);

    // The rule impl ran: its outputs landed in the cache, and the
    // call-site attribute reached `ctx.attr.greeting`.
    let repo_dir = cache.path().join("+my_repo+hello");
    assert!(repo_dir.join("BUILD.bazel").is_file());
    assert_eq!(
        std::fs::read_to_string(repo_dir.join("greeting.txt")).unwrap(),
        "hi there"
    );
}
