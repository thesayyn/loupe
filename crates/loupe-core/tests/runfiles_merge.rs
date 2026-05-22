//! `ctx.runfiles` merge semantics — patterned after
//! `src/test/java/com/google/devtools/build/lib/analysis/RunfilesTest.java`.
//! Covers: empty+empty, empty+non-empty, transitive_files vs files,
//! and merging across two deps.

use std::fs;
use std::path::{Path, PathBuf};

use loupe_core::{Configuration, Engine};
use loupe_labels::{CanonicalRepo, Label, PackagePath};

fn write(dir: &Path, files: &[(&str, &str)]) {
    for (name, contents) in files {
        let full = dir.join(name);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(full, contents).unwrap();
    }
}

fn label(name: &str) -> Label {
    Label::new(CanonicalRepo::MAIN, PackagePath::root(), name)
}

fn engine(dir: PathBuf) -> Engine {
    Engine::new(dir, Configuration::default()).unwrap()
}

#[test]
fn runfiles_merge_unions_files_from_two_deps() {
    // Two leaf rules each carrying one runfile via DefaultInfo.runfiles.
    // The consumer merges both deps' runfiles. The resulting runfiles
    // should include both files.
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        &[
            (
                "MODULE.bazel",
                r#"module(name = "demo", version = "0.0.0")"#,
            ),
            (
                "defs.bzl",
                r#"
def _leaf_impl(ctx):
    f = ctx.actions.declare_file(ctx.attr.name + ".dat")
    ctx.actions.write(f, ctx.attr.name)
    return [DefaultInfo(
        files = depset([f]),
        runfiles = ctx.runfiles(files = [f]),
    )]

NamesInfo = provider(fields = ["names"])

def _consumer_impl(ctx):
    rf = ctx.attr.a[DefaultInfo].default_runfiles.merge(
        ctx.attr.b[DefaultInfo].default_runfiles,
    )
    names = sorted([f.basename for f in rf.files.to_list()])
    return [
        NamesInfo(names = ",".join(names)),
        DefaultInfo(runfiles = rf),
    ]

leaf = rule(implementation = _leaf_impl)
consumer = rule(
    implementation = _consumer_impl,
    attrs = {
        "a": attr.label(),
        "b": attr.label(),
    },
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"load(":defs.bzl", "leaf", "consumer")
leaf(name = "x")
leaf(name = "y")
consumer(name = "world", a = ":x", b = ":y")
"#,
            ),
        ],
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "NamesInfo")
        .expect("NamesInfo missing");
    let names = p
        .fields
        .get("names")
        .unwrap()
        .to_value()
        .unpack_str()
        .unwrap()
        .to_string();
    assert!(
        names.contains("x.dat") && names.contains("y.dat"),
        "merged runfiles should contain both: {names}"
    );
}

#[test]
fn runfiles_merge_with_empty_returns_only_non_empty_files() {
    // Empty `ctx.runfiles()` merged with a non-empty one keeps the
    // non-empty content.
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        &[
            (
                "MODULE.bazel",
                r#"module(name = "demo", version = "0.0.0")"#,
            ),
            (
                "defs.bzl",
                r#"
NamesInfo = provider(fields = ["names"])

def _impl(ctx):
    f = ctx.actions.declare_file("only.dat")
    ctx.actions.write(f, "only")
    full = ctx.runfiles(files = [f])
    empty = ctx.runfiles()
    merged = full.merge(empty)
    names = sorted([fn.basename for fn in merged.files.to_list()])
    return [
        NamesInfo(names = ",".join(names)),
        DefaultInfo(runfiles = merged),
    ]

mk = rule(implementation = _impl)
"#,
            ),
            (
                "BUILD.bazel",
                r#"load(":defs.bzl", "mk")
mk(name = "world")
"#,
            ),
        ],
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "NamesInfo")
        .unwrap();
    let names = p
        .fields
        .get("names")
        .unwrap()
        .to_value()
        .unpack_str()
        .unwrap()
        .to_string();
    assert_eq!(names, "only.dat");
}

#[test]
fn runfiles_empty_plus_empty_yields_empty() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        &[
            (
                "MODULE.bazel",
                r#"module(name = "demo", version = "0.0.0")"#,
            ),
            (
                "defs.bzl",
                r#"
CountInfo = provider(fields = ["count"])
def _impl(ctx):
    a = ctx.runfiles()
    b = ctx.runfiles()
    merged = a.merge(b)
    return [
        CountInfo(count = str(len(merged.files.to_list()))),
        DefaultInfo(runfiles = merged),
    ]
mk = rule(implementation = _impl)
"#,
            ),
            (
                "BUILD.bazel",
                r#"load(":defs.bzl", "mk")
mk(name = "world")
"#,
            ),
        ],
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "CountInfo")
        .unwrap();
    let count = p
        .fields
        .get("count")
        .unwrap()
        .to_value()
        .unpack_str()
        .unwrap()
        .to_string();
    assert_eq!(count, "0");
}

#[test]
fn runfiles_transitive_files_propagates_via_depset() {
    // `ctx.runfiles(transitive_files=depset(...))` accepts a depset
    // directly and keeps its lazy shape until flattened.
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        &[
            (
                "MODULE.bazel",
                r#"module(name = "demo", version = "0.0.0")"#,
            ),
            (
                "defs.bzl",
                r#"
CountInfo = provider(fields = ["count"])
def _impl(ctx):
    a = ctx.actions.declare_file("a.txt")
    b = ctx.actions.declare_file("b.txt")
    ctx.actions.write(a, "")
    ctx.actions.write(b, "")
    rf = ctx.runfiles(transitive_files = depset([a, b]))
    return [
        CountInfo(count = str(len(rf.files.to_list()))),
        DefaultInfo(runfiles = rf),
    ]
mk = rule(implementation = _impl)
"#,
            ),
            (
                "BUILD.bazel",
                r#"load(":defs.bzl", "mk")
mk(name = "world")
"#,
            ),
        ],
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == "CountInfo")
        .unwrap();
    let count = p
        .fields
        .get("count")
        .unwrap()
        .to_value()
        .unpack_str()
        .unwrap()
        .to_string();
    assert_eq!(count, "2", "transitive_files depset must flatten");
}
