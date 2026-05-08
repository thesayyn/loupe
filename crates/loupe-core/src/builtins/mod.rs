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
//! ## Multi-file embedded tree
//!
//! Files are listed in [`EMBEDDED_FILES`] in topological order —
//! each file's `load()` statements may only reference earlier
//! entries. The loader evaluates them in list order, caching each
//! `FrozenModule` so the next file's `load(...)` sees a populated
//! `ReturnFileLoader` map. Adding a new file is a one-line append
//! to that list (assuming dependency-respecting position).
//!
//! ## Lifecycle
//!
//! 1. [`embedded_builtins`] runs once (OnceLock-cached): walks
//!    [`EMBEDDED_FILES`] in order, evaluates each against the *base*
//!    `bzl_globals` plus the previously-cached frozen modules, and
//!    returns the root `wrappers.bzl` frozen module.
//!
//! 2. [`inject_builtins_into`] is called from each user-`.bzl` and
//!    BUILD evaluator before user code runs. It anchors the cached
//!    builtins frozen heap on the user module's frozen heap and
//!    copies each top-level name from `wrappers.bzl` into the user
//!    module via [`Module::set`]. The user code then sees those
//!    names as if they were native globals.
//!
//! ## Constraints
//!
//! - Embedded files may only `load()` from each other via
//!   `@_builtins//…` paths, and only earlier entries in
//!   [`EMBEDDED_FILES`].
//! - Symbols defined here become part of every `.bzl`'s frozen
//!   identity. Two `.bzl`s referencing `coverage_common` see the
//!   *same* `FrozenValue`, which is the correct shape for provider
//!   identity.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use anyhow::{Result, anyhow};
use starlark::environment::{FrozenModule, Globals, Module};
use starlark::eval::{Evaluator, ReturnFileLoader};
use starlark::syntax::{AstModule, Dialect};

/// The embedded builtins tree, in topological evaluation order.
///
/// Each entry is `(path, source)` where `path` is the
/// `@_builtins//<path>` virtual location and `source` is the
/// compiled-in `.bzl` text. Files later in the list may `load()`
/// from earlier entries; the first entry must be self-contained.
///
/// `wrappers.bzl` is by convention the *root* — its top-level
/// bindings are what gets auto-injected into every user
/// `.bzl` / BUILD module. Sub-files exist to keep `wrappers.bzl`
/// thin; they're not visible to user code unless re-exported by
/// `wrappers.bzl`.
const EMBEDDED_FILES: &[(&str, &str)] = &[
    // Leaf files: self-contained, no internal `load()`s.
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
    // Vendored upstream (bazel-skylib `lib/`) — pure-Starlark
    // helpers that drop in unmodified. Each file is self-contained
    // (no internal `load()`s); files with relative-load chains
    // (`new_sets.bzl` → `dicts.bzl`) need a smarter loader and stay
    // out for now.
    //
    // Reachable via `load("@_builtins//skylib/<name>.bzl", "<name>")`
    // from user `.bzl`s; not auto-injected (would shadow user-side
    // bindings of the same name).
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
    // Root: aggregates everything above.
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
        let ast = AstModule::parse(&display_name, src.to_string(), &Dialect::Standard)
            .map_err(|e| anyhow!("parsing builtins {display_name}: {e}"))?;
        // Each file's `load("@_builtins//foo.bzl", ...)` resolves
        // against entries already evaluated. The display name in the
        // load statement matches the keys we built up.
        let loader_modules: HashMap<&str, &FrozenModule> = cache
            .iter()
            .map(|(k, v)| {
                // Build the full virtual-repo path so user-side
                // `load("@_builtins//common/foo.bzl", ...)` resolves.
                let full = leak_string(format!("{VIRTUAL_REPO_PREFIX}{k}"));
                (full, v)
            })
            .collect();
        let loader = ReturnFileLoader {
            modules: &loader_modules,
        };
        let frozen = Module::with_temp_heap(|module| -> Result<FrozenModule> {
            // Anchor every previously-evaluated frozen heap so the
            // values flowing through `load()` survive this module's
            // freeze.
            for fm in cache.values() {
                module.frozen_heap().add_reference(fm.frozen_heap());
            }
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

/// Leak a `String` to `&'static str` so it can populate the
/// borrowed-key `ReturnFileLoader` map. Bounded leak: there's only
/// one entry per [`EMBEDDED_FILES`] file, evaluated once at process
/// startup. Avoids restructuring `ReturnFileLoader`'s borrow shape.
fn leak_string(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
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

/// Public-facing alias `wrappers.bzl` exposes the
/// `_builtins.toplevel.*` / `_builtins.internal.*` struct under,
/// before it gets re-published as `_builtins` in user modules. The
/// underscore prefix would otherwise mark the binding private to
/// the wrappers module — this alias gives us a public name we can
/// pull out via [`FrozenModule::get_option`].
const BUILTINS_NAMESPACE_ALIAS: &str = "builtins_namespace";

/// Anchor [`embedded_builtins`]'s frozen heap on `module` and inject
/// every top-level name from the cached builtins module as a
/// pre-bound name in `module`. Call this *before* the evaluator runs
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
    // Re-publish the wrappers `builtins_namespace` struct under the
    // canonical Bazel name `_builtins`. Done via `Module::set` so
    // user `.bzl`s see it as a top-level binding (the resolver finds
    // it like any other name); the underscore-prefix private-naming
    // rule applies to *exported* visibility, not to direct lookups
    // within the same module's scope, so user code that references
    // `_builtins.toplevel.provider` resolves cleanly.
    if let Ok(Some(owned)) = builtins.get_option(BUILTINS_NAMESPACE_ALIAS) {
        let frozen_value = unsafe { owned.unchecked_frozen_value() };
        module.set("_builtins", frozen_value.to_value());
    }
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
}
