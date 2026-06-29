//! `subrule()` — a subrule's impl receives a restricted ctx exposing
//! `actions` and `label`, and the actions it declares thread into the
//! consuming target (Bazel's subrule contract). Regression for the
//! subrule ctx previously omitting `actions`/`label`.

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
fn subrule_actions_thread_into_consuming_target() {
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
def _my_subrule_impl(ctx, *, prefix):
    # The subrule ctx must expose actions + label.
    out = ctx.actions.declare_file(prefix + ".txt")
    ctx.actions.write(out, "from subrule for %s" % ctx.label.name)
    return out

_my_subrule = subrule(implementation = _my_subrule_impl)

def _rule_impl(ctx):
    f = _my_subrule(prefix = ctx.label.name)
    if f == None:
        fail("subrule returned None")
    return [DefaultInfo(files = depset([f]))]

my_rule = rule(implementation = _rule_impl, subrules = [_my_subrule])
"#,
            ),
            (
                "BUILD.bazel",
                r#"load(":defs.bzl", "my_rule")
my_rule(name = "r")
"#,
            ),
        ],
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("r"))
        .unwrap();
    // The subrule's declared file is on the consuming target's outputs,
    // named after the target (proving ctx.label resolved in the subrule).
    let files: Vec<String> = analyzed.default_files().iter().map(|f| f.path()).collect();
    assert!(
        files.iter().any(|p| p.ends_with("/r.txt")),
        "subrule-declared file missing from consuming target: {files:?}"
    );
    // The subrule's write action is recorded on the consuming target.
    assert!(
        analyzed
            .actions
            .iter()
            .any(|a| matches!(&a.kind, loupe_core::ActionKind::Write { .. })),
        "subrule Write action not recorded on consuming target"
    );
}
