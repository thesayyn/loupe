//! `analysis_test_transition(settings)` — the builtin transition
//! `analysistest.make(config_settings=…)` attaches to its
//! `target_under_test` attr. It must build a real transition that
//! overlays the settings onto the target-under-test's configuration
//! (previously a `None`-returning stub that `attr.label(cfg=…)`
//! misclassified as a broken user transition).

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
    Label::new(CanonicalRepo::main(), PackagePath::root(), name)
}

fn engine(dir: PathBuf) -> Engine {
    Engine::new(dir, Configuration::default()).unwrap()
}

#[test]
fn analysis_test_transition_overlays_settings_on_target_under_test() {
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
_t = analysis_test_transition(
    settings = {"//command_line_option:compilation_mode": "opt"},
)

def _under_test_impl(ctx):
    f = ctx.actions.declare_file("out.txt")
    ctx.actions.write(f, "x")
    return [DefaultInfo(files = depset([f]))]

under_test = rule(implementation = _under_test_impl)

def _test_impl(ctx):
    # The subject's output path carries its config segment; under the
    # transition it must contain "opt", not the default "fastbuild".
    paths = [f.path for f in ctx.attr.target_under_test[0][DefaultInfo].files.to_list()]
    joined = ",".join(paths)
    if "opt" not in joined:
        fail("analysis_test_transition not applied; subject path = %s" % joined)
    return [DefaultInfo()]

test_rule = rule(
    implementation = _test_impl,
    attrs = {
        "target_under_test": attr.label(cfg = _t),
        "_allowlist_function_transition": attr.label(
            default = "@bazel_tools//tools/allowlists/function_transition_allowlist",
        ),
    },
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"load(":defs.bzl", "under_test", "test_rule")
under_test(name = "subject")
test_rule(name = "t", target_under_test = ":subject")
"#,
            ),
        ],
    );
    // Success = the impl's "opt" self-check passed.
    engine(tmp.path().to_path_buf())
        .analyze_target(&label("t"))
        .unwrap();
}
