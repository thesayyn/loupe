//! End-to-end tests for the Phase 6 prerequisite APIs added to bring
//! Loupe up to what `rules_oci` actually uses.

use std::fs;
use std::path::PathBuf;

use loupe_core::analysis::CapturedProvider;
use loupe_core::label::{CanonicalRepo, Label, PackagePath};
use loupe_core::{Configuration, Engine};

fn write_workspace(dir: &std::path::Path, defs_bzl: &str, build: &str) {
    fs::write(
        dir.join("MODULE.bazel"),
        r#"module(name = "demo", version = "0.0.0")"#,
    )
    .unwrap();
    fs::write(dir.join("defs.bzl"), defs_bzl).unwrap();
    fs::write(dir.join("BUILD.bazel"), build).unwrap();
}

fn label(name: &str) -> Label {
    Label::new(CanonicalRepo::MAIN, PackagePath::root(), name)
}

fn engine(dir: PathBuf) -> Engine {
    Engine::new(dir, Configuration::default()).unwrap()
}

fn field_str(p: &CapturedProvider, name: &str) -> String {
    let v = p
        .fields
        .get(name)
        .unwrap_or_else(|| panic!("provider missing field `{name}`"));
    v.to_value()
        .unpack_str()
        .unwrap_or_else(|| panic!("field `{name}` is not a string"))
        .to_string()
}

fn run_and_get_provider(defs: &str, build: &str, sym: &str) -> CapturedProvider {
    let tmp = tempfile::tempdir().unwrap();
    write_workspace(tmp.path(), defs, build);
    let analyzed = engine(tmp.path().to_path_buf())
        .analyze_target(&label("world"))
        .unwrap();
    analyzed
        .providers
        .iter()
        .find(|p| p.id.symbol == sym)
        .cloned()
        .unwrap_or_else(|| panic!("provider {sym} missing"))
}

// --------------------------------------------------------------------
// Item 1: ctx.actions.declare_directory
// --------------------------------------------------------------------

#[test]
fn declare_directory_marks_file_as_is_directory() {
    let p = run_and_get_provider(
        r#"
ShapeInfo = provider(fields = ["name", "is_dir", "is_file"])

def _impl(ctx):
    d = ctx.actions.declare_directory("treeart")
    f = ctx.actions.declare_file("plain.txt")
    return [ShapeInfo(name = d.basename, is_dir = str(d.is_directory), is_file = str(f.is_directory))]

mk = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "mk")
mk(name = "world")"#,
        "ShapeInfo",
    );
    assert_eq!(field_str(&p, "name"), "treeart");
    assert_eq!(field_str(&p, "is_dir"), "True");
    assert_eq!(field_str(&p, "is_file"), "False");
}

// --------------------------------------------------------------------
// Item 2: ctx.actions.expand_template
// --------------------------------------------------------------------

#[test]
fn expand_template_visible_via_target_actions() {
    let p = run_and_get_provider(
        r#"
SeenInfo = provider(fields = ["seen"])

def _leaf_impl(ctx):
    tmpl = ctx.actions.declare_file("tmpl.in")
    ctx.actions.write(tmpl, "hi {N}")
    out = ctx.actions.declare_file("out.txt")
    ctx.actions.expand_template(template = tmpl, output = out, substitutions = {"{N}": "x"})
    return [DefaultInfo(files = [out])]

def _mid_impl(ctx):
    rows = []
    for a in ctx.attr.dep.actions:
        rows.append(a.mnemonic + "|" + (a.outputs[0].basename if a.outputs else ""))
    return [SeenInfo(seen = ";".join(rows))]

leaf = rule(implementation = _leaf_impl)
mid  = rule(implementation = _mid_impl, attrs = {"dep": attr.label()})
"#,
        r#"load(":defs.bzl", "leaf", "mid")
leaf(name = "a")
mid(name = "world", dep = ":a")"#,
        "SeenInfo",
    );
    // Order: write tmpl, then expand_template → out.
    assert_eq!(
        field_str(&p, "seen"),
        "FileWrite|tmpl.in;ExpandTemplate|out.txt"
    );
}

// --------------------------------------------------------------------
// Item 3: ctx.actions.args() value type
// --------------------------------------------------------------------

#[test]
fn args_add_and_add_all_flatten_into_run_argv() {
    let p = run_and_get_provider(
        r#"
ArgvInfo = provider(fields = ["argv"])

def _leaf_impl(ctx):
    tool = ctx.actions.declare_file("tool.bin")
    out  = ctx.actions.declare_file("out.bin")
    ctx.actions.write(tool, "tool")
    args = ctx.actions.args()
    args.add("--flag")
    args.add("--name", format = "name=%s")
    args.add_all(["a", "b"], format_each = "--in=%s")
    args.add_joined(["x", "y", "z"], join_with = ",")
    ctx.actions.run(
        executable = tool,
        arguments = [args, "trailing"],
        outputs = [out],
        mnemonic = "Run",
    )
    return [DefaultInfo(files = [out])]

def _mid_impl(ctx):
    for a in ctx.attr.dep.actions:
        if a.mnemonic == "Run":
            return [ArgvInfo(argv = ",".join(a.argv))]
    return [ArgvInfo(argv = "<missing>")]

leaf = rule(implementation = _leaf_impl)
mid  = rule(implementation = _mid_impl, attrs = {"dep": attr.label()})
"#,
        r#"load(":defs.bzl", "leaf", "mid")
leaf(name = "a")
mid(name = "world", dep = ":a")"#,
        "ArgvInfo",
    );
    assert_eq!(
        field_str(&p, "argv"),
        "--flag,name=--name,--in=a,--in=b,x,y,z,trailing"
    );
}

#[test]
fn add_all_accepts_map_each_and_directory_kwargs() {
    // aspect_bazel_lib's mtree_spec passes
    // `add_all(values, map_each=fn, expand_directories=True,
    // uniquify=True)`. v1 doesn't apply map_each (no execution), but
    // the call must not error at the parse boundary.
    let p = run_and_get_provider(
        r#"
ArgvInfo = provider(fields = ["argv"])

def _expand(f):
    return [f]

def _leaf_impl(ctx):
    tool = ctx.actions.declare_file("tool.bin")
    out  = ctx.actions.declare_file("out.bin")
    ctx.actions.write(tool, "tool")
    args = ctx.actions.args()
    args.add_all(
        ["a", "b"],
        map_each = _expand,
        expand_directories = True,
        uniquify = True,
        omit_if_empty = False,
        terminate_with = "END",
    )
    ctx.actions.run(
        executable = tool,
        arguments = [args],
        outputs = [out],
        mnemonic = "Run",
    )
    return [DefaultInfo(files = [out])]

def _mid_impl(ctx):
    for a in ctx.attr.dep.actions:
        if a.mnemonic == "Run":
            return [ArgvInfo(argv = ",".join(a.argv))]
    return [ArgvInfo(argv = "<missing>")]

leaf = rule(implementation = _leaf_impl)
mid  = rule(implementation = _mid_impl, attrs = {"dep": attr.label()})
"#,
        r#"load(":defs.bzl", "leaf", "mid")
leaf(name = "a")
mid(name = "world", dep = ":a")"#,
        "ArgvInfo",
    );
    // map_each is ignored — values pass through stringified — but
    // terminate_with does append.
    assert_eq!(field_str(&p, "argv"), "a,b,END");
}

// --------------------------------------------------------------------
// Item 4: ctx.actions.run kwargs (tools, execution_requirements)
// --------------------------------------------------------------------

#[test]
fn run_tools_kwarg_folds_into_inputs() {
    let p = run_and_get_provider(
        r#"
InfoInfo = provider(fields = ["count"])

def _leaf_impl(ctx):
    exe  = ctx.actions.declare_file("exe.bin")
    tool = ctx.actions.declare_file("tool.bin")
    out  = ctx.actions.declare_file("o.bin")
    ctx.actions.write(exe, "")
    ctx.actions.write(tool, "")
    ctx.actions.run(
        executable = exe,
        outputs = [out],
        tools = [tool],
        execution_requirements = {"no-sandbox": "1"},
        mnemonic = "Run",
    )
    return [DefaultInfo(files = [out])]

def _mid_impl(ctx):
    for a in ctx.attr.dep.actions:
        if a.mnemonic == "Run":
            return [InfoInfo(count = str(len(a.inputs)))]
    return [InfoInfo(count = "<missing>")]

leaf = rule(implementation = _leaf_impl)
mid  = rule(implementation = _mid_impl, attrs = {"dep": attr.label()})
"#,
        r#"load(":defs.bzl", "leaf", "mid")
leaf(name = "a")
mid(name = "world", dep = ":a")"#,
        "InfoInfo",
    );
    // tools=[tool.bin] folds into inputs — recorded action has 1 input.
    assert_eq!(field_str(&p, "count"), "1");
}

// --------------------------------------------------------------------
// Item 5: ctx.runfiles(transitive_files=depset(File))
// --------------------------------------------------------------------

#[test]
fn runfiles_accepts_depset_for_transitive_files() {
    let p = run_and_get_provider(
        r#"
NamesInfo = provider(fields = ["names"])

def _impl(ctx):
    a = ctx.actions.declare_file("a.txt")
    b = ctx.actions.declare_file("b.txt")
    ctx.actions.write(a, "a")
    ctx.actions.write(b, "b")
    rf = ctx.runfiles(transitive_files = depset([a, b]))
    return [NamesInfo(names = ",".join([f.basename for f in rf.files]))]

mk = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "mk")
mk(name = "world")"#,
        "NamesInfo",
    );
    assert_eq!(field_str(&p, "names"), "a.txt,b.txt");
}

// --------------------------------------------------------------------
// Item 6: OutputGroupInfo + target.output_groups
// --------------------------------------------------------------------

#[test]
fn output_group_info_round_trips_through_target() {
    let p = run_and_get_provider(
        r#"
SeenInfo = provider(fields = ["names"])

def _leaf_impl(ctx):
    a = ctx.actions.declare_file("a.txt")
    ctx.actions.write(a, "a")
    return [DefaultInfo(files = [a]), OutputGroupInfo(extra = [a])]

def _mid_impl(ctx):
    og = ctx.attr.dep.output_groups
    names = ",".join([f.basename for f in og.extra])
    return [SeenInfo(names = names)]

leaf = rule(implementation = _leaf_impl)
mid  = rule(implementation = _mid_impl, attrs = {"dep": attr.label()})
"#,
        r#"load(":defs.bzl", "leaf", "mid")
leaf(name = "a")
mid(name = "world", dep = ":a")"#,
        "SeenInfo",
    );
    assert_eq!(field_str(&p, "names"), "a.txt");
}

// --------------------------------------------------------------------
// Item 7: rule(toolchains=[...]) → ctx.toolchains[type]
// --------------------------------------------------------------------

#[test]
fn ctx_toolchains_returns_wildcard_struct_for_any_key() {
    // When no `register_toolchains` covers the requested type (this
    // fixture has none registered), `ctx.toolchains[T]` falls back to
    // a WildcardStruct so analysis doesn't refuse to evaluate. The
    // resolved-toolchain path (real `ToolchainInfo` from a registered
    // `toolchain` rule) is exercised in `toolchain_resolution.rs`.
    let p = run_and_get_provider(
        r#"
ShapeInfo = provider(fields = ["t"])

def _impl(ctx):
    return [ShapeInfo(t = type(ctx.toolchains["@a//:b_toolchain_type"]))]

mk = rule(
    implementation = _impl,
    toolchains = ["@a//:b_toolchain_type"],
)
"#,
        r#"load(":defs.bzl", "mk")
mk(name = "world")"#,
        "ShapeInfo",
    );
    assert_eq!(field_str(&p, "t"), "wildcard_struct");
}

// --------------------------------------------------------------------
// Item 8: platform_common.PlatformInfo / ToolchainInfo symbol stubs
// --------------------------------------------------------------------

#[test]
fn platform_common_symbols_are_bound() {
    // Just verify name resolution succeeds — we don't construct
    // instances. STUB at v1.
    let p = run_and_get_provider(
        r#"
ShapeInfo = provider(fields = ["pi", "ti"])

def _impl(ctx):
    return [ShapeInfo(
        pi = type(platform_common.PlatformInfo),
        ti = type(platform_common.ToolchainInfo),
    )]

mk = rule(implementation = _impl)
"#,
        r#"load(":defs.bzl", "mk")
mk(name = "world")"#,
        "ShapeInfo",
    );
    // Our ProviderType prints as a `provider_type` Starlark type.
    assert_eq!(field_str(&p, "pi"), "provider");
    assert_eq!(field_str(&p, "ti"), "provider");
}

// --------------------------------------------------------------------
// Item 9: attr.string(values=[...]) accept-and-ignore
// --------------------------------------------------------------------

#[test]
fn attr_string_values_kwarg_is_accepted() {
    // STUB: we don't enforce the constraint. Verify the rule
    // construction succeeds and the attr round-trips.
    let p = run_and_get_provider(
        r#"
TagInfo = provider(fields = ["v"])

def _impl(ctx):
    return [TagInfo(v = ctx.attr.format)]

mk = rule(
    implementation = _impl,
    attrs = {"format": attr.string(values = ["docker", "oci"], default = "docker")},
)
"#,
        r#"load(":defs.bzl", "mk")
mk(name = "world", format = "oci")"#,
        "TagInfo",
    );
    assert_eq!(field_str(&p, "v"), "oci");
}
