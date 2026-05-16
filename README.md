Bazel compatible analysis engine built on top of DICE


Run it for yourself:

```
pushd crates/loupe-test-harness/fixtures/rules_js_examples_probe;
cargo run -p loupe-cli -- cquery . --platforms=//:host --script query.bzl;
popd
```