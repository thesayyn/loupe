//! Provider identity invariants from DESIGN.md:
//! > Provider types are singletons by call site. Identity is
//! > `FrozenValue` pointer equality, established when `provider()`
//! > is called in a `.bzl`. Two `.bzl` files defining their own
//! > `MyInfo` produce distinct types even with identical fields.
//!
//! Patterned after Bazel's `StarlarkProviderTest.java`.

use std::fs;
use std::path::{Path, PathBuf};

use loupe_core::analysis::CapturedProvider;
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

fn find_provider<'a>(target: &'a loupe_core::AnalyzedTarget, symbol: &str) -> &'a CapturedProvider {
    target
        .providers
        .iter()
        .find(|p| p.id.symbol == symbol)
        .unwrap_or_else(|| panic!("provider `{symbol}` missing in {:?}", target.label))
}

#[test]
fn two_providers_with_same_name_in_distinct_bzl_files_have_distinct_ids() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        &[
            (
                "MODULE.bazel",
                r#"module(name = "demo", version = "0.0.0")"#,
            ),
            (
                // First .bzl defines its own `Info`.
                "a.bzl",
                r#"
Info = provider(fields = ["v"])
def _impl(ctx):
    return [Info(v = "from-a")]
a_rule = rule(implementation = _impl)
"#,
            ),
            (
                // Second .bzl defines its own `Info` — same name,
                // identical field set, but distinct identity by DESIGN.md.
                "b.bzl",
                r#"
Info = provider(fields = ["v"])
def _impl(ctx):
    return [Info(v = "from-b")]
b_rule = rule(implementation = _impl)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":a.bzl", "a_rule")
load(":b.bzl", "b_rule")
a_rule(name = "ta")
b_rule(name = "tb")
"#,
            ),
        ],
    );
    let eng = engine(tmp.path().to_path_buf());
    let ta = eng.analyze_target(&label("ta")).unwrap();
    let tb = eng.analyze_target(&label("tb")).unwrap();
    let pa = find_provider(&ta, "Info");
    let pb = find_provider(&tb, "Info");
    // Both providers have symbol "Info" but the `defining_bzl` field
    // differs — that's the canonical identity discriminator.
    assert_eq!(pa.id.symbol, "Info");
    assert_eq!(pb.id.symbol, "Info");
    assert_ne!(
        pa.id.defining_bzl, pb.id.defining_bzl,
        "distinct .bzl files must produce distinct provider identities"
    );
}

#[test]
fn two_targets_using_same_provider_share_identity() {
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
Info = provider(fields = ["v"])
def _impl(ctx):
    return [Info(v = ctx.attr.name)]
my_rule = rule(implementation = _impl)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "my_rule")
my_rule(name = "first")
my_rule(name = "second")
"#,
            ),
        ],
    );
    let eng = engine(tmp.path().to_path_buf());
    let t1 = eng.analyze_target(&label("first")).unwrap();
    let t2 = eng.analyze_target(&label("second")).unwrap();
    let p1 = find_provider(&t1, "Info");
    let p2 = find_provider(&t2, "Info");
    // Both come from the same defs.bzl — `id` must compare equal.
    assert_eq!(p1.id, p2.id, "same defining .bzl ⇒ same provider id");
}

#[test]
fn provider_field_unknown_to_schema_is_rejected() {
    // `provider(fields=["a", "b"])` declares a closed schema. Passing
    // an unknown field name on instantiation must error.
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
Info = provider(fields = ["a"])
def _impl(ctx):
    # `b` is not in the schema — should raise.
    return [Info(a = "x", b = "y")]
mk = rule(implementation = _impl)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "mk")
mk(name = "world")
"#,
            ),
        ],
    );
    let err = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .expect_err("unknown provider field should raise");
    let msg = format!("{err:#}");
    assert!(
        msg.to_lowercase().contains("field") || msg.contains("b"),
        "error should mention the unknown field: {msg}"
    );
}

#[test]
fn provider_with_init_uses_two_tuple_unpacking_form() {
    // `provider(init=fn)` returns a 2-tuple: (public_constructor,
    // raw_constructor). Vendored Bazel uses this for CcInfo,
    // DefaultInfo, etc. (see `builtins_core/exports.bzl`). The init
    // function transforms kwargs and returns a dict whose entries
    // become the instance's fields.
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
def _init(value):
    return {"value": value + "-doubled"}
Info, _InfoRaw = provider(fields = ["value"], init = _init)
def _impl(ctx):
    return [Info(value = "raw")]
mk = rule(implementation = _impl)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "mk")
mk(name = "world")
"#,
            ),
        ],
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = find_provider(&analyzed, "Info");
    let v = p
        .fields
        .get("value")
        .unwrap()
        .to_value()
        .unpack_str()
        .unwrap()
        .to_string();
    assert_eq!(
        v, "raw-doubled",
        "init transformed the value before storage"
    );
}

#[test]
fn provider_without_field_list_accepts_any_field_name() {
    // `provider()` with no `fields=` declaration accepts any kwargs.
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
Info = provider()
def _impl(ctx):
    return [Info(anything = "yes", whatever = "ok")]
mk = rule(implementation = _impl)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "mk")
mk(name = "world")
"#,
            ),
        ],
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = find_provider(&analyzed, "Info");
    assert!(p.fields.contains_key("anything"));
    assert!(p.fields.contains_key("whatever"));
}

#[test]
fn target_provider_lookup_via_brackets_works_for_user_providers() {
    // `target[Info]` is the canonical lookup form. The dep rule emits
    // Info; the consumer reads Info via the bracket syntax and copies
    // the field through into its own provider.
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
Info = provider(fields = ["v"])
def _producer_impl(ctx):
    return [Info(v = "from-producer")]
def _consumer_impl(ctx):
    dep_value = ctx.attr.dep[Info].v
    return [Info(v = "got: " + dep_value)]
producer = rule(implementation = _producer_impl)
consumer = rule(
    implementation = _consumer_impl,
    attrs = {"dep": attr.label()},
)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "producer", "consumer")
producer(name = "src")
consumer(name = "world", dep = ":src")
"#,
            ),
        ],
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = find_provider(&analyzed, "Info");
    let v = p
        .fields
        .get("v")
        .unwrap()
        .to_value()
        .unpack_str()
        .unwrap()
        .to_string();
    assert_eq!(v, "got: from-producer");
}

#[test]
fn provider_fields_accepts_a_tuple_of_strings() {
    // Bazel's `provider(fields=...)` param is a `Sequence`, so a tuple
    // of field names is as valid as a list. Loupe used to reject it.
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
Info = provider(fields = ("a", "b"))
def _impl(ctx):
    return [Info(a = "first", b = "second")]
mk = rule(implementation = _impl)
"#,
            ),
            (
                "BUILD.bazel",
                r#"
load(":defs.bzl", "mk")
mk(name = "world")
"#,
            ),
        ],
    );
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    let p = find_provider(&analyzed, "Info");
    let field = |name: &str| {
        p.fields
            .get(name)
            .unwrap()
            .to_value()
            .unpack_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(field("a"), "first");
    assert_eq!(field("b"), "second");
}
