//! Vendor the aspect-build/rules_js source tree for the
//! `rules_js_source_probe` integration test.
//!
//! Clones a pinned tag into `OUT_DIR/rules_js_source/<sha>/` and
//! exposes the path via the `RULES_JS_SOURCE_PATH` env var. The
//! per-SHA subdir makes a pin bump produce a fresh clone instead of
//! reusing a stale checkout.

use std::path::PathBuf;
use std::process::Command;

const RULES_JS_TAG: &str = "v3.1.2";
const RULES_JS_SHA: &str = "eb8c7ff15a861fb6dac8e28b040cbd9238834100";
const RULES_JS_REPO: &str = "https://github.com/aspect-build/rules_js.git";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"));
    let dest = out_dir.join("rules_js_source").join(RULES_JS_SHA);

    if !is_clone_at_pinned_sha(&dest) {
        if dest.exists() {
            std::fs::remove_dir_all(&dest).expect("removing stale rules_js clone");
        }
        std::fs::create_dir_all(dest.parent().unwrap()).expect("creating rules_js clone parent");
        clone_pinned(&dest);
        let actual = head_sha(&dest);
        assert_eq!(
            actual.trim(),
            RULES_JS_SHA,
            "rules_js tag `{RULES_JS_TAG}` resolved to `{actual}`, expected `{RULES_JS_SHA}` — \
             the tag has moved or the pin is wrong",
        );
    }

    println!(
        "cargo:rustc-env=RULES_JS_SOURCE_PATH={}",
        dest.to_string_lossy()
    );
}

fn is_clone_at_pinned_sha(dest: &std::path::Path) -> bool {
    if !dest.join(".git").exists() {
        return false;
    }
    head_sha(dest).trim() == RULES_JS_SHA
}

fn head_sha(dest: &std::path::Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dest)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("running `git rev-parse HEAD` on rules_js clone");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn clone_pinned(dest: &std::path::Path) {
    let status = Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            "--branch",
            RULES_JS_TAG,
            RULES_JS_REPO,
        ])
        .arg(dest)
        .status()
        .expect("running `git clone` for rules_js source");
    assert!(
        status.success(),
        "git clone of rules_js {RULES_JS_TAG} failed (status: {status})"
    );
}
