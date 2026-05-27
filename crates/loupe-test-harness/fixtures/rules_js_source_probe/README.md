# rules_js_source_probe

Analysis probe against the upstream `aspect-build/rules_js` source tree.
The fixture itself carries no Bazel files — `build.rs` clones the pinned
tag into `OUT_DIR/rules_js_source/<sha>/` and `tests/rules_js_source_probe.rs`
runs `Engine::analyze_workspace` against that clone.

Pin lives in `build.rs` (`RULES_JS_TAG` + `RULES_JS_SHA`). Bumping the
pin produces a fresh `OUT_DIR` subdir on next build.
