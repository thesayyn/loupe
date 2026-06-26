//! Regression guard for byte-exact cc command-line generation.
//!
//! loupe reproduces Bazel's `CppCompile` / `CppArchive` / `CppLink` argv
//! (feature flag_set expansion + variable substitution + tool-path
//! resolution) for both `cc_library` (`:add`) and `cc_binary` (`:prog`).
//! `fixtures/cc_argv/BAZEL_REFERENCE.txt` is the real `bazel cquery
//! --output=starlark` capture; loupe matches it byte-for-byte **except**
//! two deliberate, documented pins for reproducible output:
//!   - config segment: `k8-fastbuild` (loupe pins `cpu=k8`) vs the
//!     capturing host's `darwin_arm64-fastbuild`.
//!   - `-mmacosx-version-min=11.0` (pinned) vs the host SDK's `27.0`.
//!
//! This test pins loupe's exact argv (with those pins) so any drift in the
//! command-line expander surfaces immediately.
//!
//! macOS-gated: the reference is an apple-toolchain capture (`libtool`,
//! `-mmacosx-version-min`, the `cc_wrapper.sh` tool). On Linux loupe
//! resolves a different host cc toolchain and emits a different argv, so a
//! single golden can't cover both — this guards the platform the byte-exact
//! work was validated on.
#![cfg(target_os = "macos")]

use std::path::PathBuf;

use loupe_core::engine::EngineBuilder;
use loupe_core::{Configuration, Label};
use loupe_labels::{CanonicalRepo, PackageId, PackagePath};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("cc_argv")
}

fn main_pkg() -> PackageId {
    PackageId::new(CanonicalRepo::main(), PackagePath::root())
}

/// Build an engine over the cc_argv fixture, mirroring the CLI `--bzlmod`
/// defaults (cache under `.loupe/bzlmod`, BCR on) and the default target
/// platform `@@platforms//host:host` (cc rules need a configured platform).
fn build_engine() -> loupe_core::Engine {
    let ws = fixture_path();
    let cache = ws.join(".loupe").join("bzlmod");
    let host = Label::resolve("@@platforms//host:host", &main_pkg(), &Default::default())
        .expect("resolving @@platforms//host:host");
    let config = Configuration::default().with_target_platform(host);
    EngineBuilder::new(ws, config)
        .with_bzlmod_cache(cache)
        .with_bcr()
        .build()
        .expect("Engine::build on cc_argv fixture")
}

fn argv_for(engine: &loupe_core::Engine, target: &str, mnemonic: &str) -> String {
    let label = Label::resolve(target, &main_pkg(), &Default::default())
        .unwrap_or_else(|e| panic!("label {target}: {e}"));
    let expr = format!("[a.argv for a in target.actions if a.mnemonic == \"{mnemonic}\"]");
    engine
        .cquery_expr(&label, &expr)
        .unwrap_or_else(|e| panic!("cquery_expr {target} {mnemonic}: {e:#}"))
        .trim()
        .to_string()
}

#[test]
fn cpp_compile_argv_is_byte_exact_modulo_pins() {
    let engine = build_engine();
    let got = argv_for(&engine, "@@//:add", "CppCompile");
    let expected = r#"[["external/rules_cc++cc_configure_extension+local_config_cc/cc_wrapper.sh", "-U_FORTIFY_SOURCE", "-fstack-protector", "-Wall", "-Wthread-safety", "-Wself-assign", "-Wunused-but-set-parameter", "-Wno-free-nonheap-object", "-fcolor-diagnostics", "-fno-omit-frame-pointer", "-std=c++17", "-frandom-seed=bazel-out/k8-fastbuild/bin/_objs/add/add.o", "-mmacosx-version-min=11.0", "-MD", "-MF", "bazel-out/k8-fastbuild/bin/_objs/add/add.d", "-iquote", ".", "-iquote", "bazel-out/k8-fastbuild/bin", "-c", "add.cc", "-o", "bazel-out/k8-fastbuild/bin/_objs/add/add.o", "-no-canonical-prefixes", "-Wno-builtin-macro-redefined", "-D__DATE__=\"redacted\"", "-D__TIMESTAMP__=\"redacted\"", "-D__TIME__=\"redacted\""]]"#;
    assert_eq!(
        got, expected,
        "CppCompile argv drifted from the pinned byte-exact reference"
    );
}

#[test]
fn cpp_archive_argv_is_byte_exact_modulo_pins() {
    let engine = build_engine();
    let got = argv_for(&engine, "@@//:add", "CppArchive");
    let expected = r#"[["/usr/bin/libtool", "-D", "-no_warning_for_no_symbols", "-static", "-o", "bazel-out/k8-fastbuild/bin/libadd.a", "bazel-out/k8-fastbuild/bin/_objs/add/add.o"]]"#;
    assert_eq!(
        got, expected,
        "CppArchive argv drifted from the pinned byte-exact reference"
    );
}

// `:prog` is a `cc_binary` (deps on `:add`): exercises the executable
// path — compiling its own src and the `CppLink` command-line. Same pins
// (k8-fastbuild segment, -mmacosx-version-min=11.0); the link argv is
// loupe's validated byte-exact output for the apple host toolchain.
#[test]
fn cc_binary_compile_argv_is_byte_exact_modulo_pins() {
    let engine = build_engine();
    let got = argv_for(&engine, "@@//:prog", "CppCompile");
    let expected = r#"[["external/rules_cc++cc_configure_extension+local_config_cc/cc_wrapper.sh", "-U_FORTIFY_SOURCE", "-fstack-protector", "-Wall", "-Wthread-safety", "-Wself-assign", "-Wunused-but-set-parameter", "-Wno-free-nonheap-object", "-fcolor-diagnostics", "-fno-omit-frame-pointer", "-std=c++17", "-frandom-seed=bazel-out/k8-fastbuild/bin/_objs/prog/main.o", "-mmacosx-version-min=11.0", "-MD", "-MF", "bazel-out/k8-fastbuild/bin/_objs/prog/main.d", "-iquote", ".", "-iquote", "bazel-out/k8-fastbuild/bin", "-iquote", "external/rules_cc+", "-iquote", "bazel-out/k8-fastbuild/bin/external/rules_cc+", "-c", "main.cc", "-o", "bazel-out/k8-fastbuild/bin/_objs/prog/main.o", "-no-canonical-prefixes", "-Wno-builtin-macro-redefined", "-D__DATE__=\"redacted\"", "-D__TIMESTAMP__=\"redacted\"", "-D__TIME__=\"redacted\""]]"#;
    assert_eq!(
        got, expected,
        "cc_binary CppCompile argv drifted from the pinned byte-exact reference"
    );
}

#[test]
fn cc_binary_link_argv_is_byte_exact_modulo_pins() {
    let engine = build_engine();
    let got = argv_for(&engine, "@@//:prog", "CppLink");
    let expected = r#"[["external/rules_cc++cc_configure_extension+local_config_cc/cc_wrapper.sh", "-o", "bazel-out/k8-fastbuild/bin/prog", "bazel-out/k8-fastbuild/bin/_objs/prog/main.o", "bazel-out/k8-fastbuild/bin/libadd.a", "-mmacosx-version-min=11.0", "-no-canonical-prefixes", "-fobjc-link-runtime", "-headerpad_max_install_names", "-lc++", "-lm"]]"#;
    assert_eq!(
        got, expected,
        "cc_binary CppLink argv drifted from the pinned byte-exact reference"
    );
}
