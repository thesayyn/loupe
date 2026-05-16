//! Bazel `builtins.bzl`-style compat surface, in user-space Starlark.
//!
//! Bazel ships a private ["builtins
//! zip"](https://github.com/bazelbuild/bazel/blob/master/src/main/starlark/builtins_bzl/README.md)
//! whose `.bzl` files define the rule constructors, provider types,
//! and helper namespaces (`coverage_common`, `platform_common`, …)
//! that user `.bzl`s reference as if they were native globals. Loupe
//! mirrors that pattern: keep the bottom primitives (`provider`,
//! `attr.*`, `rule`, `ctx.actions.*`, `File`, `depset`) in the host
//! (Rust), and layer Bazel-compatibility behaviour on top in
//! `bzl/wrappers.bzl` (root) plus a tree of sub-files under
//! `@_builtins//…`. New compat surface lands as a `.bzl` change.
//!
//! ## Vendored upstream
//!
//! Loupe vendors Bazel's actual
//! `src/main/starlark/builtins_bzl/` tree verbatim alongside its own
//! Loupe-authored compat files. The vendored files reach for
//! `_builtins.toplevel.*` and `_builtins.internal.*` — Bazel's
//! magic injection namespace. `_builtins.toplevel` is a strict
//! `AllocStruct` snapshot of `wrappers.bzl::builtins_namespace.toplevel`
//! (real primitives + explicit stubs); `_builtins.internal` is a
//! [`WildcardStruct`](crate::starlark_types::wildcard::WildcardStruct)
//! that returns more wildcards on any access/call so vendored file
//! bodies which read the host-Java machinery (`cc_internal`,
//! `py_builtins`, …) freeze without per-name stubs.
//!
//! ## Multi-file embedded tree
//!
//! Files are listed in [`EMBEDDED_FILES`] in topological order —
//! each file's `load()` statements may only reference earlier
//! entries. The loader evaluates them in list order, caching each
//! `FrozenModule` so the next file's `load(...)` sees a populated
//! [`EmbeddedLoader`] map. Adding a new file is a one-line append
//! to that list (assuming dependency-respecting position).
//!
//! Bazel's vendored files use a mix of load-path forms:
//!   - `@_builtins//common/foo.bzl`
//!   - `@_builtins//:common/foo.bzl`
//!   - `:common/foo.bzl` (root-relative, used inside the builtins tree)
//!
//! The [`EmbeddedLoader`] normalises all three to the same canonical
//! key (the file's [`EMBEDDED_FILES`] path).
//!
//! ## Lifecycle
//!
//! 1. [`embedded_builtins`] runs once (OnceLock-cached): walks
//!    [`EMBEDDED_FILES`] in order, evaluates each against the *base*
//!    `bzl_globals` plus the previously-cached frozen modules, and
//!    returns the root `wrappers.bzl` frozen module. Each module
//!    gets `_builtins` injected before eval so vendored files'
//!    top-level `_builtins.toplevel.X` accesses succeed.
//!
//! 2. [`inject_builtins_into`] is called from each user-`.bzl` and
//!    BUILD evaluator before user code runs. It anchors the cached
//!    builtins frozen heap on the user module's frozen heap, copies
//!    each top-level name from `wrappers.bzl` into the user module via
//!    [`Module::set`], and injects the same `_builtins` magic
//!    namespace. The user code then sees those names as if they were
//!    native globals.

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::{Result, anyhow};
use starlark::collections::SmallMap;
use starlark::environment::{FrozenModule, Globals, Module};
use starlark::eval::Evaluator;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::Value;
use starlark::values::ValueLike;
use starlark::values::structs::AllocStruct;

use crate::starlark_types::wildcard::WildcardStruct;

/// The embedded builtins tree, in topological evaluation order.
///
/// Each entry is `(path, source)` where `path` is the
/// `@_builtins//<path>` virtual location and `source` is the
/// compiled-in `.bzl` text. Files later in the list may `load()`
/// from earlier entries; the first entry must be self-contained.
///
/// `wrappers.bzl` is by convention the *root* whose top-level
/// bindings get auto-injected into every user `.bzl` / BUILD module.
/// Sub-files exist to keep `wrappers.bzl` thin; they're not
/// auto-injected unless re-exported by `wrappers.bzl`.
///
/// Per-repo embedded trees. Each top-level directory under
/// `crates/loupe-core/src/starlark/` is its own `include_dir!`
/// constant — one per canonical repo. The engine registers each as a
/// `RepoRoot::Embedded { canonical, dir: &<CONST> }`; that one repo's
/// load path resolves through the embedded tree without affecting
/// any other canonical.
///
/// Primary roots ([`primary_embedded_roots`]) go FIRST in the
/// precedence chain — they shadow on-disk versions of the same
/// canonical so vendored Bazel-side fixups stay reproducible.
/// Fallback roots ([`fallback_embedded_roots`]) go LAST — they're
/// stubs the engine ships for canonicals (`@bazel_skylib`) that
/// vendored content depends on but user workspaces don't always
/// declare; a real on-disk skylib in the middle of the chain wins
/// over the fallback.
pub(crate) static EMBEDDED_BAZEL_TOOLS: include_dir::Dir<'_> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/src/starlark/bazel_tools");
pub(crate) static EMBEDDED_BUILTINS_CORE: include_dir::Dir<'_> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/src/starlark/builtins_core");
pub(crate) static EMBEDDED_BUILTINS_BZL: include_dir::Dir<'_> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/src/starlark/builtins_bzl");
pub(crate) static EMBEDDED_BAZEL_SKYLIB: include_dir::Dir<'_> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/src/starlark/bazel_skylib+");

/// `(canonical, dir)` for every embedded repo, in iteration order.
/// `embedded_bzl_files` walks these to build the `.bzl` source map;
/// [`primary_embedded_roots`] and [`fallback_embedded_roots`] use
/// the same per-repo constants when constructing `RepoRoot::Embedded`
/// entries.
const EMBEDDED_REPOS: &[(&str, &include_dir::Dir<'_>)] = &[
    ("bazel_tools", &EMBEDDED_BAZEL_TOOLS),
    ("builtins_core", &EMBEDDED_BUILTINS_CORE),
    ("builtins_bzl", &EMBEDDED_BUILTINS_BZL),
    ("bazel_skylib+", &EMBEDDED_BAZEL_SKYLIB),
];

/// `RepoRoot::Embedded` entries the engine puts FIRST in the
/// precedence chain. Everything except fallback stubs.
pub(crate) fn primary_embedded_roots() -> Vec<crate::repo_root::RepoRoot> {
    EMBEDDED_REPOS
        .iter()
        .filter(|(c, _)| *c != "bazel_skylib+")
        .map(|(c, d)| crate::repo_root::RepoRoot::Embedded {
            canonical: c,
            dir: d,
        })
        .collect()
}

/// `RepoRoot::Embedded` entries the engine puts LAST — only those
/// the user is expected to potentially shadow with a real on-disk
/// copy.
pub(crate) fn fallback_embedded_roots() -> Vec<crate::repo_root::RepoRoot> {
    EMBEDDED_REPOS
        .iter()
        .filter(|(c, _)| *c == "bazel_skylib+")
        .map(|(c, d)| crate::repo_root::RepoRoot::Embedded {
            canonical: c,
            dir: d,
        })
        .collect()
}

/// Iterator over every `.bzl` file in the embedded tree, yielding
/// `(canonical_path, source)` pairs where `canonical_path` is the
/// virtual `<canonical>/<rel>` key the embedded loader uses
/// internally (e.g. `builtins_core/wrappers.bzl`).
fn embedded_bzl_files() -> impl Iterator<Item = (String, &'static str)> {
    fn walk<'a>(
        dir: &'a include_dir::Dir<'a>,
    ) -> Box<dyn Iterator<Item = &'a include_dir::File<'a>> + 'a> {
        Box::new(dir.files().chain(dir.dirs().flat_map(|d| walk(d))))
    }
    EMBEDDED_REPOS.iter().flat_map(|(canonical, dir)| {
        walk(dir).filter_map(move |f| {
            let rel = f.path().to_str()?;
            if !rel.ends_with(".bzl") {
                return None;
            }
            Some((format!("{canonical}/{rel}"), f.contents_utf8()?))
        })
    })
}

#[rustfmt::skip]

/// Engine-bridge root: defines `invoke_rule`, the indirection through
/// which the analysis driver dispatches every rule impl. Loaded
/// eagerly so [`invoke_rule_callable`] never has to evaluate it on
/// demand.
const WRAPPERS_PATH: &str = "builtins_core/wrappers.bzl";

/// Bazel-standard exports root: defines `exported_rules`,
/// `exported_toplevels`, and `_builtins` (with strict `toplevel` /
/// `internal` sub-structs). Its `exported_toplevels` and
/// `exported_rules` entries become globals on every user `.bzl` /
/// BUILD module via [`inject_builtins_into`].
///
/// Self-contained (no `load()`s) — evaluates in a single pass with
/// the freeze-time wildcard scaffolding from
/// [`bind_freeze_time_wildcards`].
const EXPORTS_PATH: &str = "builtins_core/exports.bzl";

/// Bazel `builtins_bzl/exports.bzl` aggregator — unions
/// `common/exports.bzl` (cc/objc/xcode/fdo rule constructors and
/// `CcSharedLibraryInfo`/`cc_common`/`apple_common` providers) with
/// `bazel/exports.bzl` (`py_internal`, `java_common`). Walked by
/// [`inject_builtins_into`] as a SECOND source of toplevels and
/// rules — mirrors bonanza's `getBzlFileBuiltins` (configurable list
/// of builtins modules). Loupe's list is `[builtins_core, builtins_bzl]`.
const BUILTINS_BZL_EXPORTS_PATH: &str = "builtins_bzl/exports.bzl";

/// Loupe-specific extras roots (auto-injected on every user `.bzl` /
/// BUILD module). Currently empty — Loupe keeps
/// free of bridge `.bzl` files:
///
/// - `BuildSettingInfo` comes from `@bazel_skylib//rules:common_settings.bzl`
///   when a workspace depends on skylib (which most Bazel
///   workspaces do).
/// - `alias` / `label_flag` / `label_setting` are Rust-side BUILD
///   globals registered via `register_for_build` in
///   [`crate::starlark_types::native_rules`].
/// - The vendored `builtins_core/exports.bzl` provides `cc_common`,
///   `coverage_common`, provider types, etc.
///
/// Kept as an allowlist for future Loupe-authored extras; today it
/// has no entries.
const LOUPE_EXTRAS_PATHS: &[(&str, &[&str])] = &[];

/// Lazily-grown cache for evaluated embedded `.bzl` modules. Keyed by
/// canonical embedded path (e.g. `builtins_core/wrappers.bzl`).
///
/// Two-phase initialization: `evaluate_embedded_tree` populates it on
/// first call with the two roots ([`WRAPPERS_PATH`], [`EXPORTS_PATH`])
/// and their transitive `load()` chain. Subsequent lookups via
/// [`embedded_module_by_path`] can request additional vendored files
/// — they get evaluated lazily and inserted under a lock.
static CACHED: OnceLock<std::sync::Mutex<HashMap<String, FrozenModule>>> = OnceLock::new();

/// Build the globals `wrappers.bzl` is evaluated against. This is
/// the same surface user `.bzl` files see (`provider`, `attr.*`,
/// `struct`, etc.) — wrappers.bzl is itself a `.bzl` file, just one
/// we ship inside loupe-core.
///
/// Kept private so the cache always uses the same globals identity:
/// builtins evaluation is deterministic regardless of which call
/// site (BUILD vs `.bzl`) first triggers it.
fn wrappers_globals() -> Globals {
    use starlark::environment::{GlobalsBuilder, LibraryExtension::*};
    // `Json` library extension intentionally omitted — we register
    // our own `json` namespace via `bzl_eval` that adds
    // `encode_indent` / `indent` (vendored upstream files call both).
    GlobalsBuilder::extended_by(&[
        StructType,
        RecordType,
        EnumType,
        NamespaceType,
        Map,
        Filter,
        Partial,
        Debug,
        Print,
        Pprint,
        Pstr,
        Prepr,
        Typing,
        SetType,
    ])
    .with(crate::starlark_types::register_for_bzl)
    .with(loupe_bzlmod::register_for_bzlmod)
    .with_namespace("json", crate::bzl_eval::bazel_json_namespace)
    .build()
}

/// Allocate `_builtins = struct(toplevel=…, internal=…)` on
/// `module`'s heap and bind it under the `_builtins` name.
///
/// During embedded-tree evaluation (before `wrappers.bzl` is built)
/// the namespace is pure-wildcard: vendored Bazel files reach for
/// `_builtins.{toplevel,internal}.X` only at module-eval time for
/// names like `cc_internal`, `py_builtins`, `exec_transition` —
/// none of which Loupe models — and a [`WildcardStruct`] returns
/// another wildcard from any access/call so the bodies parse + freeze.
///
/// During user-side [`inject_builtins_into`], we have the cached
/// `wrappers.bzl` module which exposes a `builtins_namespace` struct
/// holding the real toplevel primitives (`provider`, `rule`,
/// `DefaultInfo`, …). The `toplevel` side is strict: every name
/// reachable through `_builtins.toplevel` must be present in
/// `wrappers.bzl`'s `builtins_namespace.toplevel` or the access
/// raises. Vendored Bazel files that need a magic name
/// (`exec_transition`, `proto_common_do_not_use`, …) get an explicit
/// stub there.
///
/// `_builtins.internal` is still a `WildcardStruct` — the host-Java
/// machinery vendored files reach for through `internal`
/// (`cc_internal`, `py_builtins`, …) has no clean stub story yet.
fn inject_builtins_magic(module: &Module, namespace: Option<&FrozenModule>) {
    let (toplevel, internal) = match namespace {
        Some(ns) => (
            alloc_strict_from_namespace(module.heap(), ns, "toplevel"),
            // Internal still falls through to wildcard for unmodelled
            // names — vendored Bazel files reach for cc_internal /
            // py_builtins / apple_common / java_common_internal_do_not_use
            // and the per-name stub story isn't designed yet.
            module.heap().alloc(WildcardStruct),
        ),
        None => (
            module.heap().alloc(WildcardStruct),
            module.heap().alloc(WildcardStruct),
        ),
    };
    let pairs: SmallMap<&str, Value<'_>> = [("toplevel", toplevel), ("internal", internal)]
        .into_iter()
        .collect();
    let value = module.heap().alloc(AllocStruct(pairs));
    module.set("_builtins", value);

    // `native` is a Rust-registered BUILD global (see
    // `crate::starlark_types::native_rules`) — but the *embedded*
    // tree still needs the bare name to freeze: vendored files like
    // `skylib/versions.bzl` read `native.bazel_version` inside
    // function bodies, and Starlark static name resolution requires
    // a binding even when the body never runs. Before `wrappers.bzl`
    // is built (`namespace=None`), bind `native` to a
    // [`WildcardStruct`] so freeze succeeds; user `.bzl` execution
    // sees the real BUILD-global `native.*` set instead.
    if namespace.is_none() {
        let native = module.heap().alloc(WildcardStruct);
        module.set("native", native);
    }
}

/// Pull `builtins_namespace.<key>` out of `wrappers.bzl`'s frozen
/// module, snapshot its struct fields into a strict `AllocStruct`,
/// and allocate it on `heap`. Strict: unknown attribute access raises
/// (no fall-through to wildcard).
///
/// If `builtins_namespace` is missing or shaped unexpectedly we fall
/// back to a `WildcardStruct` once — that's a "wrappers.bzl is
/// broken" signal, not a runtime fall-through.
fn alloc_strict_from_namespace<'v>(
    heap: starlark::values::Heap<'v>,
    namespace_module: &FrozenModule,
    key: &str,
) -> Value<'v> {
    let owned = match namespace_module.get_option("builtins_namespace") {
        Ok(Some(v)) => v,
        _ => return heap.alloc(WildcardStruct),
    };
    let namespace = unsafe { owned.unchecked_frozen_value() };
    let inner = match namespace.to_value().get_attr(key, heap) {
        Ok(Some(v)) => v,
        _ => return heap.alloc(WildcardStruct),
    };
    // `inner` is a frozen `Struct`; iterate its fields via `dir_attr`
    // + `get_attr` (no public iterator API, but this round-trip is
    // cheap and one-shot).
    let mut pairs: Vec<(String, Value<'v>)> = Vec::new();
    for name in inner.dir_attr() {
        if let Ok(Some(v)) = inner.get_attr(&name, heap) {
            pairs.push((name, v));
        }
    }
    heap.alloc(AllocStruct(pairs))
}

// `FrozenModule::dupe` is provided by `dupe::Dupe`.
use dupe::Dupe;

/// Bind a generous set of Bazel host primitives as
/// `WildcardStruct` placeholders before evaluating an embedded file.
///
/// Starlark-rust does static name resolution at freeze time — any
/// bare-name reference must resolve to a binding even if the
/// containing function body never runs. The vendored tree reaches
/// for ~20 host-provided globals (`config`, `native`,
/// `config_common`, `coverage_common`, …); rather than threading
/// every one through `register_for_bzl`, bind them all here as
/// wildcards.
///
/// User `.bzl` execution doesn't go through this path — it sees
/// real values via [`inject_builtins_into`]. This is freeze-time
/// only.
fn bind_freeze_time_wildcards(module: &Module) {
    const NAMES: &[&str] = &[
        // Build-setting / configuration primitives.
        "config",
        "config_common",
        "configuration_field",
        // Native rule namespace (user-space `native` shadows this on
        // the user side; embedded files just need the name to resolve).
        "native",
        // Toplevel helpers the host normally registers.
        "coverage_common",
        "platform_common",
        "cc_common",
        "apple_common",
        "java_common",
        "proto_common",
        "proto_common_do_not_use",
        "testing",
        // Configuration value sentinel.
        "Configuration",
        // Common transition / aspect placeholders.
        "exec_transition",
        "transition",
        "analysis_test_transition",
        "exec_group",
        "subrule",
        "macro",
        "tag_class",
        "module_extension",
        "repository_rule",
        "use_extension",
        "use_repo",
        "bazel_dep",
        "module",
        "register_toolchains",
        "register_execution_platforms",
        "single_version_override",
        "archive_override",
        "git_override",
        "local_path_override",
        "include",
        "use_repo_rule",
        "multiple_version_override",
        "override_repo",
        "inject_repo",
        "package",
        "package_group",
        "licenses",
        "visibility",
        "environment_group",
        // Builtins commonly bound on `_builtins.toplevel.X`.
        "exports_files",
        "glob",
        // Top-level providers that Bazel auto-injects on every
        // embedded `.bzl` so vendored cc/objc/xcode files can
        // reference them bare (`if DefaultInfo in attr_value`, etc.).
        // User-side `.bzl` modules see real values via
        // `inject_builtins_into`; embedded files freeze through this
        // wildcard placeholder.
        "DefaultInfo",
        "OutputGroupInfo",
        "RunEnvironmentInfo",
        "InstrumentedFilesInfo",
        "CcInfo",
        "CcToolchainConfigInfo",
        "CcSharedLibraryInfo",
        "CcSharedLibraryHintInfo",
        "DebugPackageInfo",
        "JavaInfo",
        "JavaPluginInfo",
        "PackageSpecificationInfo",
        "ProguardSpecProvider",
        "PyInfo",
        "AnalysisFailureInfo",
        "AnalysisTestResultInfo",
        "PlatformInfo",
        "ToolchainInfo",
        "ConstraintValueInfo",
        "ConstraintSettingInfo",
        "TemplateVariableInfo",
        "FeatureFlagInfo",
        "ExecutionInfo",
        "BuildSettingInfo",
    ];
    for name in NAMES {
        let v = module.heap().alloc(WildcardStruct);
        module.set(name, v);
    }
}

/// Source map: canonical embedded path → `.bzl` text. Built once
/// from [`EMBEDDED_DIR`].
fn source_map() -> &'static HashMap<String, &'static str> {
    static MAP: OnceLock<HashMap<String, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| embedded_bzl_files().collect())
}

/// Resolve a Bazel-style `load(...)` path used inside an embedded
/// file to the canonical [`EMBEDDED_FILES`] key.
///
/// Forms handled:
///
/// - `@<module>//<pkg>:<file>` — apparent repo `module` is mapped to
///   the top-level directory of the same name under `src/starlark/`.
/// - `//<pkg>:<file>` — module-relative; `pkg` is resolved against
///   the *calling* file's module (top-level directory).
/// - `:<file>` — same-package; resolved against the calling file's
///   directory.
/// - `<pkg>/<file>` or bare `<file>` (no leading sigil) — treated as
///   relative to the calling file's package.
///
/// `current_file` is the canonical key of the file doing the load.
fn resolve_load(current_file: &str, load_str: &str) -> Result<String> {
    // Apparent module: top-level directory of `current_file`.
    let current_module = current_file
        .split_once('/')
        .map(|(m, _)| m)
        .unwrap_or(current_file);
    // Package directory of `current_file` (relative to its module
    // root, so the key is `<module>/<pkg>/<file>` shape).
    let current_pkg_dir = current_file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");

    if let Some(rest) = load_str.strip_prefix("@@") {
        return resolve_apparent_module_load(rest, "@@");
    }
    if let Some(rest) = load_str.strip_prefix('@') {
        return resolve_apparent_module_load(rest, "@");
    }
    if let Some(rest) = load_str.strip_prefix("//") {
        // `//<pkg>:<file>` relative to `current_module`. `<pkg>` may
        // be empty when the form is `//:file`.
        let (pkg, file) = rest.split_once(':').unwrap_or(("", rest));
        return Ok(if pkg.is_empty() {
            format!("{current_module}/{file}")
        } else {
            format!("{current_module}/{pkg}/{file}")
        });
    }
    if let Some(rest) = load_str.strip_prefix(':') {
        // `:<file>` — same package as the loading file. Inside
        // `builtins_bzl/` the entire tree is conventionally one
        // package (no sub-BUILD files), so the colon form is
        // module-relative; for other modules (`bazel_tools/` which
        // does have sub-packages) we fall back to directory-relative
        // resolution.
        if current_module == "builtins_bzl" {
            return Ok(format!("{current_module}/{rest}"));
        }
        return Ok(if current_pkg_dir.is_empty() {
            rest.to_string()
        } else {
            format!("{current_pkg_dir}/{rest}")
        });
    }
    // Bare relative path: treat as same-package.
    Ok(if current_pkg_dir.is_empty() {
        load_str.to_string()
    } else {
        format!("{current_pkg_dir}/{load_str}")
    })
}

fn resolve_apparent_module_load(rest: &str, prefix: &str) -> Result<String> {
    // `<module>//<pkg>:<file>` or `<module>//<file>` (no colon).
    let (module, after) = rest.split_once("//").ok_or_else(|| {
        anyhow!(
            "embedded load `{prefix}{rest}` lacks `//` separator between apparent repo and package"
        )
    })?;
    // `@_builtins` is Bazel's virtual repo name for the vendored
    // builtins tree. Loupe ships that tree at
    // `src/starlark/builtins_bzl/`, so the source-map key uses
    // `builtins_bzl/` as the canonical prefix. Translate at load-
    // resolution time so vendored files using either spelling
    // (`@_builtins//common/foo.bzl` and `@builtins_bzl//common/foo.bzl`)
    // land on the same cached module.
    let module = if module == "_builtins" {
        "builtins_bzl"
    } else {
        module
    };
    let (pkg, file) = after.split_once(':').unwrap_or(("", after));
    Ok(if pkg.is_empty() {
        format!("{module}/{file}")
    } else {
        format!("{module}/{pkg}/{file}")
    })
}

/// Evaluate one embedded file, recursively evaluating its
/// dependencies first. `in_progress` tracks the current resolution
/// stack so we can detect cycles. `cache` memoises results across
/// the whole tree walk.
fn evaluate_one(
    path: &str,
    globals: &Globals,
    cache: &mut HashMap<String, FrozenModule>,
    in_progress: &mut std::collections::BTreeSet<String>,
) -> Result<FrozenModule> {
    if let Some(m) = cache.get(path) {
        return Ok(m.dupe());
    }
    if !in_progress.insert(path.to_string()) {
        return Err(anyhow!("embedded load cycle through `{path}`"));
    }

    let src = *source_map()
        .get(path)
        .ok_or_else(|| anyhow!("no embedded builtin at `{path}`"))?;
    let display_name = format!("@_builtins//{path}");
    let ast = AstModule::parse(&display_name, src.to_string(), &Dialect::Extended)
        .map_err(|e| anyhow!("parsing builtins {display_name}: {e}"))?;

    // Resolve every `load(...)` to its canonical embedded key and
    // recursively evaluate the dependency. Build the
    // ReturnFileLoader's key→module map keyed on the load-string
    // *as written* — starlark-rust's loader sees the raw string.
    let loads: Vec<(String, String)> = ast
        .loads()
        .into_iter()
        .map(|l| {
            let raw = l.module_id.to_string();
            let resolved = resolve_load(path, &raw)?;
            Ok::<_, anyhow::Error>((raw, resolved))
        })
        .collect::<Result<_>>()?;

    let mut loaded: HashMap<String, FrozenModule> = HashMap::new();
    for (raw, resolved) in &loads {
        let dep = evaluate_one(resolved, globals, cache, in_progress)?;
        loaded.insert(raw.clone(), dep);
    }

    let defining_label = loupe_labels::Label::new(
        loupe_labels::CanonicalRepo::new("_builtins"),
        loupe_labels::PackagePath::root(),
        path,
    );

    // Once `wrappers.bzl` is in the cache, subsequent embedded files
    // (notably the `builtins_bzl/` cc/objc/xcode chain) can see the
    // real `_builtins.toplevel.X` values rather than wildcards —
    // letting their module-time
    // `_builtins.toplevel.proto_common_do_not_use.external_proto_infos()`
    // and similar calls return real iterables. Files evaluated
    // BEFORE wrappers.bzl is cached (wrappers.bzl itself) get the
    // wildcard fallback. `wrappers.bzl` exports a
    // `builtins_namespace` struct that
    // [`alloc_strict_from_namespace`] reads to build the strict
    // `_builtins.toplevel` shape.
    let namespace_module: Option<FrozenModule> = if path != WRAPPERS_PATH {
        cache.get(WRAPPERS_PATH).cloned()
    } else {
        None
    };
    let frozen = Module::with_temp_heap(|module| -> Result<FrozenModule> {
        for dep in loaded.values() {
            module.frozen_heap().add_reference(dep.frozen_heap());
        }
        inject_builtins_magic(&module, namespace_module.as_ref());
        bind_freeze_time_wildcards(&module);
        let loader_modules: HashMap<&str, &FrozenModule> =
            loaded.iter().map(|(k, v)| (k.as_str(), v)).collect();
        let loader = starlark::eval::ReturnFileLoader {
            modules: &loader_modules,
        };
        {
            let mut eval = Evaluator::new(&module);
            eval.set_loader(&loader);
            eval.eval_module(ast, globals)
                .map_err(|e| anyhow!("evaluating builtins {display_name}: {e}"))?;
        }
        let all_names: Vec<String> = module
            .names_and_visibilities()
            .map(|(n, _)| n.as_str().to_string())
            .collect();
        let frozen_module = module
            .freeze()
            .map_err(|e| anyhow!("freezing builtins {display_name}: {e:?}"))?;
        crate::bzl_eval::stamp_origin_with_names(
            &frozen_module,
            &all_names,
            &defining_label,
            &std::collections::BTreeMap::new(),
        );
        Ok(frozen_module)
    })?;

    in_progress.remove(path);
    cache.insert(path.to_string(), frozen.dupe());
    Ok(frozen)
}

/// Ensure the two roots ([`WRAPPERS_PATH`], [`EXPORTS_PATH`]) plus
/// their transitive `load()` chains are evaluated and cached. Other
/// vendored files load lazily via [`embedded_module_by_path`].
///
/// Eagerly evaluating the *entire* 96-file tree at startup would
/// drag in references the embedded loader can't resolve in isolation;
/// lazy + reachable-only avoids freezing files we'll never need.
fn evaluate_embedded_tree() -> Result<&'static std::sync::Mutex<HashMap<String, FrozenModule>>> {
    if let Some(map) = CACHED.get() {
        return Ok(map);
    }
    let globals = wrappers_globals();
    let mut cache: HashMap<String, FrozenModule> = HashMap::new();
    let mut in_progress = std::collections::BTreeSet::new();
    evaluate_one(WRAPPERS_PATH, &globals, &mut cache, &mut in_progress)?;
    evaluate_one(EXPORTS_PATH, &globals, &mut cache, &mut in_progress)?;
    evaluate_one(BUILTINS_BZL_EXPORTS_PATH, &globals, &mut cache, &mut in_progress)?;
    for (path, _) in LOUPE_EXTRAS_PATHS {
        evaluate_one(path, &globals, &mut cache, &mut in_progress)?;
    }
    let _ = CACHED.set(std::sync::Mutex::new(cache));
    Ok(CACHED.get().expect("CACHED set above"))
}

/// Return the cached `builtins_core/exports.bzl` frozen module — its
/// `exported_rules` and `exported_toplevels` dicts hold the symbols
/// auto-injected into every user `.bzl` / BUILD via
/// [`inject_builtins_into`].
pub fn embedded_builtins() -> Result<FrozenModule> {
    let cache = evaluate_embedded_tree()?;
    cache
        .lock()
        .expect("builtins cache poisoned")
        .get(EXPORTS_PATH)
        .cloned()
        .ok_or_else(|| anyhow!("internal: builtins tree missing exports `{EXPORTS_PATH}`"))
}

/// Return the cached `builtins_bzl/exports.bzl` aggregator frozen
/// module — unions `common/exports.bzl` (cc/objc/xcode/fdo) with
/// `bazel/exports.bzl` (`py_internal`, `java_common`). Walked
/// alongside [`embedded_builtins`] by [`inject_builtins_into`].
fn embedded_builtins_bzl() -> Result<FrozenModule> {
    let cache = evaluate_embedded_tree()?;
    cache
        .lock()
        .expect("builtins cache poisoned")
        .get(BUILTINS_BZL_EXPORTS_PATH)
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "internal: builtins tree missing exports `{BUILTINS_BZL_EXPORTS_PATH}`"
            )
        })
}

/// Look up an embedded builtins file by its `@_builtins//<path>`
/// virtual location. If the file isn't yet in the cache, evaluate it
/// (and any transitively-loaded deps) on demand. Used by
/// `dice_keys::compute_bzl_load` so user-side
/// `load("@_builtins//builtins_bzl/common/paths.bzl", "paths")` reaches
/// any vendored file, not just the two eager roots.
pub fn embedded_module_by_path(path: &str) -> Result<Option<FrozenModule>> {
    if !source_map().contains_key(path) {
        return Ok(None);
    }
    let cache = evaluate_embedded_tree()?;
    if let Some(m) = cache
        .lock()
        .expect("builtins cache poisoned")
        .get(path)
        .cloned()
    {
        return Ok(Some(m));
    }
    let globals = wrappers_globals();
    let mut local: HashMap<String, FrozenModule> =
        cache.lock().expect("builtins cache poisoned").clone();
    let mut in_progress = std::collections::BTreeSet::new();
    let frozen = evaluate_one(path, &globals, &mut local, &mut in_progress)?;
    let mut guard = cache.lock().expect("builtins cache poisoned");
    for (k, v) in local {
        guard.entry(k).or_insert(v);
    }
    Ok(Some(frozen))
}

/// Same as [`embedded_module_by_path`] but also returns the file's raw
/// source text. Used by `BzlLoadKey` to compute the `.bzl` source
/// digest that enters the marker file's `predeclaredInputHash`.
pub fn embedded_module_with_source_by_path(
    path: &str,
) -> Result<Option<(FrozenModule, &'static str)>> {
    let Some(src) = source_map().get(path).copied() else {
        return Ok(None);
    };
    let Some(frozen) = embedded_module_by_path(path)? else {
        return Ok(None);
    };
    Ok(Some((frozen, src)))
}

/// Fetch the `invoke_rule` callable from the embedded
/// `builtins_core/wrappers.bzl`. The Rust analysis driver dispatches
/// every rule impl through this function instead of calling the impl
/// directly: `invoke_rule(impl, ctx)` runs `impl(_wrap_rule_ctx(ctx))`,
/// which is the seam where Starlark adds derived ctx fields without
/// touching Rust.
///
/// Returns the frozen value plus the owning `FrozenModule` (caller
/// must anchor the latter on whichever heap it's evaluating against).
pub fn invoke_rule_callable() -> Result<(starlark::values::FrozenValue, FrozenModule)> {
    let module = embedded_module_by_path("builtins_core/wrappers.bzl")?.ok_or_else(|| {
        anyhow!("internal: builtins_core/wrappers.bzl missing from embedded tree")
    })?;
    let owned = module
        .get_option("invoke_rule")
        .map_err(|e| anyhow!("internal: looking up invoke_rule: {e:?}"))?
        .ok_or_else(|| anyhow!("internal: builtins_core/wrappers.bzl missing `invoke_rule`"))?;
    // SAFETY: the FrozenValue lives on `module`'s frozen heap, which
    // we return alongside it for the caller to anchor.
    let fv = unsafe { owned.unchecked_frozen_value() };
    Ok((fv, module))
}

/// Anchor [`embedded_builtins`]'s frozen heap on `module` and inject
/// the contents of `exports.bzl::exported_rules` and
/// `exports.bzl::exported_toplevels` as pre-bound names in `module`.
/// Call this *before* the evaluator runs user code on `module`.
///
/// `_builtins` is itself an entry in `exported_toplevels` (set by
/// `exports.bzl` to a struct with strict `toplevel` / `internal`
/// sub-structs), so the magic namespace gets injected automatically
/// alongside the other toplevels.
pub fn inject_builtins_into(module: &Module) -> Result<()> {
    let exports = embedded_builtins()?;
    module.frozen_heap().add_reference(exports.frozen_heap());
    let bzl_exports = embedded_builtins_bzl()?;
    module.frozen_heap().add_reference(bzl_exports.frozen_heap());

    // `exported_toplevels` from both sources: providers, struct
    // namespaces (`coverage_common`, `platform_common`,
    // `cc_common`, `apple_common`, `CcSharedLibraryInfo`,
    // `py_internal`, `java_common`, …), plus the `_builtins`
    // magic — all become bare top-level names on user modules.
    // Mirrors bonanza's `getBzlFileBuiltins`, which walks every
    // builtins module's `exports.bzl::exported_toplevels`.
    inject_dict_entries(module, &exports, "exported_toplevels")?;
    inject_dict_entries(module, &bzl_exports, "exported_toplevels")?;

    // Surface BUILD-level rule constructors from BOTH exports
    // sources. `builtins_core/exports.bzl::exported_rules` carries
    // the Loupe-authored / bonanza-shape rules (`filegroup`,
    // `genrule`, `config_setting`, `platform`, …);
    // `builtins_bzl/exports.bzl::exported_rules` carries the
    // vendored Bazel cc/objc/xcode/fdo rules. The walk binds each
    // as a bare global; the `bind_native_struct` call further down
    // re-uses the same dicts to populate `native.<rule>` for
    // macros that reach through `native.cc_binary` / etc.
    //
    // For impls that fail to actually analyze (e.g. vendored
    // `fail("TODO: implement")` stubs), Loupe's analysis driver
    // dispatches to a Rust `synthesise_*_analysis` path BEFORE
    // invoking the impl; the injected binding just needs to
    // satisfy BUILD-eval and record an `UnconfiguredTarget`.
    inject_all_dict_entries(module, &exports, "exported_rules")?;
    inject_all_dict_entries(module, &bzl_exports, "exported_rules")?;
    // Loupe-specific extras: provider types like `BuildSettingInfo`
    // and accept-and-ignore stubs (`visibility`, `licenses`,
    // `transition`, …) that the vendored tree doesn't define but
    // user rules routinely reference. Each file contributes a strict
    // allowlist of names; everything else (including the freeze-time
    // `_builtins` + wildcard bindings every embedded module carries)
    // is intentionally ignored so we don't shadow real values from
    // `exported_toplevels`.
    for (path, symbols) in LOUPE_EXTRAS_PATHS {
        let Some(extras) = embedded_module_by_path(path)? else {
            continue;
        };
        module.frozen_heap().add_reference(extras.frozen_heap());
        for symbol in *symbols {
            let Ok(Some(owned)) = extras.get_option(symbol) else {
                continue;
            };
            let frozen_value = unsafe { owned.unchecked_frozen_value() };
            module.set(symbol, frozen_value.to_value());
        }
    }
    // Promote a curated set of `_builtins.internal.*` names to bare
    // top-levels. Real Bazel registers `cc_common` / … as host-Java
    // top-level builtins; the vendored upstream's `exports.bzl` only
    // puts them under `_builtins.internal`. Rulesets in the wild
    // (rules_cc, rules_nodejs) alias these at module scope
    // (`native_cc_common = cc_common`), so the bare names must
    // resolve. `apple_common` is intentionally *not* promoted from
    // internal — the vendored copy only carries `dotted_version`,
    // missing `Objc`/`XcodeVersionConfig`/… that rules_cc reaches
    // for. Workspaces needing those Apple bits today have to
    // override at workspace scope.
    for name in ["cc_common"] {
        promote_internal_to_toplevel(module, name)?;
    }
    // `native = struct(<every BUILD-level rule>)` for macros that
    // call `native.filegroup(...)` / `native.cc_library(...)` / etc.
    // Combines two sources:
    //   - User-space rules from `exports.bzl::exported_rules`
    //     (`filegroup`, `genrule`, `config_setting`, `platform`,
    //     `toolchain`, …).
    //   - Rust-side natives that don't live in `.bzl` (`alias`,
    //     `label_flag`, `label_setting`, `package_name`,
    //     `repository_name`, `package`, `licenses`).
    // No `WildcardStruct` fallback — accessing an unmodelled name
    // (e.g. `native.cc_library`) raises so the gap stays visible.
    bind_native_struct(module, &exports)?;
    Ok(())
}

/// Build `native = struct(<rules>)` on `module` from
/// `exports.bzl::exported_rules`. The same dict that drives bare-name
/// rule injection also defines the macro-visible `native.<rule>`
/// surface — both views derive from one source so adding a rule to
/// `exports.bzl` is one line, not two. Rust-side `native.*` helpers
/// that aren't rule constructors (`package_name`, `repository_name`)
/// come from a cached FrozenModule built once at startup.
fn bind_native_struct(module: &Module, exports: &FrozenModule) -> Result<()> {
    // Build the `native` struct from three sources, in order so later
    // writes win on conflict:
    //   1. Rust-side baseline (`package_name`, `repository_name`) —
    //      these are engine introspection helpers not representable
    //      as `exports.bzl` entries.
    //   2. `builtins_core/exports.bzl::exported_rules` — Loupe-shape
    //      user-space rules (`filegroup`, `genrule`, `sh_test`, …).
    //   3. `builtins_bzl/exports.bzl::exported_rules` — vendored
    //      Bazel cc/objc/xcode/fdo rule constructors.
    //
    // Mirrors bonanza's `getBzlFileBuiltins` (`pkg/model/analysis/
    // compiled_bzl_file.go`): walk every builtins module's
    // `exported_rules`, accumulate into the native struct.
    let mut entries: std::collections::BTreeMap<String, Value<'_>> =
        std::collections::BTreeMap::new();

    let extras = rust_native_extras()?;
    module.frozen_heap().add_reference(extras.frozen_heap());
    if let Ok(Some(owned)) = extras.get_option("native_extras") {
        let extras_value = unsafe { owned.unchecked_frozen_value() }.to_value();
        if let Some(d) = starlark::values::dict::DictRef::from_value(extras_value) {
            for (k, v) in d.iter() {
                let Some(name) = k.unpack_str() else { continue };
                entries.insert(name.to_string(), v);
            }
        }
    }

    for src in [exports, &embedded_builtins_bzl()?] {
        if let Ok(Some(dict_owned)) = src.get_option("exported_rules") {
            let dict_value = unsafe { dict_owned.unchecked_frozen_value() }.to_value();
            if let Some(d) = starlark::values::dict::DictRef::from_value(dict_value) {
                for (k, v) in d.iter() {
                    let Some(raw) = k.unpack_str() else { continue };
                    let name = raw.strip_prefix('+').unwrap_or(raw);
                    if v.downcast_ref::<WildcardStruct>().is_some() {
                        continue;
                    }
                    entries.insert(name.to_string(), v);
                }
            }
        }
    }

    let pairs: Vec<(&str, Value<'_>)> =
        entries.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    let native_struct = module.heap().alloc(AllocStruct(pairs));
    module.set("native", native_struct);
    Ok(())
}

/// Frozen module exporting Rust-side `native.*` members that aren't
/// in `exports.bzl::exported_rules` — `package_name`,
/// `repository_name`. Built lazily by evaluating a small snippet
/// against a Globals with the native-rule registrations applied;
/// OnceLock-cached so subsequent `inject_builtins_into` calls reuse
/// the same FrozenValues.
fn rust_native_extras() -> Result<&'static FrozenModule> {
    static CACHE: OnceLock<FrozenModule> = OnceLock::new();
    if let Some(m) = CACHE.get() {
        return Ok(m);
    }
    use starlark::environment::GlobalsBuilder;
    use starlark::eval::Evaluator;

    let globals = GlobalsBuilder::standard()
        .with(crate::starlark_types::native_rules::native_rules_globals)
        .with(crate::starlark_types::glob::glob_globals)
        .build();
    // Rust-side BUILD globals that need to travel via this cached
    // module because they aren't representable as `exports.bzl` rule
    // entries — they're Rust intrinsics that need engine state
    // (`BuildContext`, the filesystem walk). Everything else on
    // `native.*` (rule constructors, providers) is sourced from
    // vendored exports.bzl in `bind_native_struct`.
    // `native.bazel_version` is exposed by Bazel as the current
    // host Bazel version string. Several vendored .bzl files
    // (`bazel_features`'s `globals_repo.bzl`, `bazel_skylib`'s
    // `versions.bzl`) read it to drive feature gating. Loupe pins
    // `"9.0.0"` — Bazel 9 is the lowest version that makes
    // `bazel_features` light up *every* modern feature without
    // collapsing into legacy fallbacks; raising the pin is safe
    // when newer Bazel features need detection.
    // `native.bazel_version` / `native.existing_rule` /
    // `native.existing_rules` are read by many vendored `.bzl`s
    // (bazel_features's `globals_repo.bzl`, aspect_rules_js's
    // `pnpm_repository.bzl`, skylib's `versions.bzl`) during repo-rule
    // eval. Loupe doesn't model BUILD-time package state — stub the
    // existence queries to "no existing rules" so the calls are
    // pass-through; pin `bazel_version` to a value high enough that
    // feature gates in vendored code light up all modern features.
    let src = "def _existing_rule(*args, **kwargs):\n\
        \x20   _ = (args, kwargs)\n\
        \x20   return None\n\
        def _existing_rules(*args, **kwargs):\n\
        \x20   _ = (args, kwargs)\n\
        \x20   return {}\n\
        native_extras = {\n\
        \x20   \"package_name\": package_name,\n\
        \x20   \"repository_name\": repository_name,\n\
        \x20   \"glob\": glob,\n\
        \x20   \"bazel_version\": \"9.0.0\",\n\
        \x20   \"existing_rule\": _existing_rule,\n\
        \x20   \"existing_rules\": _existing_rules,\n\
        }\n";
    let ast = AstModule::parse("<rust_native_extras>", src.to_string(), &Dialect::Standard)
        .map_err(|e| anyhow!("parsing rust_native_extras: {e}"))?;
    let frozen = Module::with_temp_heap(|module| -> Result<FrozenModule> {
        {
            let mut eval = Evaluator::new(&module);
            eval.eval_module(ast, &globals)
                .map_err(|e| anyhow!("evaluating rust_native_extras: {e}"))?;
        }
        module
            .freeze()
            .map_err(|e| anyhow!("freezing rust_native_extras: {e:?}"))
    })?;
    let _ = CACHE.set(frozen);
    Ok(CACHE.get().expect("CACHE set above"))
}

/// Read `_builtins.internal.<name>` from the user module's already-
/// injected `_builtins` magic struct and bind the result as a bare
/// top-level name on the same module.
///
/// Used to upgrade host-Java-style toplevel primitives the vendored
/// upstream files only expose through the `_builtins.internal.*`
/// path (notably `cc_common`, `apple_common`).
fn promote_internal_to_toplevel(module: &Module, name: &str) -> Result<()> {
    use starlark::values::Value;
    let Some(builtins) = module.get("_builtins") else {
        return Ok(());
    };
    let internal = match Value::get_attr(builtins, "internal", module.heap()) {
        Ok(Some(v)) => v,
        _ => return Ok(()),
    };
    let value = match Value::get_attr(internal, name, module.heap()) {
        Ok(Some(v)) => v,
        _ => return Ok(()),
    };
    module.set(name, value);
    Ok(())
}

/// Inject only the listed keys from a frozen dict, ignoring everything
/// else. Used when a caller wants to restrict which entries become
/// bare globals.
///
/// Entries whose value is a [`WildcardStruct`] are skipped — these
/// come from `native.<name>` references in the vendored exports that
/// can't resolve during embedded eval (`native` is a wildcard at that
/// point). Shadowing the real Rust globals (`exports_files`, `glob`,
/// `package_group`, `licenses`, …) with a placeholder would break
/// BUILD evaluation.
#[allow(dead_code)]
fn inject_named_dict_entries(
    target: &Module,
    module: &FrozenModule,
    dict_name: &str,
    keys: &[&str],
) -> Result<()> {
    let owned = module
        .get_option(dict_name)
        .map_err(|e| anyhow!("looking up `{dict_name}` in exports: {e:?}"))?
        .ok_or_else(|| anyhow!("exports.bzl missing `{dict_name}`"))?;
    let frozen = unsafe { owned.unchecked_frozen_value() };
    let dict = starlark::values::dict::DictRef::from_value(frozen.to_value())
        .ok_or_else(|| anyhow!("`{dict_name}` in exports.bzl is not a dict"))?;
    for (k, v) in dict.iter() {
        let Some(name) = k.unpack_str() else {
            continue;
        };
        if !keys.contains(&name) {
            continue;
        }
        if v.downcast_ref::<WildcardStruct>().is_some() {
            continue;
        }
        target.set(name, v);
    }
    Ok(())
}

/// Inject every entry from `module::dict_name` as a top-level binding
/// on `target`. Mirrors bonanza's `getBzlFileBuiltins` walk over
/// `exports.bzl::exported_rules` — every rule becomes a bare global.
/// Bazel-style `+name` prefix (default-but-overridable marker) is
/// stripped from keys. Wildcard placeholders skipped, same rationale
/// as [`inject_named_dict_entries`].
fn inject_all_dict_entries(
    target: &Module,
    module: &FrozenModule,
    dict_name: &str,
) -> Result<()> {
    let owned = module
        .get_option(dict_name)
        .map_err(|e| anyhow!("looking up `{dict_name}` in exports: {e:?}"))?
        .ok_or_else(|| anyhow!("exports.bzl missing `{dict_name}`"))?;
    let frozen = unsafe { owned.unchecked_frozen_value() };
    let dict = starlark::values::dict::DictRef::from_value(frozen.to_value())
        .ok_or_else(|| anyhow!("`{dict_name}` in exports.bzl is not a dict"))?;
    for (k, v) in dict.iter() {
        let Some(raw) = k.unpack_str() else {
            continue;
        };
        let name = raw.strip_prefix('+').unwrap_or(raw);
        if v.downcast_ref::<WildcardStruct>().is_some() {
            continue;
        }
        target.set(name, v);
    }
    Ok(())
}

fn inject_dict_entries(target: &Module, module: &FrozenModule, dict_name: &str) -> Result<()> {
    let owned = module
        .get_option(dict_name)
        .map_err(|e| anyhow!("looking up `{dict_name}` in exports: {e:?}"))?
        .ok_or_else(|| anyhow!("exports.bzl missing `{dict_name}`"))?;
    // SAFETY: target.add_reference(exports.frozen_heap()) is done by
    // the caller; values we promote remain valid for the target heap.
    let frozen = unsafe { owned.unchecked_frozen_value() };
    let dict = starlark::values::dict::DictRef::from_value(frozen.to_value())
        .ok_or_else(|| anyhow!("`{dict_name}` in exports.bzl is not a dict"))?;
    for (k, v) in dict.iter() {
        let Some(name) = k.unpack_str() else {
            return Err(anyhow!(
                "`{dict_name}` in exports.bzl has non-string key: {k}"
            ));
        };
        if v.downcast_ref::<WildcardStruct>().is_some() {
            continue;
        }
        target.set(name, v);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invoke_rule_dispatches_through_user_space_wrapper() {
        // Pins the engine ↔ rule-impl indirection: `invoke_rule(fn,
        // ctx)` exists, is callable, and forwards through
        // `_wrap_rule_ctx` so derived ctx fields (build_file_path,
        // expand_make_variables, …) reach the rule's impl from
        // user-space rather than as Rust stubs.
        let (fv, _module) = invoke_rule_callable().unwrap();
        let display = fv.to_value().to_string();
        // Frozen function values display as their qualified name; we
        // just want to confirm we got something callable, not a None.
        assert!(
            display.contains("invoke_rule") || display.contains("function"),
            "invoke_rule callable looks wrong: {display}"
        );
    }

    #[test]
    fn embedded_tree_evaluates_in_topological_order() {
        // wrappers.bzl `load()`s fragment_info.bzl and exports.bzl,
        // so this exercises the multi-file load path end-to-end. The
        // wrappers root must carry the engine indirection
        // (`invoke_rule`) plus the symbols it re-exports from the
        // exports chain.
        let wrappers = embedded_module_by_path(WRAPPERS_PATH).unwrap().unwrap();
        let names: std::collections::BTreeSet<String> =
            wrappers.names().map(|n| n.as_str().to_string()).collect();
        assert!(names.contains("invoke_rule"), "names = {names:?}");
        assert!(names.contains("invoke_subrule"), "names = {names:?}");
        // Pulled in via `load()` from builtins_core/exports.bzl:
        assert!(names.contains("PlatformInfo"), "names = {names:?}");
        assert!(names.contains("SymlinkEntry"), "names = {names:?}");
        assert!(names.contains("TemplateVariableInfo"), "names = {names:?}");
        assert!(names.contains("runfiles"), "names = {names:?}");
        // Pulled in via `load()` from bazel_tools/fragments/fragment_info.bzl:
        assert!(names.contains("FragmentInfo"), "names = {names:?}");
    }

    #[test]
    fn exports_bzl_publishes_exported_dicts() {
        // builtins_core/exports.bzl is the user-visible names root:
        // its `exported_rules` and `exported_toplevels` dicts are
        // walked by `inject_builtins_into` to populate every user
        // `.bzl`/BUILD module.
        let exports = embedded_module_by_path(EXPORTS_PATH)
            .unwrap()
            .expect("builtins_core/exports.bzl missing from embedded tree");
        let names: std::collections::BTreeSet<String> =
            exports.names().map(|n| n.as_str().to_string()).collect();
        assert!(
            names.contains("exported_rules"),
            "exported_rules missing: {names:?}"
        );
        assert!(
            names.contains("exported_toplevels"),
            "exported_toplevels missing: {names:?}"
        );
    }

    #[test]
    fn inject_builtins_makes_coverage_common_visible_in_user_module() {
        let globals = wrappers_globals();
        Module::with_temp_heap(|module| {
            inject_builtins_into(&module).unwrap();
            let ast = AstModule::parse(
                "test.bzl",
                "result = type(coverage_common)".to_string(),
                &Dialect::Standard,
            )
            .unwrap();
            {
                let mut eval = Evaluator::new(&module);
                eval.eval_module(ast, &globals).unwrap();
            }
            let result = module.get("result").unwrap();
            assert_eq!(result.unpack_str(), Some("struct"));
        });
    }

    #[test]
    fn inject_builtins_exposes_underscore_builtins_namespace() {
        // `_builtins` is built by builtins_core/exports.bzl as
        // `struct(toplevel=struct(**exported_toplevels), internal=struct(…))`
        // and shipped through `exported_toplevels["_builtins"]`. Both
        // sides are strict structs: known sub-names resolve to real
        // values; unknown names raise.
        let globals = wrappers_globals();
        Module::with_temp_heap(|module| {
            inject_builtins_into(&module).unwrap();
            // `DefaultInfo` is in exported_toplevels → reachable
            // through `_builtins.toplevel`. It's a provider value.
            let ast = AstModule::parse(
                "test.bzl",
                "result = type(_builtins.toplevel.DefaultInfo)".to_string(),
                &Dialect::Standard,
            )
            .unwrap();
            {
                let mut eval = Evaluator::new(&module);
                eval.eval_module(ast, &globals).unwrap();
            }
            let result = module.get("result").unwrap();
            assert_eq!(result.unpack_str(), Some("provider"));
        });
        // `_builtins.internal.cc_internal` is the cc-internal struct
        // (a real `struct` value) built by exports.bzl.
        Module::with_temp_heap(|module| {
            inject_builtins_into(&module).unwrap();
            let ast = AstModule::parse(
                "test.bzl",
                "result = type(_builtins.internal.cc_internal)".to_string(),
                &Dialect::Standard,
            )
            .unwrap();
            {
                let mut eval = Evaluator::new(&module);
                eval.eval_module(ast, &globals).unwrap();
            }
            let result = module.get("result").unwrap();
            assert_eq!(result.unpack_str(), Some("struct"));
        });
    }
}
