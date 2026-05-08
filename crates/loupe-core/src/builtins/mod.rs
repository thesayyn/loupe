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
//! magic injection namespace — which Loupe stubs as
//! [`WildcardStruct`](crate::starlark_types::compat::WildcardStruct):
//! arbitrary attribute access + call returns more wildcards. Vendored
//! file bodies that read those magic values evaluate without crashing
//! even though the underlying Java/C++ machinery isn't modelled. The
//! function bodies that actually *use* those wildcard values only
//! execute when called, which `.bzl` analysis doesn't reach for these
//! stubs.
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
use std::sync::{Arc, OnceLock};

use anyhow::{Result, anyhow};
use starlark::collections::SmallMap;
use starlark::environment::{FrozenModule, Globals, Module};
use starlark::eval::{Evaluator, FileLoader};
use starlark::syntax::{AstModule, Dialect};
use starlark::values::Value;
use starlark::values::structs::AllocStruct;

use crate::starlark_types::compat::{LenientStruct, WildcardStruct};

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
const EMBEDDED_FILES: &[(&str, &str)] = &[
    // -- Loupe-authored compat files -----------------------------------
    // These define the user-facing provider/namespace surface
    // (`coverage_common`, `platform_common`, `DefaultInfo`, etc.) that
    // gets re-exported from `wrappers.bzl` and auto-injected as globals.
    (
        "common/coverage.bzl",
        include_str!("bzl/common/coverage.bzl"),
    ),
    (
        "common/platform.bzl",
        include_str!("bzl/common/platform.bzl"),
    ),
    (
        "common/providers.bzl",
        include_str!("bzl/common/providers.bzl"),
    ),
    // -- Vendored bazel-skylib (`bazel-skylib/lib/`) -------------------
    // Pure-Starlark helpers, drop-in unmodified. Reachable via
    // `load("@_builtins//skylib/<name>.bzl", …)` from user `.bzl`s;
    // not auto-injected.
    (
        "skylib/collections.bzl",
        include_str!("bzl/skylib/collections.bzl"),
    ),
    ("skylib/dicts.bzl", include_str!("bzl/skylib/dicts.bzl")),
    ("skylib/paths.bzl", include_str!("bzl/skylib/paths.bzl")),
    ("skylib/shell.bzl", include_str!("bzl/skylib/shell.bzl")),
    ("skylib/structs.bzl", include_str!("bzl/skylib/structs.bzl")),
    ("skylib/types.bzl", include_str!("bzl/skylib/types.bzl")),
    (
        "skylib/versions.bzl",
        include_str!("bzl/skylib/versions.bzl"),
    ),
    // -- Vendored Bazel `builtins_bzl/` --------------------------------
    // Verbatim copy of `bazelbuild/bazel`'s
    // `src/main/starlark/builtins_bzl/` tree. These files reach for
    // `_builtins.{toplevel,internal}.*` which Loupe stubs as
    // [`WildcardStruct`]. Order is topological: leaves before
    // intermediates before roots.
    //
    // Leaves (no internal `load()`s):
    ("common/util.bzl", include_str!("bzl/common/util.bzl")),
    ("common/paths.bzl", include_str!("bzl/common/paths.bzl")),
    (
        "common/cc/cc_helper_internal.bzl",
        include_str!("bzl/common/cc/cc_helper_internal.bzl"),
    ),
    (
        "common/objc/objc_info.bzl",
        include_str!("bzl/common/objc/objc_info.bzl"),
    ),
    (
        "common/objc/apple_env.bzl",
        include_str!("bzl/common/objc/apple_env.bzl"),
    ),
    (
        "common/objc/apple_toolchain.bzl",
        include_str!("bzl/common/objc/apple_toolchain.bzl"),
    ),
    (
        "common/objc/apple_platform.bzl",
        include_str!("bzl/common/objc/apple_platform.bzl"),
    ),
    (
        "common/java/java_common.bzl",
        include_str!("bzl/common/java/java_common.bzl"),
    ),
    (
        "common/python/py_internal.bzl",
        include_str!("bzl/common/python/py_internal.bzl"),
    ),
    (
        "common/exports.bzl",
        include_str!("bzl/common/exports.bzl"),
    ),
    // Mid (load leaves):
    (
        "common/cc/cc_common.bzl",
        include_str!("bzl/common/cc/cc_common.bzl"),
    ),
    (
        "common/cc/cc_common_bazel.bzl",
        include_str!("bzl/common/cc/cc_common_bazel.bzl"),
    ),
    (
        "common/xcode/providers.bzl",
        include_str!("bzl/common/xcode/providers.bzl"),
    ),
    (
        "common/builtin_exec_platforms.bzl",
        include_str!("bzl/common/builtin_exec_platforms.bzl"),
    ),
    // Late (load mids):
    (
        "common/objc/apple_common.bzl",
        include_str!("bzl/common/objc/apple_common.bzl"),
    ),
    (
        "bazel/exports.bzl",
        include_str!("bzl/bazel/exports.bzl"),
    ),
    ("exports.bzl", include_str!("bzl/exports.bzl")),
    // -- Root: aggregates the Loupe-authored files for auto-injection.
    ("wrappers.bzl", include_str!("bzl/wrappers.bzl")),
];

/// The path within [`EMBEDDED_FILES`] whose top-level bindings get
/// auto-injected as globals. Must be the last entry (its `load()`s
/// resolve against everything before it).
const ROOT_PATH: &str = "wrappers.bzl";

/// Filename prefix used for `load("@_builtins//...")` in user-side
/// imports. Files inside the embedded tree use this to reference
/// each other; outside callers don't see it directly.
const VIRTUAL_REPO_PREFIX: &str = "@_builtins//";

/// Lazily-initialised cache for the fully-evaluated builtins tree.
/// Keyed by [`EMBEDDED_FILES`] path.
static CACHED: OnceLock<Arc<HashMap<String, FrozenModule>>> = OnceLock::new();

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
        Json,
        Typing,
        SetType,
    ])
    .with(crate::starlark_types::register_for_bzl)
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
/// `DefaultInfo`, …). We wrap each side (`toplevel`, `internal`) in
/// a [`LenientStruct`]: known names fall through to the real
/// frozen value, unknowns return a wildcard. So
/// `_builtins.toplevel.provider(...)` constructs a real
/// `ProviderType` *and* `_builtins.toplevel.exec_transition(...)`
/// silently returns a wildcard.
fn inject_builtins_magic(module: &Module, namespace: Option<&FrozenModule>) {
    let (toplevel, internal) = match namespace {
        Some(ns) => (
            alloc_lenient_from_namespace(module.heap(), ns, "toplevel"),
            alloc_lenient_from_namespace(module.heap(), ns, "internal"),
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
}

/// Pull `builtins_namespace.<key>` out of `wrappers.bzl`'s frozen
/// module, snapshot its struct fields into a [`LenientStruct`], and
/// allocate it on `heap`. If anything's missing or shaped unexpectedly,
/// fall back to a pure [`WildcardStruct`] (keeps eval moving even
/// when the wrappers tree is wrong — better than a hard crash).
fn alloc_lenient_from_namespace<'v>(
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
    let mut fields: SmallMap<String, starlark::values::FrozenValue> = SmallMap::new();
    for name in inner.dir_attr() {
        if let Ok(Some(v)) = inner.get_attr(&name, heap) {
            if let Some(fv) = v.unpack_frozen() {
                fields.insert(name, fv);
            }
        }
    }
    heap.alloc(LenientStruct { fields })
}

/// [`FileLoader`] for the embedded builtins tree.
///
/// Normalises load paths so vendored Bazel files can use any of:
///   - `@_builtins//common/foo.bzl`
///   - `@_builtins//:common/foo.bzl`
///   - `:common/foo.bzl` (the root-relative form Bazel's own
///     `builtins_bzl/` files use to reference siblings)
/// All three resolve to the canonical [`EMBEDDED_FILES`] key
/// `common/foo.bzl`.
struct EmbeddedLoader<'a> {
    cache: &'a HashMap<String, FrozenModule>,
}

impl<'a> EmbeddedLoader<'a> {
    fn canonicalize(path: &str) -> String {
        let p = path.strip_prefix(VIRTUAL_REPO_PREFIX).unwrap_or(path);
        p.strip_prefix(':').unwrap_or(p).to_string()
    }
}

impl<'a> FileLoader for EmbeddedLoader<'a> {
    fn load(&self, path: &str) -> starlark::Result<FrozenModule> {
        let key = Self::canonicalize(path);
        match self.cache.get(&key) {
            Some(m) => Ok(m.dupe()),
            None => Err(starlark::Error::new_other(anyhow!(
                "embedded builtins loader: no module at `{path}` (canonical `{key}`)"
            ))),
        }
    }
}

// `FrozenModule::dupe` is provided by `dupe::Dupe`.
use dupe::Dupe;

/// Evaluate every entry in [`EMBEDDED_FILES`] in declared order,
/// returning the resulting `path → FrozenModule` map. Cached for the
/// process lifetime.
fn evaluate_embedded_tree() -> Result<Arc<HashMap<String, FrozenModule>>> {
    if let Some(map) = CACHED.get() {
        return Ok(Arc::clone(map));
    }
    let globals = wrappers_globals();
    let mut cache: HashMap<String, FrozenModule> = HashMap::new();
    for (path, src) in EMBEDDED_FILES {
        let display_name = format!("{VIRTUAL_REPO_PREFIX}{path}");
        // Vendored Bazel files use Bazel's `.bzl` dialect — keyword-only
        // arguments (`def f(*, x)`), top-level `if`s, type hints — so
        // parse them with the permissive `Extended` dialect. Loupe-
        // authored compat files happen to also be Standard-compatible,
        // so using Extended here is uniformly safe.
        let ast = AstModule::parse(&display_name, src.to_string(), &Dialect::Extended)
            .map_err(|e| anyhow!("parsing builtins {display_name}: {e}"))?;
        let frozen = Module::with_temp_heap(|module| -> Result<FrozenModule> {
            // Anchor every previously-evaluated frozen heap so the
            // values flowing through `load()` survive this module's
            // freeze.
            for fm in cache.values() {
                module.frozen_heap().add_reference(fm.frozen_heap());
            }
            // Inject the `_builtins.{toplevel,internal}` magic
            // namespace before eval so vendored Bazel files'
            // top-level `_builtins.toplevel.X` / `_builtins.internal.X`
            // accesses succeed.
            inject_builtins_magic(&module, None);
            let loader = EmbeddedLoader { cache: &cache };
            {
                let mut eval = Evaluator::new(&module);
                eval.set_loader(&loader);
                eval.eval_module(ast, &globals)
                    .map_err(|e| anyhow!("evaluating builtins {display_name}: {e}"))?;
            }
            module
                .freeze()
                .map_err(|e| anyhow!("freezing builtins {display_name}: {e:?}"))
        })?;
        cache.insert(path.to_string(), frozen);
    }
    let arc = Arc::new(cache);
    let _ = CACHED.set(Arc::clone(&arc));
    Ok(CACHED.get().cloned().unwrap_or(arc))
}

/// Return the cached root `wrappers.bzl` frozen module — its
/// top-level names are the symbols auto-injected into every user
/// `.bzl` / BUILD via [`inject_builtins_into`].
pub fn embedded_builtins() -> Result<FrozenModule> {
    let tree = evaluate_embedded_tree()?;
    tree.get(ROOT_PATH)
        .cloned()
        .ok_or_else(|| anyhow!("internal: builtins tree missing root `{ROOT_PATH}`"))
}

/// Look up an embedded builtins file by its `@_builtins//<path>`
/// virtual location, returning its frozen module if present. Used
/// by `dice_keys::compute_bzl_load` to short-circuit the disk-walk
/// path when a user `.bzl` does
/// `load("@_builtins//common/coverage.bzl", "coverage_common")` —
/// the load resolves directly against the cached embedded tree.
pub fn embedded_module_by_path(path: &str) -> Result<Option<FrozenModule>> {
    let tree = evaluate_embedded_tree()?;
    Ok(tree.get(path).cloned())
}

/// Anchor [`embedded_builtins`]'s frozen heap on `module` and inject
/// every top-level name from the cached builtins module as a
/// pre-bound name in `module`. Also injects the
/// `_builtins.{toplevel,internal}` magic namespace via
/// [`inject_builtins_magic`]. Call this *before* the evaluator runs
/// user code on `module`.
pub fn inject_builtins_into(module: &Module) -> Result<()> {
    let builtins = embedded_builtins()?;
    module.frozen_heap().add_reference(builtins.frozen_heap());
    for name in builtins.names() {
        let symbol = name.as_str();
        let owned = match builtins.get_option(symbol) {
            Ok(Some(v)) => v,
            _ => continue,
        };
        // SAFETY: anchored above via add_reference; the FrozenValue
        // we promote remains valid for the lifetime of `module`'s
        // frozen heap.
        let frozen_value = unsafe { owned.unchecked_frozen_value() };
        module.set(symbol, frozen_value.to_value());
    }
    inject_builtins_magic(module, Some(&builtins));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_tree_evaluates_in_topological_order() {
        // wrappers.bzl `load()`s coverage.bzl, so this exercises the
        // multi-file path end-to-end. The cached root must carry
        // both the re-exported coverage symbols and the wrappers-
        // local provider types.
        let builtins = embedded_builtins().unwrap();
        let names: std::collections::BTreeSet<String> =
            builtins.names().map(|n| n.as_str().to_string()).collect();
        // From common/coverage.bzl, re-exported through wrappers.bzl:
        assert!(names.contains("coverage_common"), "names = {names:?}");
        assert!(names.contains("InstrumentedFilesInfo"), "names = {names:?}");
        // Defined directly in wrappers.bzl:
        assert!(names.contains("DefaultInfo"), "names = {names:?}");
        assert!(names.contains("OutputGroupInfo"), "names = {names:?}");
        assert!(names.contains("platform_common"), "names = {names:?}");
    }

    #[test]
    fn vendored_skylib_paths_parses_and_exposes_paths_struct() {
        // Fully-qualified upstream `bazel-skylib/lib/paths.bzl` drops
        // into the embedded tree unmodified. This exercises the
        // load-from-vendored-file path: the file isn't re-exported
        // by wrappers.bzl, but its `paths` struct is reachable
        // through the cached frozen-module map.
        let tree = evaluate_embedded_tree().unwrap();
        let paths_module = tree
            .get("skylib/paths.bzl")
            .expect("skylib/paths.bzl missing from embedded tree");
        let names: std::collections::BTreeSet<String> = paths_module
            .names()
            .map(|n| n.as_str().to_string())
            .collect();
        assert!(names.contains("paths"), "paths struct missing: {names:?}");
    }

    #[test]
    fn vendored_bazel_root_exports_evaluates() {
        // The root `exports.bzl` from upstream `builtins_bzl/`
        // pulls in `bazel/exports.bzl`, `common/exports.bzl`, and
        // `common/util.bzl` — exercising the full `_builtins.*`
        // wildcard surface plus the colon/no-colon load forms.
        let tree = evaluate_embedded_tree().unwrap();
        let exports = tree
            .get("exports.bzl")
            .expect("exports.bzl missing from embedded tree");
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
        assert!(
            names.contains("exported_to_java"),
            "exported_to_java missing: {names:?}"
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
        // User `.bzl` code that references `_builtins.toplevel.X` or
        // `_builtins.internal.X` must see the wildcard-shaped magic
        // namespace, not an unbound name.
        let globals = wrappers_globals();
        Module::with_temp_heap(|module| {
            inject_builtins_into(&module).unwrap();
            let ast = AstModule::parse(
                "test.bzl",
                "result = type(_builtins.toplevel.anything.deep.chain)".to_string(),
                &Dialect::Standard,
            )
            .unwrap();
            {
                let mut eval = Evaluator::new(&module);
                eval.eval_module(ast, &globals).unwrap();
            }
            let result = module.get("result").unwrap();
            assert_eq!(result.unpack_str(), Some("wildcard_struct"));
        });
    }
}
