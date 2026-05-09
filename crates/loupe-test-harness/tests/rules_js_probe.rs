//! Probes Loupe's analysis against the vendored rules_js fixture.
//!
//! Set `LOUPE_RULES_JS_VENDOR` (default `/tmp/loupe-rules-js-vendor`)
//! to the directory created by the probe's `vendor.sh`. Test skips
//! silently when that directory is absent.

use std::path::PathBuf;

use loupe_core::{
    CanonicalRepo, Configuration, Engine, Label, PackageId, PackagePath, engine::EngineBuilder,
};

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("rules_js_examples_probe")
}

fn vendor_path() -> PathBuf {
    PathBuf::from(
        std::env::var("LOUPE_RULES_JS_VENDOR")
            .unwrap_or_else(|_| "/tmp/loupe-rules-js-vendor".into()),
    )
}

fn engine() -> Option<Engine> {
    let v = vendor_path();
    if !v.is_dir() {
        eprintln!("skipping: vendor dir `{}` not found", v.display());
        return None;
    }
    Some(
        EngineBuilder::new(fixture_path(), Configuration::default())
            .external_repo_root(v)
            .build()
            .expect("engine"),
    )
}

fn parse(s: &str) -> Label {
    Label::resolve(
        s,
        &PackageId::new(CanonicalRepo::MAIN, PackagePath::root()),
        &Default::default(),
    )
    .unwrap()
}

#[test]
fn rules_js_probe_targets_analyze() {
    let Some(eng) = engine() else { return };
    let targets = [
        "lib",
        "bin",
        "test",
        "run_bin",
        "devserver",
        "lib_files",
        "img_layer",
        "pkg",
        "package",
    ];
    let mut failures: Vec<(String, String)> = Vec::new();
    for t in &targets {
        let label = parse(&format!("//:{t}"));
        if let Err(e) = eng.analyze_target(&label) {
            failures.push((t.to_string(), e.to_string()));
        }
    }
    if !failures.is_empty() {
        for (t, e) in &failures {
            eprintln!("FAIL //:{t}: {e}");
        }
        panic!("{} target(s) failed analysis", failures.len());
    }
}
