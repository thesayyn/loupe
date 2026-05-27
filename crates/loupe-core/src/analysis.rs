//! Analysis — turning `UnconfiguredTarget`s into `AnalyzedTarget`s.
//!
//! Phase 5 deliverables (per `PLAN.md`):
//! > Implement `Configuration`, `AnalysisKey`, the analysis compute
//! > function (recursive analysis of label deps, select resolution,
//! > ctx construction, rule impl invocation), `Target` value with
//! > provider indexing, `ctx` minimum (attr, label, workspace_name).
//!
//! The compute function:
//!
//! 1. Force `PackageKey` for the target's package, look up the
//!    `UnconfiguredTarget`.
//! 2. Force `BzlLoadKey` for the rule's defining `.bzl`, find the
//!    `FrozenRuleClass` it defines.
//! 3. (Phase 5 stub: no `attr.label` yet, so no recursive analysis of
//!    dep labels. The plumbing is here so Phase 5+ can drop it in.)
//! 4. (Phase 5 stub: no `select()` resolution since fixture #1 has no
//!    selectors. `resolve_selector` exists for the next ruleset.)
//! 5. Build a Starlark heap for the analysis run, allocate `ctx` with
//!    resolved attrs, invoke the impl through `Evaluator::eval_function`.
//! 6. Process the impl's return value — must be a list of provider
//!    instances — into `Vec<CapturedProvider>` and freeze.
//!
//! Everything in `AnalyzedTarget` is plain Rust data. No Starlark
//! pointers cross the DICE boundary — that's the DESIGN.md invariant
//! for cross-recompute persistence.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use allocative::Allocative;
use anyhow::{Context, Result, anyhow};
use dupe::Dupe;
use pagable::Pagable;
use starlark::any::ProvidesStaticType;
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::values::structs::AllocStruct;
use starlark::values::{FrozenValue, ValueLike};

use crate::action::Action;
use crate::package::{AttrValue, UnconfiguredTarget};
use crate::starlark_types::ctx::CtxGen;
use crate::starlark_types::file::File;
use crate::starlark_types::provider::{
    FrozenProviderType, ProviderId, ProviderInstance, ProviderType,
};
use crate::starlark_types::rule::FrozenRuleClass;
use crate::starlark_types::selector::{Selector, SelectorList, SelectorPiece};
use loupe_labels::{Label, PackageId};

/// Build configuration. v1 has exactly one (the empty/default config),
/// but the type is plumbed everywhere so multi-config (Phase 5+) can
/// land without API surgery.
///
/// The `<config>` segment in `bazel-out/<config>/bin/...` paths is
/// derived from the configuration's distinguishing inputs (cpu,
/// compilation_mode, exec marker, platform_suffix, transition hash) by
/// [`Configuration::segment`], matching Bazel's
/// `BuildConfigurationValue.getOutputDirectoryName()` byte-for-byte
/// for the cases v1 surfaces. See the implementation for the exact
/// derivation rules and the STUB notes for exec-hash / transition-hash
/// gaps that need engine work to close.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Allocative, Pagable)]
pub struct Configuration {
    /// Optional explicit override of the `<config>` segment. When
    /// `None` (the usual case), [`Configuration::segment`] derives
    /// from `build_settings` + `is_exec`. When `Some`, that exact
    /// string is returned — fixtures or tests that need a stable
    /// non-Bazel-shaped segment for snapshot diffing can set this.
    ///
    /// Defaults to `None`; only [`Configuration::with_segment_override`]
    /// flips it on. `unconfigured()` sets it to `Some("none")` so its
    /// `none/bin/...` sentinel paths stay deterministic.
    segment_override: Option<String>,
    /// Target platform label, if set. Currently only consumed by the
    /// toolchain resolver for `target_compatible_with` matching;
    /// other Bazel uses of platform (constraint setting fallback,
    /// transitions) are STUB.
    target_platform: Option<loupe_labels::Label>,
    /// Exec platform label, if explicitly set. `None` falls back to
    /// the target platform — that preserves the single-platform host
    /// build semantics Loupe shipped with before the split. For
    /// cross-builds, set this via [`Configuration::with_exec_platform`];
    /// `attr.label(cfg="exec")` deps then transition to a config whose
    /// `target_platform` is this label, and `exec_compatible_with`
    /// constraint matching keys off this platform's constraint set.
    exec_platform: Option<loupe_labels::Label>,
    /// Engine-exported build settings — the values normally driven by
    /// Bazel command-line flags (`--compilation_mode`, `--cpu`,
    /// `--platforms`, `--define KEY=VALUE`, …). Loupe doesn't read a
    /// CLI; the engine ships a fixed default map and tests override
    /// individual keys via [`Configuration::with_build_setting`].
    ///
    /// Consumed by `config_setting(values = {...})` matching in
    /// `select()` — the resolver compares each `(k, v)` against this
    /// map. The `@bazel_tools//command_line_option:<name>` virtual
    /// flag namespace is served by the embedded `bazel_tools` repo
    /// (loaded through [`crate::repo_root::RepoRoot::Embedded`]); the
    /// `bool_flag`/`string_flag` rule impls there read
    /// `ctx.build_setting_value` which analysis populates from this
    /// map (keyed by target name) with the BUILD-declared
    /// `build_setting_default` as fallback.
    build_settings: std::collections::BTreeMap<String, String>,
    /// Marker set by [`Configuration::unconfigured`]. Targets analyzed
    /// under an unconfigured config carry the same flag through their
    /// dep resolution. Matches Bazel's `config.none()` semantics
    /// (`attr.label(cfg = config.none())`).
    unconfigured: bool,
    /// Repository layout for external-repo exec_paths — matches Bazel's
    /// `--experimental_sibling_repository_layout`. Default-off; flip via
    /// [`Configuration::with_sibling_repository_layout`]. Read via
    /// [`Configuration::repository_layout`].
    ///
    /// Affects `File::path()` for external-repo files only. Main-repo
    /// paths are the same in both layouts (no prefix on source,
    /// `bazel-out/<cfg>/bin/...` on generated). Match what Bazel's own
    /// `repository_exec_path()` in vendored
    /// `builtins_bzl/common/cc/cc_helper_internal.bzl` produces.
    repository_layout: RepositoryLayout,
    /// Set when this configuration was produced by an
    /// `attr.label(cfg="exec")` transition. Bazel's
    /// `getOutputDirectoryName()` emits `<cpu>-opt-exec-<hash>` for
    /// exec configs and `<cpu>-<mode>` for target configs; the marker
    /// lets [`Configuration::segment`] branch correctly.
    ///
    /// `derive_dep_config` flips it on when walking `AttrCfgKind::Exec`.
    is_exec: bool,
    /// REAPI `Platform.properties` — the sorted key/value pairs that
    /// land in the `Action.platform` (or `Command.platform`) proto when
    /// Loupe emits an Action Cache key. Bazel populates this from two
    /// sources, both surfaced through this field:
    ///
    /// 1. The configured target platform's `exec_properties = {...}`
    ///    attr (read off the `platform(...)` target inside
    ///    [`crate::dice_keys`]'s `load_one_platform` and merged onto
    ///    a per-target `Configuration` clone in `compute_analysis`).
    /// 2. An engine-side override via [`Configuration::with_exec_properties`]
    ///    — tests and direct API consumers that want to set platform
    ///    properties without standing up a `platform(...)` target.
    ///
    /// Per-action `execution_requirements` (the `ctx.actions.run`
    /// kwarg) lives on [`crate::action::Action`], not here — its scope
    /// is one Action, not the whole configuration.
    ///
    /// Defaults to empty. `BTreeMap` so iteration order is sorted —
    /// matches REAPI's "ordered key/value list" requirement without a
    /// post-hoc sort at AC-key time.
    exec_properties: std::collections::BTreeMap<String, String>,
}

/// Execroot layout for external-repo files. Mirrors Bazel's
/// `--experimental_sibling_repository_layout` flag.
///
/// The flag only affects external-repo exec_paths. Main-repo files
/// surface as `<pkg>/<short>` (source) or `bazel-out/<cfg>/bin/<pkg>/<short>`
/// (generated) under either layout.
///
/// Bazel's own logic is in vendored `cc_helper_internal.bzl` at
/// `repository_exec_path()`:
/// - Default (`Default`) — external repos sit under `external/<canonical>/`.
/// - Sibling (`Sibling`) — execroot moves up one level; external repos
///   resolve via `../<canonical>/` relative to the main repo's execroot.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Allocative, Pagable,
)]
pub enum RepositoryLayout {
    /// `--noexperimental_sibling_repository_layout`. External repos at
    /// `external/<canonical>/<pkg>/<short>` (source) or
    /// `bazel-out/<cfg>/bin/external/<canonical>/<pkg>/<short>` (generated).
    #[default]
    Default,
    /// `--experimental_sibling_repository_layout`. External repos at
    /// `../<canonical>/<pkg>/<short>` (source) or
    /// `../<canonical>/bazel-out/<cfg>/bin/<pkg>/<short>` (generated).
    Sibling,
}

impl Configuration {
    /// The default v1 configuration. Builds from
    /// [`default_build_settings`] (`cpu=k8`, `compilation_mode=fastbuild`),
    /// so `Configuration::segment()` returns `"k8-fastbuild"` — exactly
    /// what Bazel emits for an out-of-the-box Linux-x86_64 build.
    pub fn default_v1() -> Self {
        Configuration {
            segment_override: None,
            target_platform: None,
            exec_platform: None,
            build_settings: default_build_settings(),
            unconfigured: false,
            repository_layout: RepositoryLayout::Default,
            is_exec: false,
            exec_properties: std::collections::BTreeMap::new(),
        }
    }

    /// The "no configuration" sentinel — `attr.label(cfg=config.none())`
    /// resolves deps under this. Matches Bazel: empty build-settings,
    /// no platform, segment marker `"none"` so output paths stay
    /// disambiguable. The segment is pinned via `segment_override`
    /// because the empty build_settings would otherwise derive to
    /// `-fastbuild`-with-no-cpu (nonsensical for an "unconfigured" dep).
    pub fn unconfigured() -> Self {
        Configuration {
            segment_override: Some("none".to_string()),
            target_platform: None,
            exec_platform: None,
            build_settings: std::collections::BTreeMap::new(),
            unconfigured: true,
            repository_layout: RepositoryLayout::Default,
            is_exec: false,
            // `config.none()` semantics: no platform means no platform
            // properties either. Mirrors how `build_settings` zeroes
            // out — the unconfigured config is a deliberately empty
            // dep-resolution baseline.
            exec_properties: std::collections::BTreeMap::new(),
        }
    }

    /// Returns a copy with the build-settings overlay stripped.
    /// Backs `attr.label(cfg=config.unconfigured())`: the dep keeps
    /// the parent's target platform but loses CLI-flag-shaped state.
    pub fn without_build_settings(&self) -> Self {
        Configuration {
            segment_override: self.segment_override.clone(),
            target_platform: self.target_platform.clone(),
            exec_platform: self.exec_platform.clone(),
            build_settings: std::collections::BTreeMap::new(),
            unconfigured: self.unconfigured,
            repository_layout: self.repository_layout,
            is_exec: self.is_exec,
            // `exec_properties` tracks the *platform*, not the
            // command-line flag overlay; preserve it across the
            // build-settings strip the same way `target_platform`
            // survives.
            exec_properties: self.exec_properties.clone(),
        }
    }

    /// True iff this config was produced by [`Configuration::unconfigured`].
    pub fn is_unconfigured(&self) -> bool {
        self.unconfigured
    }

    /// Set the target platform — a `platform(...)` target's label.
    /// Constraint matching (`target_compatible_with`) only kicks in
    /// when this is set; with no platform, the resolver falls back
    /// to first-match-wins on `toolchain_type`.
    pub fn with_target_platform(mut self, label: loupe_labels::Label) -> Self {
        // Mirror the platform onto `//command_line_option:platforms`
        // so transitions that read it via the flag namespace see the
        // same value. Bazel keeps these in lockstep too.
        self.build_settings
            .insert("platforms".to_string(), label.to_string());
        self.target_platform = Some(label);
        self
    }

    /// Set the exec platform explicitly. Cross-builds use this to
    /// distinguish "where the binary will run" (target) from "where
    /// the build actions run" (exec). When unset, [`exec_platform`]
    /// falls back to the target platform — the single-platform host
    /// build semantics Loupe started with.
    pub fn with_exec_platform(mut self, label: loupe_labels::Label) -> Self {
        self.exec_platform = Some(label);
        self
    }

    /// Override a build setting (`compilation_mode`, `cpu`, `define`,
    /// …). Returns the builder for fluent chaining. Only the keys
    /// already in `build_settings` (or new keys callers explicitly
    /// add) are visible to `config_setting(values = {...})` matching;
    /// unknown keys read as missing.
    pub fn with_build_setting(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.build_settings.insert(key.into(), value.into());
        self
    }

    /// `<config>` segment used in `bazel-out/<config>/bin/...`.
    ///
    /// Returns the explicit `segment_override` when set (the
    /// `unconfigured()` / `with_segment_override()` paths), otherwise
    /// derives via [`derive_output_segment`] from `build_settings` and
    /// `is_exec` to match Bazel's `getOutputDirectoryName()`.
    pub fn segment(&self) -> String {
        if let Some(s) = &self.segment_override {
            return s.clone();
        }
        derive_output_segment(&self.build_settings, self.is_exec)
    }

    /// Pin the `<config>` segment to a specific string. Bypasses the
    /// build_settings-driven derivation — useful for fixtures /
    /// snapshot tests that want a stable, Bazel-shape-independent
    /// segment.
    pub fn with_segment_override(mut self, segment: impl Into<String>) -> Self {
        self.segment_override = Some(segment.into());
        self
    }

    /// Mark this configuration as the exec config produced by an
    /// `attr.label(cfg="exec")` transition. `segment()` then emits
    /// `<cpu>-opt-exec-<HASH>` (Bazel's exec-config naming). Hash
    /// derivation is STUB — see [`derive_output_segment`].
    pub fn as_exec(mut self) -> Self {
        self.is_exec = true;
        self
    }

    /// True iff this configuration was marked exec via
    /// [`Configuration::as_exec`].
    pub fn is_exec(&self) -> bool {
        self.is_exec
    }

    pub fn target_platform(&self) -> Option<&loupe_labels::Label> {
        self.target_platform.as_ref()
    }

    /// Effective exec platform: the explicitly-set exec platform when
    /// configured via [`Configuration::with_exec_platform`], otherwise
    /// falls back to the target platform. Returning a fallback keeps
    /// host-build callers (today's default) on identical semantics —
    /// `exec_compatible_with` checks against the same set as
    /// `target_compatible_with`.
    pub fn exec_platform(&self) -> Option<&loupe_labels::Label> {
        self.exec_platform
            .as_ref()
            .or(self.target_platform.as_ref())
    }

    /// Read-only view of the engine-exported build settings. See
    /// [`Configuration::build_settings`] field doc for the consumers.
    pub fn build_settings(&self) -> &std::collections::BTreeMap<String, String> {
        &self.build_settings
    }

    /// Match Bazel's `--experimental_sibling_repository_layout`.
    /// External-repo `File::path()` switches between
    /// `external/<canonical>/...` (default) and `../<canonical>/...`
    /// (sibling). Main-repo paths are unaffected.
    pub fn with_sibling_repository_layout(mut self, sibling: bool) -> Self {
        self.repository_layout = if sibling {
            RepositoryLayout::Sibling
        } else {
            RepositoryLayout::Default
        };
        self
    }

    /// Active repository layout for this configuration. Consumed by
    /// `File::path()` to render external-repo exec_paths.
    pub fn repository_layout(&self) -> RepositoryLayout {
        self.repository_layout
    }

    /// Replace the platform-properties overlay. The map matches Bazel's
    /// `platform(exec_properties = {...})` shape: a sorted string-to-
    /// string dict that ends up in REAPI's `Platform.properties`.
    ///
    /// Two ways the map gets populated in practice:
    ///
    /// - **Direct**: tests and Rust callers that want to pin platform
    ///   properties without standing up a `platform(...)` target use
    ///   this builder.
    /// - **Indirect**: when [`Configuration::with_target_platform`]
    ///   points at a `platform(...)` target, the analyzer reads that
    ///   target's `exec_properties` attr inside
    ///   [`crate::dice_keys`]'s `load_one_platform` and merges it onto
    ///   a per-target `Configuration` clone before invoking the rule
    ///   impl. The Configuration carried by `AnalyzedTarget` then
    ///   reflects the merged view; the engine-level Configuration the
    ///   user constructed stays as written.
    pub fn with_exec_properties(
        mut self,
        properties: std::collections::BTreeMap<String, String>,
    ) -> Self {
        self.exec_properties = properties;
        self
    }

    /// Read-only view of `Platform.properties`. See
    /// [`Configuration::exec_properties`] field docs for the consumers.
    pub fn exec_properties(&self) -> &std::collections::BTreeMap<String, String> {
        &self.exec_properties
    }
}

/// The engine-exported build settings every fresh `Configuration`
/// starts with. These are Bazel native flags whose values would
/// normally come from the command line; Loupe doesn't read one, so
/// each gets a stable default. Tests / callers override via
/// [`Configuration::with_build_setting`].
///
/// The list is intentionally short — only the flags Loupe actually
/// matches against in `select()` resolution or surfaces through
/// `//command_line_option:<name>`. Adding a new flag is a new entry
/// here plus (if it influences `select()`) a config_setting handler.
/// Derive Bazel's `<config>` mnemonic for the `bazel-out/<config>/bin/...`
/// path, matching `BuildConfigurationValue.getOutputDirectoryName()`.
///
/// ## Rules implemented
///
/// - **Target config (default).** `<cpu>-<compilation_mode>`. `cpu`
///   comes from `build_settings["cpu"]` (default `"k8"`);
///   `compilation_mode` from `build_settings["compilation_mode"]`
///   (default `"fastbuild"`). Out of the box: `"k8-fastbuild"` —
///   identical to what `bazel info output_path` reports on a clean
///   Linux-x86_64 build.
///
/// - **Exec config.** `<cpu>-opt-exec-<HASH>`. Bazel forces
///   `compilation_mode = opt` on exec deps regardless of the parent's
///   mode, then appends a hash of the exec-transition options.
///
/// - **`--platform_suffix=<S>`.** Appended via `build_settings`
///   (`platform_suffix`) as `<cpu>_<S>-<mode>...` per Bazel.
///
/// ## STUBs
///
/// - **Exec hash.** Bazel computes a sha1 prefix of the serialized
///   exec-transition options. Loupe doesn't (yet) maintain the
///   exec-options proto that hashes, so we emit `"0000000"` as a
///   visible-but-stable placeholder. Once the exec transition fires
///   real options through, hash them here and the segment will be
///   byte-identical to Bazel's.
///
/// - **Starlark-transition suffix.** Bazel appends
///   `-ST-<sha1_prefix>` when a Starlark transition non-trivially
///   changed build_settings vs. the baseline. Loupe v1 doesn't track
///   "vs baseline" diffs on Configuration; not emitted. Closing this
///   needs Configuration to know which settings differ from
///   `default_build_settings()` and to hash that delta. Tracked.
///
/// - **`--experimental_output_directory_naming_scheme=diff_against_baseline`.**
///   Bazel 7+'s diff-based scheme produces shorter mnemonics. We pin
///   the legacy/explicit scheme (Bazel's pre-diff default and still
///   the available mode); the diff scheme is a separate flag.
pub fn derive_output_segment(
    build_settings: &std::collections::BTreeMap<String, String>,
    is_exec: bool,
) -> String {
    let cpu = build_settings
        .get("cpu")
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("k8");
    let platform_suffix = build_settings
        .get("platform_suffix")
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty());

    let cpu_segment = match platform_suffix {
        Some(s) => format!("{cpu}_{s}"),
        None => cpu.to_string(),
    };

    if is_exec {
        // STUB: the trailing hash. Bazel's value is the sha1 prefix of
        // the serialized exec-transition options proto. Loupe doesn't
        // surface that proto yet, so emit a fixed placeholder. The
        // shape (`<cpu>-opt-exec-<7chars>`) matches Bazel; the hash
        // bytes don't.
        return format!("{cpu_segment}-opt-exec-0000000");
    }

    let mode = build_settings
        .get("compilation_mode")
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("fastbuild");
    format!("{cpu_segment}-{mode}")
}

fn default_build_settings() -> std::collections::BTreeMap<String, String> {
    let mut m = std::collections::BTreeMap::new();
    // Bazel's `--compilation_mode`. Bazel default is `fastbuild`.
    m.insert("compilation_mode".to_string(), "fastbuild".to_string());
    // `--cpu` / `--host_cpu`. Bazel auto-detects from the host;
    // Loupe pins `k8` (Linux x86_64) since most rule logic that
    // selects on cpu treats it as a coarse OS/arch fingerprint and
    // there's nothing to detect against on a test bench.
    m.insert("cpu".to_string(), "k8".to_string());
    m.insert("host_cpu".to_string(), "k8".to_string());
    // `--platforms` / `--host_platform`. Empty until callers wire a
    // target platform via `with_target_platform`, which mirrors the
    // value into `platforms` automatically.
    m.insert("platforms".to_string(), String::new());
    m.insert("host_platform".to_string(), String::new());
    // `--stamp` — controls volatile-status-file embedding. Off by
    // default in Bazel; rules_oci's `oci_image` consults it.
    m.insert("stamp".to_string(), "0".to_string());
    m
}

impl Default for Configuration {
    fn default() -> Self {
        Self::default_v1()
    }
}

impl std::fmt::Display for Configuration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.segment())
    }
}

impl Dupe for Configuration {}

/// One provider on an analyzed target. Identity is the `ProviderId`;
/// fields are live `FrozenValue`s into `AnalyzedTarget.analysis_heap`,
/// preserving the impl's full typed shape (lists of `File`, `depset`,
/// structs, …). Reading a field requires the consumer to anchor that
/// heap via `frozen_heap().add_reference(...)` first — see DESIGN.md
/// "non-negotiable invariants".
#[derive(Debug, Clone, Allocative)]
pub struct CapturedProvider {
    pub id: ProviderId,
    pub fields: BTreeMap<String, FrozenValue>,
    /// Frozen pointer to the `ProviderType` value the impl invoked
    /// to construct this instance. Lets `target[Provider]` and
    /// `ctx.toolchains[T]` re-emit a real `ProviderInstance` (not
    /// a struct) so rules that pass the result through their own
    /// providers list (rules_oci's `_resolved_toolchain_impl`)
    /// survive the consumer's `analysis::collect_providers_from_frozen`
    /// type check.
    pub type_ptr: Option<FrozenValue>,
}

impl PartialEq for CapturedProvider {
    fn eq(&self, other: &Self) -> bool {
        // Equality on (id, field-name set + per-field FrozenValue
        // pointer identity). Two analyses producing the same providers
        // typically allocate distinct frozen heaps, so PartialEq on
        // CapturedProvider is most useful inside a single
        // analysis-output module.
        self.id == other.id
            && self.fields.len() == other.fields.len()
            && self.fields.iter().all(|(k, v)| {
                other
                    .fields
                    .get(k)
                    .map(|w| v.to_value().ptr_eq(w.to_value()))
                    .unwrap_or(false)
            })
    }
}
impl Eq for CapturedProvider {}

/// Result of analyzing one configured target.
#[derive(Debug, Clone, Allocative)]
pub struct AnalyzedTarget {
    pub label: Label,
    pub configuration: Configuration,
    pub rule_kind: String,
    /// Providers in the order returned by the impl. `AnalyzedTarget::provider`
    /// looks up by `ProviderId`.
    pub providers: Vec<CapturedProvider>,
    /// Actions the impl recorded via `ctx.actions.*`. Empty for
    /// source-file targets.
    pub actions: Vec<Action>,
    /// The frozen analysis-output heap that owns every `FrozenValue`
    /// in `providers` (and `actions`'s `File`s, transitively). Held as
    /// `Arc<FrozenModule>` so an `AnalyzedTarget` is cheap to dupe and
    /// the heap stays pinned for the engine's lifetime. Downstream
    /// rules that read a dep's providers anchor this heap on their
    /// own analysis module.
    pub analysis_heap: Arc<FrozenModule>,
}

impl PartialEq for AnalyzedTarget {
    fn eq(&self, other: &Self) -> bool {
        self.label == other.label
            && self.configuration == other.configuration
            && self.rule_kind == other.rule_kind
            && Arc::ptr_eq(&self.analysis_heap, &other.analysis_heap)
            && self.providers == other.providers
            && self.actions == other.actions
    }
}
impl Eq for AnalyzedTarget {}

impl AnalyzedTarget {
    pub fn provider(&self, id: &ProviderId) -> Option<&CapturedProvider> {
        self.providers.iter().find(|p| &p.id == id)
    }

    /// Find a captured provider by its symbol name alone, ignoring
    /// the defining `.bzl` Label. Useful for Loupe internals that
    /// need to reach for a Bazel-named provider (`ToolchainInfo`,
    /// `DefaultInfo`, `OutputGroupInfo`) regardless of whether it
    /// was registered as a native Rust provider or migrated to
    /// `crate::builtins::wrappers.bzl`. **Don't** use this for user-
    /// defined provider identity — Bazel keeps two `MyInfo`s in
    /// different `.bzl`s distinct, and that's the right shape.
    pub fn provider_by_symbol(&self, symbol: &str) -> Option<&CapturedProvider> {
        self.providers.iter().find(|p| p.id.symbol == symbol)
    }

    /// The `DefaultInfo` provider — Bazel guarantees every analyzed
    /// target has one (synthesized empty by `run_analysis` if the
    /// impl didn't return one explicitly). Looks up by symbol so a
    /// `DefaultInfo` constructed via the wrappers.bzl-defined
    /// ProviderType matches the same way as the synthesized empty
    /// CapturedProvider whose id has `defining_bzl = None`.
    pub fn default_info(&self) -> &CapturedProvider {
        self.provider_by_symbol("DefaultInfo")
            .expect("internal: every AnalyzedTarget must carry a DefaultInfo")
    }

    /// `DefaultInfo.files` flattened to a `Vec<File>`. v1 accepts
    /// either a flat list or a depset of `File`; everything else
    /// errors via [`unpack_default_files`]. Empty when the impl
    /// didn't set `files=` (synthesized DefaultInfo).
    pub fn default_files(&self) -> Vec<crate::starlark_types::file::File> {
        let p = self.default_info();
        match p.fields.get("files") {
            None => Vec::new(),
            Some(v) => unpack_default_files(v.to_value()).unwrap_or_default(),
        }
    }

    /// `DefaultInfo.executable` if set; `None` otherwise. The
    /// vendored `DefaultInfo` init folds the `executable` ctor kwarg
    /// into `files_to_run.executable` (the legacy
    /// top-level `executable` field no longer survives). Read
    /// `files_to_run.executable` first; fall back to the legacy
    /// top-level field for synthesised providers
    /// (`synthesise_*_analysis` builds DefaultInfo by hand).
    pub fn default_executable(&self) -> Option<crate::starlark_types::file::File> {
        let p = self.default_info();
        if let Some(ftr) = p.fields.get("files_to_run")
            && let Some(inst) =
                ftr.downcast_ref::<crate::starlark_types::provider::FrozenProviderInstance>()
            && let Some(exec_fv) = inst.fields.get("executable")
            && !exec_fv.is_none()
            && let Some(f) = crate::starlark_types::file::File::from_value(exec_fv.to_value())
        {
            return Some(f.clone());
        }
        p.fields
            .get("executable")
            .and_then(|v| crate::starlark_types::file::File::from_value(v.to_value()).cloned())
    }

    /// Find an action whose primary output matches `short_path`.
    /// Convenience for tests / aquery-style inspectors.
    pub fn action_for_output(&self, short_path: &str) -> Option<&Action> {
        self.actions.iter().find(|a| {
            a.primary_output()
                .map(|f| f.short_path() == short_path)
                .unwrap_or(false)
        })
    }
}

/// Build the `ctx.files`, `ctx.file`, `ctx.executable` structs by
/// walking the analyzed target's label-typed attrs and pulling each
/// dep's `DefaultInfo` field. All three live as fields on `Ctx` so
/// `ctx.files.<attr>` is plain attribute access at impl-call time.
///
/// - `ctx.files.<attr>`: a flat `[File]` of every dep's
///   `DefaultInfo.files`, in dep order.
/// - `ctx.file.<attr>`: only set for `attr.label` (single-dep) attrs;
///   value is the dep's `DefaultInfo.files[0]` or `None` if empty.
/// - `ctx.executable.<attr>`: only set for `attr.label(executable=True)`
///   attrs; value is the dep's `DefaultInfo.executable`.
fn build_dep_file_structs<'v>(
    heap: starlark::values::Heap<'v>,
    target: &UnconfiguredTarget,
    resolved_deps: &BTreeMap<String, ResolvedDep>,
    attrs_schema: &BTreeMap<String, crate::starlark_types::attr::FrozenAttrSpec>,
) -> (
    starlark::values::Value<'v>,
    starlark::values::Value<'v>,
    starlark::values::Value<'v>,
) {
    use crate::starlark_types::attr::AttrKind;
    let _ = target;
    // Pre-populate every label-typed attr with a falsy default so
    // `ctx.file.<attr>` / `ctx.files.<attr>` / `ctx.executable.<attr>`
    // resolve cleanly when the BUILD-side invocation didn't supply
    // a value (Bazel: optional `attr.label` reads as `None`,
    // optional `attr.label_list` reads as `[]`). Without this,
    // `ctx.file.loader` on an unsupplied `loader` attr errors with
    // "struct has no attribute loader" — see rules_oci's `oci_load`.
    let mut files_map: std::collections::BTreeMap<String, starlark::values::Value<'v>> =
        std::collections::BTreeMap::new();
    let mut file_map: std::collections::BTreeMap<String, starlark::values::Value<'v>> =
        std::collections::BTreeMap::new();
    let mut exec_map: std::collections::BTreeMap<String, starlark::values::Value<'v>> =
        std::collections::BTreeMap::new();
    for (attr_name, spec) in attrs_schema {
        match spec.kind {
            AttrKind::Label => {
                files_map.insert(
                    attr_name.clone(),
                    heap.alloc(Vec::<starlark::values::Value<'v>>::new()),
                );
                file_map.insert(attr_name.clone(), starlark::values::Value::new_none());
                if spec.executable {
                    exec_map.insert(attr_name.clone(), starlark::values::Value::new_none());
                }
            }
            AttrKind::LabelList | AttrKind::LabelKeyedStringDict => {
                files_map.insert(
                    attr_name.clone(),
                    heap.alloc(Vec::<starlark::values::Value<'v>>::new()),
                );
            }
            _ => {}
        }
    }

    for (attr_name, dep) in resolved_deps {
        let spec = attrs_schema.get(attr_name);

        // Flatten every dep's DefaultInfo.files into one list per
        // attr. `ctx.files.deps` is what most ruleset code iterates.
        let mut all_files: Vec<starlark::values::Value<'v>> = Vec::new();
        for at in dep.iter_targets() {
            for f in at.default_files() {
                all_files.push(heap.alloc(f));
            }
        }
        files_map.insert(attr_name.clone(), heap.alloc(all_files));

        // ctx.file.<attr>: single-dep attrs only.
        if let ResolvedDep::Single(at) = dep {
            let single = at.default_files().into_iter().next();
            let v = match single {
                Some(f) => heap.alloc(f),
                None => starlark::values::Value::new_none(),
            };
            file_map.insert(attr_name.clone(), v);
        }

        // ctx.executable.<attr>: only for `attr.label(executable=True)`.
        let exec_dep = match (spec, dep) {
            (Some(s), ResolvedDep::Single(at)) if s.executable => Some(at),
            _ => None,
        };
        if let Some(at) = exec_dep {
            // Resolve in priority order:
            // 1. The dep's `DefaultInfo.executable` (which, post-
            //    two-stage init pattern, lives at `files_to_run.executable`;
            //    `default_executable()` handles both shapes).
            // 2. The dep's `DefaultInfo.files[0]` (Bazel surfaces the
            //    source file directly for `attr.label(executable=True,
            //    allow_files=True)` — `tar_toolchain(binary = "tar")`).
            // Anything else returns `None` and the downstream
            // `.short_path` access raises so the gap is visible.
            let v = match at.default_executable() {
                Some(f) => heap.alloc(f),
                None => match at.default_files().into_iter().next() {
                    Some(f) => heap.alloc(f),
                    None => starlark::values::Value::new_none(),
                },
            };
            exec_map.insert(attr_name.clone(), v);
        }
    }

    let files = heap.alloc(AllocStruct(files_map.iter().map(|(k, v)| (k.as_str(), *v))));
    let file = heap.alloc(AllocStruct(file_map.iter().map(|(k, v)| (k.as_str(), *v))));
    let exec = heap.alloc(AllocStruct(exec_map.iter().map(|(k, v)| (k.as_str(), *v))));
    (files, file, exec)
}

/// Build `ctx.outputs` — a struct keyed by every `attr.output(...)` /
/// `attr.output_list(...)` attr name on the rule. For each:
///
/// - `AttrKind::Output`: the materialised value is a label string. A
///   non-empty value declares a generated `File` in the target's
///   package; an empty string surfaces as `None` so the canonical
///   `ctx.outputs.<name> or ctx.actions.declare_file(...)` fallback
///   path keeps working.
/// - `AttrKind::OutputList`: the materialised value is a `List` of
///   strings; each element becomes a `File`. Empty list → `[]`.
///
/// Output labels are resolved against the target's package: a bare
/// name like `"foo.spec"` becomes `<pkg>/foo.spec`. Bazel allows
/// label-style values (`":foo.spec"`, `"//pkg:foo.spec"`); v1 only
/// strips a leading `:` and otherwise treats the value as a relative
/// path. Cross-package output labels aren't valid Bazel anyway.
fn build_outputs_struct<'v>(
    heap: starlark::values::Heap<'v>,
    target: &UnconfiguredTarget,
    materialised_attrs: &BTreeMap<String, AttrValue>,
    attrs_schema: &BTreeMap<String, crate::starlark_types::attr::FrozenAttrSpec>,
    config_segment: &str,
    repository_layout: RepositoryLayout,
) -> starlark::values::Value<'v> {
    use crate::starlark_types::attr::AttrKind;
    use crate::starlark_types::file::File;

    let pkg = target.label.package.clone();
    let make_file = |name: &str| -> Option<File> {
        let short = name.strip_prefix(':').unwrap_or(name);
        if short.is_empty() {
            None
        } else {
            Some(
                File::generated_bin(pkg.clone(), short, config_segment)
                    .with_layout(repository_layout),
            )
        }
    };

    let mut pairs: Vec<(String, starlark::values::Value<'v>)> = Vec::new();
    for (name, spec) in attrs_schema {
        let v = match spec.kind {
            AttrKind::Output => {
                let raw = materialised_attrs.get(name);
                let label_str = match raw {
                    Some(AttrValue::String(s)) => s.as_str(),
                    _ => "",
                };
                match make_file(label_str) {
                    Some(f) => heap.alloc(f),
                    None => starlark::values::Value::new_none(),
                }
            }
            AttrKind::OutputList => {
                let raw = materialised_attrs.get(name);
                let names: Vec<&str> = match raw {
                    Some(AttrValue::List(items)) => items
                        .iter()
                        .filter_map(|x| match x {
                            AttrValue::String(s) => Some(s.as_str()),
                            _ => None,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                let files: Vec<starlark::values::Value<'v>> = names
                    .into_iter()
                    .filter_map(|n| make_file(n).map(|f| heap.alloc(f)))
                    .collect();
                heap.alloc(files)
            }
            _ => continue,
        };
        pairs.push((name.clone(), v));
    }
    heap.alloc(AllocStruct(pairs.iter().map(|(k, v)| (k.as_str(), *v))))
}

/// Unpack a Starlark value into a flat `Vec<File>`. Accepts a flat
/// `list[File]` or a `depset[File]` — both shapes flow through Bazel
/// `DefaultInfo(files=...)` calls (rules_oci uses `depset(...)`,
/// simpler rulesets just pass a list). Anything else errors.
///
/// Depsets are flattened on a temp heap that drops at function exit;
/// `File` is a `starlark_simple_value` carrying plain Rust data, so
/// the cloned `File`s in the returned `Vec` are independent of any
/// heap.
pub fn unpack_default_files<'v>(
    v: starlark::values::Value<'v>,
) -> Result<Vec<crate::starlark_types::file::File>> {
    use crate::starlark_types::depset::{Depset, FrozenDepset};
    use crate::starlark_types::file::File;

    if let Some(list) = starlark::values::list::ListRef::from_value(v) {
        return list
            .iter()
            .map(|item| {
                File::from_value(item).cloned().ok_or_else(|| {
                    anyhow!(
                        "DefaultInfo.files entry is not a File (got {})",
                        item.get_type()
                    )
                })
            })
            .collect();
    }

    // depset(File) — walk `direct` and `transitive` Values directly,
    // downcasting each leaf to a `File`. No heap needed: depset's
    // structural fields hold the items as Values referencing the
    // caller's heap, and `File` is a `starlark_simple_value` whose
    // cloned Rust struct is heap-independent.
    if v.downcast_ref::<Depset<'v>>().is_some() || v.downcast_ref::<FrozenDepset>().is_some() {
        let mut out = Vec::new();
        flatten_files_depset(v, &mut out)?;
        return Ok(out);
    }

    Err(anyhow!(
        "DefaultInfo.files must be a list or depset of File (got {})",
        v.get_type()
    ))
}

/// Flatten a `depset(File)` into the materialised `Vec<File>` that
/// `ctx.files.<dep>` / `ctx.file.<dep>` consumers expect.
///
/// Delegates to [`depset::flatten_to_values`], which honors the
/// depset's `order=` (default/preorder/postorder/topological) and
/// dedupes by Starlark value-equality. `File` implements `equals` /
/// `write_hash` on its kind + package + short_path + config segment
/// (see `starlark_types::file`), so dedupe actually collapses
/// duplicate references rather than no-opping on identity.
fn flatten_files_depset(
    v: starlark::values::Value<'_>,
    out: &mut Vec<crate::starlark_types::file::File>,
) -> Result<()> {
    use crate::starlark_types::file::File;
    let values = crate::starlark_types::depset::flatten_to_values(v).map_err(|e| anyhow!("{e}"))?;
    for item in values {
        let f = File::from_value(item).cloned().ok_or_else(|| {
            anyhow!(
                "DefaultInfo.files depset element is not a File (got {})",
                item.get_type()
            )
        })?;
        out.push(f);
    }
    Ok(())
}

/// Per-`run_analysis` mutable state. Lives in `Evaluator::extra` so
/// `ctx.actions.*` and friends can record into it without the impl
/// passing handles around. Mirrors the BUILD-side `BuildContext`.
#[derive(Debug, ProvidesStaticType)]
pub struct AnalysisContext {
    pub package: PackageId,
    pub configuration: Configuration,
    pub target_label: Label,
    /// Canonical labels of every `constraint_value` the configured
    /// target platform satisfies. Pre-computed by
    /// `dice_keys::force_target_platform_constraints` and threaded
    /// through `run_analysis`. `None` when no target platform is set
    /// — in that case `ctx.target_platform_has_constraint(cv)` raises
    /// (matches Bazel: with no platform, the call is undefined).
    pub target_platform_constraints: Option<Arc<std::collections::BTreeSet<Label>>>,
    /// Canonical labels of every `constraint_value` the configured
    /// exec platform satisfies. Defaults to the target set when no
    /// exec platform is set explicitly (`Configuration::exec_platform`
    /// falls back to target). The toolchain resolver checks each
    /// candidate's `exec_compatible_with` against this set.
    pub exec_platform_constraints: Option<Arc<std::collections::BTreeSet<Label>>>,
    /// Actions recorded by `ctx.actions.{write, run, run_shell,
    /// symlink}`. Order matches call order — Bazel's aquery output
    /// preserves declaration order.
    pub actions: RefCell<Vec<Action>>,
    /// Files declared via `ctx.actions.declare_file`. Indexed by
    /// short_path (the workspace-relative path) so a future
    /// `ctx.outputs` view can surface them by attr name.
    pub declared_files: RefCell<BTreeMap<String, File>>,
}

impl AnalysisContext {
    pub fn new(target_label: Label, configuration: Configuration) -> Self {
        let package = target_label.package.clone();
        AnalysisContext {
            package,
            configuration,
            target_label,
            target_platform_constraints: None,
            exec_platform_constraints: None,
            actions: RefCell::new(Vec::new()),
            declared_files: RefCell::new(BTreeMap::new()),
        }
    }
}

/// Pre-computed `select()` branch-key matches keyed by canonical
/// Label. The value is the *constraint set* that branch required to
/// match — the union of:
///
/// - For a `constraint_value` key: the constraint_value's own Label
///   (singleton).
/// - For a `config_setting` key: every Label in `constraint_values=…`
///   plus every key label in `flag_values=…` (after `values=`
///   normalisation rewrote it into `flag_values`).
///
/// Used by [`resolve_selector`] to disambiguate multiple matching
/// branches via Bazel's "most-specific" rule: branch A wins iff its
/// constraint set is a strict superset of every other matching
/// branch's. Incomparable matches (neither set ⊂ the other) are an
/// error — see `resolve_one_selector` below.
///
/// Built by `compute_select_matches` in `dice_keys`.
pub type SelectMatches = std::collections::BTreeMap<
    loupe_labels::Label,
    std::collections::BTreeSet<loupe_labels::Label>,
>;

/// Configuration-time resolution of a `SelectorList` against
/// `select_matches` — the pre-computed set of branch-key Labels that
/// match the current config. Each piece resolves either to its
/// concrete `AttrValue` (for `SelectorPiece::Concrete`) or to the
/// branch picked from a `Selector` by Bazel's "most specific match"
/// rule (see [`resolve_one_selector`]).
pub fn resolve_selector(
    list: &SelectorList,
    current_pkg: &loupe_labels::PackageId,
    repo_mapping: &loupe_labels::RepoMapping,
    select_matches: &SelectMatches,
) -> Result<AttrValue> {
    let mut concatenated: Vec<AttrValue> = Vec::new();
    let mut all_lists = true;
    let mut all_dicts = true;
    let mut all_strings = true;
    let mut all_label_lists = true;
    let mut all_label_dicts = true;
    for piece in &list.pieces {
        let resolved = match piece {
            SelectorPiece::Concrete(v) => v.clone(),
            SelectorPiece::Selector(sel) => {
                resolve_one_selector(sel, current_pkg, repo_mapping, select_matches)?
            }
        };
        if !matches!(resolved, AttrValue::List(_)) {
            all_lists = false;
        }
        if !matches!(resolved, AttrValue::Dict(_)) {
            all_dicts = false;
        }
        if !matches!(resolved, AttrValue::String(_)) {
            all_strings = false;
        }
        if !matches!(resolved, AttrValue::LabelList(_)) {
            all_label_lists = false;
        }
        if !matches!(resolved, AttrValue::LabelKeyedStringDict(_)) {
            all_label_dicts = false;
        }
        concatenated.push(resolved);
    }
    if concatenated.len() == 1 {
        return Ok(concatenated.into_iter().next().unwrap());
    }
    if all_lists {
        // `list + list` / `[…] + select({…: […]})` — concatenate.
        let mut joined = Vec::new();
        for v in concatenated {
            if let AttrValue::List(items) = v {
                joined.extend(items);
            }
        }
        return Ok(AttrValue::List(joined));
    }
    if all_label_lists {
        // `label_list + label_list` — concatenate. npm's
        // `npm_link_all_packages` builds `deps = [...] + select(...)`
        // over label lists.
        let mut joined = Vec::new();
        for v in concatenated {
            if let AttrValue::LabelList(items) = v {
                joined.extend(items);
            }
        }
        return Ok(AttrValue::LabelList(joined));
    }
    if all_label_dicts {
        // `label_keyed_string_dict | select({…: {…}})` — right-biased
        // merge. npm's generated `deps = {Label: alias} | select(...)`.
        let mut joined = std::collections::BTreeMap::new();
        for v in concatenated {
            if let AttrValue::LabelKeyedStringDict(m) = v {
                joined.extend(m);
            }
        }
        return Ok(AttrValue::LabelKeyedStringDict(joined));
    }
    if all_dicts {
        // `dict | dict` / `{…} | select({…: {…}})` — right-biased
        // merge (later pieces win), matching Bazel's `|`.
        let mut joined = std::collections::BTreeMap::new();
        for v in concatenated {
            if let AttrValue::Dict(m) = v {
                joined.extend(m);
            }
        }
        return Ok(AttrValue::Dict(joined));
    }
    if all_strings {
        // `str + str` / `"…" + select({…: "…"})` — concatenate.
        let mut joined = String::new();
        for v in concatenated {
            if let AttrValue::String(s) = v {
                joined.push_str(&s);
            }
        }
        return Ok(AttrValue::String(joined));
    }
    let kinds: Vec<&str> = concatenated
        .iter()
        .map(|v| match v {
            AttrValue::None => "None",
            AttrValue::String(_) => "string",
            AttrValue::Int(_) => "int",
            AttrValue::Bool(_) => "bool",
            AttrValue::List(_) => "list",
            AttrValue::Dict(_) => "dict",
            AttrValue::Label(_) => "Label",
            AttrValue::LabelList(_) => "LabelList",
            AttrValue::LabelKeyedStringDict(_) => "LabelKeyedStringDict",
            AttrValue::Selector(_) => "Selector",
            AttrValue::Opaque(_) => "Opaque",
        })
        .collect();
    Err(anyhow!(
        "select-list with mixed/unsupported piece types is not supported in v1 (pieces: [{}])",
        kinds.join(", ")
    ))
}

/// Resolve one `Selector` against `select_matches` using Bazel's
/// "most specific" rule:
///
/// 1. Collect every matching branch (excluding `//conditions:default`).
/// 2. Find the branches whose constraint set is a (non-strict)
///    superset of every other matching branch's set. Equivalently:
///    branches not strictly refined by any other matching branch.
/// 3. If exactly one such branch exists → it wins.
/// 4. If multiple incomparable branches survive → error
///    ("ambiguous select"). Bazel raises here; Loupe matches that
///    behaviour rather than silently picking the first.
/// 5. If no branch matched → fall through to `//conditions:default`
///    or surface `no_match_error`.
///
/// "Refines" means: A refines B iff A's constraint set is a strict
/// superset of B's. Two matching branches with identical sets also
/// fail (4) — the user wrote two distinct config_settings whose
/// match conditions coincide on this configuration.
fn resolve_one_selector(
    sel: &Selector,
    current_pkg: &loupe_labels::PackageId,
    repo_mapping: &loupe_labels::RepoMapping,
    select_matches: &SelectMatches,
) -> Result<AttrValue> {
    use loupe_labels::Label;

    // Walk every non-default branch; collect (key_str, value, set)
    // for everything that matched.
    let mut matches: Vec<(&str, &AttrValue, &std::collections::BTreeSet<Label>)> = Vec::new();
    for (key, value) in &sel.branches {
        if key == "//conditions:default" {
            continue;
        }
        if let Ok(l) = Label::resolve(key, current_pkg, repo_mapping)
            && let Some(set) = select_matches.get(&l)
        {
            matches.push((key.as_str(), value, set));
        }
    }

    if matches.is_empty() {
        if let Some(v) = sel.branches.get("//conditions:default") {
            return Ok(v.clone());
        }
        if let Some(msg) = &sel.no_match_error {
            return Err(anyhow!("select() had no matching branch: {msg}"));
        }
        return Err(anyhow!(
            "select() had no matching branch and no //conditions:default; \
             got branches {:?} (no_match_error: {:?})",
            sel.branches.keys().collect::<Vec<_>>(),
            sel.no_match_error
        ));
    }

    // A branch is "maximal" iff every other matching branch's set is
    // a (non-strict) subset of its own. Among maximals, exactly one
    // survives iff there's a unique strict-superset; otherwise some
    // pair is incomparable.
    let mut maximal_indices: Vec<usize> = Vec::new();
    for i in 0..matches.len() {
        let set_i = matches[i].2;
        let is_maximal = matches
            .iter()
            .enumerate()
            .all(|(j, (_, _, set_j))| i == j || set_j.is_subset(set_i));
        if is_maximal {
            maximal_indices.push(i);
        }
    }

    if maximal_indices.len() == 1 {
        return Ok(matches[maximal_indices[0]].1.clone());
    }
    // Multiple maximals → ambiguous. (`maximal_indices` is never
    // empty when `matches` is non-empty: at least one element has the
    // largest set among the finite collection.)
    let keys: Vec<&str> = maximal_indices.iter().map(|&i| matches[i].0).collect();
    Err(anyhow!(
        "select() matches multiple incomparable branches: {keys:?}. \
         None of these is a strict refinement of the others — \
         either remove one or add a constraint that disambiguates."
    ))
}

/// Per-attr resolved dependency from `compute_analysis`.
///
/// Mirrors `AttrKind::{Label, LabelList, LabelKeyedStringDict}`:
/// `Single` for `attr.label`, `List` for `attr.label_list`, `Dict` for
/// `attr.label_keyed_string_dict`. Each carries the dep's fully-
/// analyzed `AnalyzedTarget`; the `Dict` variant additionally carries
/// the original `Label` (for sort stability) and the per-key string
/// value supplied at BUILD time.
#[derive(Debug, Clone)]
pub enum ResolvedDep {
    Single(Arc<AnalyzedTarget>),
    List(Vec<Arc<AnalyzedTarget>>),
    Dict(Vec<(Label, Arc<AnalyzedTarget>, String)>),
}

/// Run analysis for one target. Synchronous — caller is responsible
/// for forcing `PackageKey`, `BzlLoadKey`, and recursive `AnalysisKey`
/// for each label-typed attr ahead of time, and passing the resolved
/// deps in via `resolved_deps`.
///
/// `defining_bzl` is the `FrozenModule` produced by the rule's
/// defining `.bzl`. Its frozen heap is anchored on the analysis
/// module so the `FrozenRuleClass` we look up by symbol stays valid
/// while we invoke the impl.
#[allow(clippy::too_many_arguments)] // matches Bazel's analysis surface
pub fn run_analysis(
    target: &UnconfiguredTarget,
    materialised_attrs: &BTreeMap<String, AttrValue>,
    defining_bzl: Arc<FrozenModule>,
    rule_symbol: &str,
    workspace_name: &str,
    configuration: &Configuration,
    resolved_deps: &BTreeMap<String, ResolvedDep>,
    resolved_toolchains: crate::starlark_types::toolchains::Toolchains,
    target_platform_constraints: Option<Arc<std::collections::BTreeSet<Label>>>,
    exec_platform_constraints: Option<Arc<std::collections::BTreeSet<Label>>>,
    empty_default_info: &EmptyDefaultInfo,
) -> Result<AnalyzedTarget> {
    let rule_value = defining_bzl
        .get_any_visibility(rule_symbol)
        .map(|(v, _vis)| v)
        .with_context(|| format!("looking up `{}` in defining .bzl", rule_symbol))?;

    let frozen_rc = rule_value
        .value()
        .downcast_ref::<FrozenRuleClass>()
        .ok_or_else(|| {
            anyhow!(
                "`{}` is not a RuleClass (got {})",
                rule_symbol,
                rule_value.value().get_type()
            )
        })?;

    let attrs_schema = frozen_rc.attrs.clone();
    let _ = &attrs_schema;
    // The impl FrozenValue lives on `defining_bzl`'s heap, but we
    // can't construct a `Value<'v>` for it out here without a target
    // heap — so we re-promote inside the closure.
    let impl_fn_frozen: starlark::values::FrozenValue = frozen_rc.implementation;

    let actx = {
        let mut a = AnalysisContext::new(target.label.clone(), configuration.clone());
        a.target_platform_constraints = target_platform_constraints;
        a.exec_platform_constraints = exec_platform_constraints;
        a
    };

    // Build the analysis module on a temp Heap, evaluate the impl,
    // and freeze. The temp `Heap` itself goes away when the closure
    // returns, but the produced `FrozenModule` carries an independent
    // `FrozenHeap` whose `Arc` we hold from then on — and that's what
    // owns every provider field's `FrozenValue`.
    let frozen_module: Arc<FrozenModule> =
        Module::with_temp_heap(|module| -> Result<Arc<FrozenModule>> {
            // Anchor every frozen heap that contributes to the
            // analysis: the defining `.bzl` (rule impl + provider
            // types), and each dep's `analysis_heap` (so FrozenValues
            // surfaced through `Target` / `ctx.attr.<dep>` stay valid
            // after this module's own freeze).
            module
                .frozen_heap()
                .add_reference(defining_bzl.frozen_heap());
            // Anchor the shared empty-`DefaultInfo` singleton's heap
            // unconditionally (one cheap edge). If the impl turns out not
            // to return its own `DefaultInfo`, the fallback below pushes
            // the singleton's `CapturedProvider`, whose `FrozenValue`s
            // live on that heap — so this reference keeps them valid past
            // this module's freeze.
            module
                .frozen_heap()
                .add_reference(empty_default_info.heap.frozen_heap());
            for dep in resolved_deps.values() {
                for at in dep.iter_targets() {
                    module
                        .frozen_heap()
                        .add_reference(at.analysis_heap.frozen_heap());
                }
            }
            // Anchor each resolved toolchain impl's heap, too — the
            // ToolchainInfo provider's FrozenValues live there and
            // ctx.toolchains[T] hands them through to the consumer.
            for at in resolved_toolchains.by_type.values() {
                module
                    .frozen_heap()
                    .add_reference(at.analysis_heap.frozen_heap());
            }

            let _globals = analysis_globals();

            let result_value = {
                let mut eval = Evaluator::new(&module);
                eval.extra = Some(&actx);

                // Build `ctx.attr` as a struct of resolved attr
                // values. Label-typed attrs become `Target` /
                // `[Target]` from `resolved_deps`; everything else
                // routes through `alloc_attr_value`.
                let mut attr_pairs: Vec<(String, starlark::values::Value<'_>)> =
                    Vec::with_capacity(materialised_attrs.len());
                for (name, raw) in materialised_attrs {
                    let cfg_transitioned = attrs_schema
                        .get(name)
                        .map(|s| s.cfg.surfaces_as_list())
                        .unwrap_or(false);
                    let v = match resolved_deps.get(name) {
                        Some(ResolvedDep::Single(t)) => {
                            let t_value = module
                                .heap()
                                .alloc(crate::starlark_types::target::Target::new(Arc::clone(t)));
                            if cfg_transitioned {
                                // `attr.label(cfg=…)` mimics Bazel's
                                // transition shape: a list-of-one
                                // Target. rules_oci's
                                // `_crane = attr.label(cfg=transition)`
                                // reads `ctx.attr._crane[0]`.
                                module.heap().alloc(vec![t_value])
                            } else {
                                t_value
                            }
                        }
                        Some(ResolvedDep::List(ts)) => {
                            let xs: Vec<starlark::values::Value<'_>> = ts
                                .iter()
                                .map(|t| {
                                    module
                                        .heap()
                                        .alloc(crate::starlark_types::target::Target::new(
                                            Arc::clone(t),
                                        ))
                                })
                                .collect();
                            module.heap().alloc(xs)
                        }
                        Some(ResolvedDep::Dict(entries)) => {
                            // `attr.label_keyed_string_dict` surfaces as a
                            // real Starlark dict keyed by `Target`. Targets
                            // are hashable on their configured label
                            // (`Target::write_hash`).
                            use starlark::collections::SmallMap;
                            use starlark::values::dict::Dict;
                            let mut sm: SmallMap<
                                starlark::values::Value<'_>,
                                starlark::values::Value<'_>,
                            > = SmallMap::with_capacity(entries.len());
                            for (_label, at, val) in entries {
                                let k = module.heap().alloc(
                                    crate::starlark_types::target::Target::new(Arc::clone(at)),
                                );
                                let v = module.heap().alloc(val.as_str());
                                let hashed = k.get_hashed().map_err(|e| {
                                    anyhow!("hashing Target dict key for `{name}`: {e}")
                                })?;
                                sm.insert_hashed(hashed, v);
                            }
                            module.heap().alloc(Dict::new(sm))
                        }
                        None => alloc_attr_value(module.heap(), raw.clone()),
                    };
                    attr_pairs.push((name.clone(), v));
                }
                // Hidden attr the vendored `_wrap_rule_ctx` looks for:
                // `__default_toolchains` surfaces every type-resolved
                // toolchain (from `rule(toolchains=[types])`) as a
                // Target the wrapper iterates to build `ctx.var` and
                // `ctx.expand_make_variables`.
                let toolchain_targets: Vec<starlark::values::Value<'_>> = resolved_toolchains
                    .by_type
                    .values()
                    .map(|at| {
                        module
                            .heap()
                            .alloc(crate::starlark_types::target::Target::new(Arc::clone(at)))
                    })
                    .collect();
                attr_pairs.push((
                    "__default_toolchains".to_string(),
                    module.heap().alloc(toolchain_targets),
                ));
                // `ctx.attr.toolchains` is the *instance-level* make-
                // variable provider list (canonicalised to a LabelList
                // at invocation, analyzed as deps, and already surfaced
                // above from `resolved_deps`). Bazel's implicit
                // `toolchains` attribute defaults to []; synthesise that
                // empty list only when the BUILD didn't set it, so the
                // wrapper's `TemplateVariableInfo` walk always has
                // something to iterate.
                if !attr_pairs.iter().any(|(k, _)| k == "toolchains") {
                    let empty: Vec<starlark::values::Value<'_>> = Vec::new();
                    attr_pairs.push(("toolchains".to_string(), module.heap().alloc(empty)));
                }
                let attr_struct = module.heap().alloc(AllocStruct(
                    attr_pairs.iter().map(|(k, v)| (k.as_str(), *v)),
                ));

                // Build `ctx.files` / `ctx.file` / `ctx.executable`
                // by walking the resolved deps and reading each
                // dep's `DefaultInfo`. These three structs are
                // pre-built once per analysis so the impl reads them
                // as plain attribute access without us doing any
                // per-read work.
                let (files_struct, file_struct, exec_struct) =
                    build_dep_file_structs(module.heap(), target, resolved_deps, &attrs_schema);
                let outputs_struct = build_outputs_struct(
                    module.heap(),
                    target,
                    materialised_attrs,
                    &attrs_schema,
                    &configuration.segment(),
                    configuration.repository_layout(),
                );

                let toolchains_value = module.heap().alloc(resolved_toolchains.clone());
                // `ctx.build_setting_value` — `None` unless the target's
                // rule was declared with `rule(build_setting=…)`. The
                // `build_setting_default` attr is added by `rule()` only
                // in that case (see `rule.rs::synthesize_implicit_attrs`),
                // so its presence is the signal that this target IS a
                // build_setting. Engine-side overrides
                // (`Configuration::with_build_setting(name, value)`) win
                // over the BUILD-declared default — keyed by the
                // target's `name` (the Bazel convention for
                // `--<flag_name>=...`). The configuration map stores
                // values as strings; coerce back using the attr
                // default's shape.
                let default_attr = materialised_attrs.get("build_setting_default");
                let override_str = if default_attr.is_some() {
                    configuration.build_settings().get(target.label.name())
                } else {
                    None
                };
                let build_setting_value: starlark::values::Value<'_> = match default_attr {
                    Some(AttrValue::String(default)) => {
                        let v = override_str.map(String::as_str).unwrap_or(default.as_str());
                        module.heap().alloc(v)
                    }
                    Some(AttrValue::Bool(default)) => {
                        let v = override_str
                            .and_then(|s| match s.as_str() {
                                "True" | "true" | "1" => Some(true),
                                "False" | "false" | "0" | "" => Some(false),
                                _ => None,
                            })
                            .unwrap_or(*default);
                        starlark::values::Value::new_bool(v)
                    }
                    Some(AttrValue::Int(default)) => {
                        let v = override_str
                            .and_then(|s| s.parse::<i64>().ok())
                            .unwrap_or(*default);
                        module.heap().alloc(v)
                    }
                    Some(AttrValue::List(items)) => {
                        // List-typed build settings stay on the attr
                        // default; engine-side override of list-shaped
                        // flags isn't wired yet.
                        let xs: Vec<starlark::values::Value<'_>> = items
                            .iter()
                            .map(|v| alloc_attr_value(module.heap(), v.clone()))
                            .collect();
                        module.heap().alloc(xs)
                    }
                    _ => starlark::values::Value::new_none(),
                };
                let ctx_value = module.heap().alloc(CtxGen {
                    label_str: target.label.to_string(),
                    workspace_name: workspace_name.to_string(),
                    config_segment: configuration.segment(),
                    attr: attr_struct,
                    files: files_struct,
                    file: file_struct,
                    executable: exec_struct,
                    outputs: outputs_struct,
                    toolchains: toolchains_value,
                    build_setting_value,
                });

                // Wrapper indirection: dispatch the rule impl
                // through user-space `invoke_rule(fn, ctx)` rather than
                // calling `fn(ctx)` directly. The wrapper lives in
                // `builtins_core/wrappers.bzl`; it's the seam where
                // Starlark-side ctx-derived fields land in later
                // phases. Phase 1 keeps the wrapper's `_wrap_rule_ctx`
                // as the identity, so behaviour is unchanged.
                let (invoke_rule_fv, invoke_rule_module) = crate::builtins::invoke_rule_callable()?;
                module
                    .frozen_heap()
                    .add_reference(invoke_rule_module.frozen_heap());
                let rv = eval
                    .eval_function(
                        invoke_rule_fv.to_value(),
                        &[impl_fn_frozen.to_value(), ctx_value],
                        &[],
                    )
                    .map_err(|e| anyhow!("invoking rule `{}` impl: {e}", target.kind))?;
                rv
            };

            // Stash the impl's return so its FrozenValues survive
            // the freeze and we can walk them out by name.
            module.set("__providers__", result_value);
            let frozen = module
                .freeze()
                .map_err(|e| anyhow!("freezing analysis module for `{}`: {e:?}", target.label))?;
            Ok(Arc::new(frozen))
        })?;

    let mut providers = collect_providers_from_frozen(&frozen_module)?;
    // Bazel guarantees every analyzed target carries a `DefaultInfo`. If
    // the impl didn't return one, push the shared empty-`DefaultInfo`
    // singleton (minted once through the vendored provider, so its
    // `computed_fields` — `default_runfiles` / `data_runfiles` — are
    // already flattened into the captured `fields`). Its `FrozenValue`s
    // stay valid because `frozen_module` references the singleton's heap
    // (anchored above). Downstream `target[DefaultInfo].default_runfiles`
    // (npm_link_package_store) then resolves instead of erroring.
    if !providers.iter().any(|p| p.id.symbol == "DefaultInfo") {
        providers.push(empty_default_info.provider.clone());
    }
    let actions = actx.actions.into_inner();

    Ok(AnalyzedTarget {
        label: target.label.clone(),
        configuration: configuration.clone(),
        rule_kind: target.kind.clone(),
        providers,
        actions,
        analysis_heap: frozen_module,
    })
}

impl ResolvedDep {
    fn iter_targets(&self) -> Box<dyn Iterator<Item = &Arc<AnalyzedTarget>> + '_> {
        match self {
            ResolvedDep::Single(t) => Box::new(std::iter::once(t)),
            ResolvedDep::List(ts) => Box::new(ts.iter()),
            ResolvedDep::Dict(entries) => Box::new(entries.iter().map(|(_, t, _)| t)),
        }
    }
}

/// Pull the impl's `[Provider, ...]` return from the frozen analysis
/// module's `__providers__` binding and reconstruct a `Vec<CapturedProvider>`
/// holding `FrozenValue` references into the same heap. Provider field
/// values are not copied.
fn collect_providers_from_frozen(frozen: &FrozenModule) -> Result<Vec<CapturedProvider>> {
    let result = frozen
        .get_option("__providers__")
        .map_err(|e| anyhow!("{e}"))?
        .ok_or_else(|| anyhow!("internal: analysis module missing `__providers__`"))?;
    let result_value = result.value();
    // Bazel rules return either a list of providers or a bare single
    // provider — `return DefaultInfo(executable=…, runfiles=…)` is
    // canonical for executable rules (see rules_oci's `oci_push`).
    // Accept both shapes.
    let single_value: [starlark::values::Value<'_>; 1];
    let items: &[starlark::values::Value<'_>] =
        if result_value.is_none() {
            // Bazel tolerates a rule impl that returns `None` (or falls
            // off the end without a `return`) — it just means "no extra
            // providers"; the target still gets the synthesised empty
            // DefaultInfo. rules_js's `generate_preset` does exactly
            // this (declares an action, returns nothing).
            &[]
        } else if let Some(list) = starlark::values::list::ListRef::from_value(result_value) {
            list.content()
        } else if ProviderInstance::from_value(result_value).is_some() {
            single_value = [result_value];
            &single_value
        } else {
            return Err(anyhow!(
                "rule impl must return a list of providers or a single provider, got {}",
                result_value.get_type()
            ));
        };
    let mut out = Vec::with_capacity(items.len());
    for item in items.iter().copied() {
        let pi = ProviderInstance::from_value(item).ok_or_else(|| {
            anyhow!(
                "rule impl returned non-provider in list: got {} (item={item:?})",
                item.get_type()
            )
        })?;
        let frozen_pi = match pi {
            either::Either::Right(f) => f,
            either::Either::Left(_) => {
                return Err(anyhow!(
                    "internal: provider instance in frozen module is still live — \
                     all values should have been frozen"
                ));
            }
        };
        let id = provider_id_from_type(frozen_pi.type_ptr.to_value())?;
        let mut fields: BTreeMap<String, FrozenValue> = frozen_pi
            .fields
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        // Flatten `provider(computed_fields=…)` results into the
        // captured fields map so downstream `target[Provider].<name>`
        // resolves the same as a direct attribute read.
        if let Some(computed) = frozen_pi.computed.get() {
            for (k, v) in computed.iter() {
                fields.entry(k.clone()).or_insert(*v);
            }
        }
        out.push(CapturedProvider {
            id,
            fields,
            type_ptr: Some(frozen_pi.type_ptr),
        });
    }
    Ok(out)
}

/// The empty `DefaultInfo()` instance, minted once through the vendored
/// provider and shared by every target whose impl returned no
/// `DefaultInfo`. `heap` owns the captured `FrozenValue`s; consumers must
/// anchor it via `frozen_heap().add_reference(...)` before reading. The
/// DICE singleton that computes this is `EmptyDefaultInfoKey`.
#[derive(Debug, Clone, Allocative)]
pub struct EmptyDefaultInfo {
    pub heap: Arc<FrozenModule>,
    pub provider: CapturedProvider,
}

impl PartialEq for EmptyDefaultInfo {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.heap, &other.heap)
    }
}
impl Eq for EmptyDefaultInfo {}
impl Dupe for EmptyDefaultInfo {}

/// Mint a `DefaultInfo(...)` through the vendored provider in a
/// standalone temp module and capture it. The provider's `init` /
/// `computed_fields` only touch the heap and the provider's own frozen
/// globals (`depset`, `_runfiles`, `FilesToRunProvider`) — never the
/// analysis `ActionContext` — so this runs with a bare `Evaluator` and no
/// DICE deps. `Some(file)` mints `DefaultInfo(files = depset([file]))`
/// (source files / predeclared outputs); `None` mints the empty instance.
/// The returned `FrozenModule` owns the captured `FrozenValue`s.
pub(crate) fn mint_default_info(
    file: Option<crate::starlark_types::file::File>,
) -> Result<(Arc<FrozenModule>, CapturedProvider)> {
    let frozen: Arc<FrozenModule> =
        Module::with_temp_heap(|module| -> Result<Arc<FrozenModule>> {
            let (default_info_fv, default_info_module) = crate::builtins::default_info_provider()?;
            module
                .frozen_heap()
                .add_reference(default_info_module.frozen_heap());
            let result = {
                let mut eval = Evaluator::new(&module);
                let named: Vec<(&str, starlark::values::Value)> = if let Some(f) = &file {
                    use crate::starlark_types::depset::{Depset, DepsetOrder};
                    let file_value = module.heap().alloc(f.clone());
                    let files_depset = module.heap().alloc_complex(Depset {
                        direct: vec![file_value],
                        transitive: Vec::new(),
                        order: DepsetOrder::Default,
                    });
                    vec![("files", files_depset)]
                } else {
                    Vec::new()
                };
                eval.eval_function(default_info_fv.to_value(), &[], &named)
                    .map_err(|e| anyhow!("minting DefaultInfo: {e}"))?
            };
            module.set("__default_info__", result);
            Ok(Arc::new(module.freeze().map_err(|e| {
                anyhow!("freezing minted DefaultInfo: {e:?}")
            })?))
        })?;
    let mut provider = capture_provider_by_module_name(&frozen, "__default_info__")?
        .ok_or_else(|| anyhow!("internal: minted DefaultInfo not captured"))?;
    // `DefaultInfo` is a *native* provider: synthesised instances carry
    // `defining_bzl: None` (Bazel cquery surfaces it as bare
    // "DefaultInfo"). The vendored provider in
    // `builtins_core/exports.bzl` stamps its id with `@_builtins//…`, so
    // normalise to the native shape — `target[DefaultInfo]`'s strict-id
    // match then misses and falls through to the symbol-based lookup that
    // recognises synthetics (see `target.rs` `at`). `type_ptr` follows:
    // the re-emission path falls back to the index key when it's `None`.
    provider.id = ProviderId {
        defining_bzl: None,
        symbol: "DefaultInfo".to_string(),
    };
    provider.type_ptr = None;
    Ok((frozen, provider))
}

/// Capture a single provider instance stored under `module_name` in a
/// frozen analysis module into a [`CapturedProvider`], flattening any
/// `computed_fields` into `fields` (same shape as
/// [`collect_providers_from_frozen`]). Returns `None` when the name is
/// absent or doesn't hold a provider instance.
fn capture_provider_by_module_name(
    frozen: &FrozenModule,
    module_name: &str,
) -> Result<Option<CapturedProvider>> {
    let Some(owned) = frozen.get_option(module_name).map_err(|e| anyhow!("{e}"))? else {
        return Ok(None);
    };
    let value = owned.value();
    let Some(pi) = ProviderInstance::from_value(value) else {
        return Ok(None);
    };
    let frozen_pi = match pi {
        either::Either::Right(f) => f,
        either::Either::Left(_) => return Ok(None),
    };
    let id = provider_id_from_type(frozen_pi.type_ptr.to_value())?;
    let mut fields: BTreeMap<String, FrozenValue> = frozen_pi
        .fields
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    if let Some(computed) = frozen_pi.computed.get() {
        for (k, v) in computed.iter() {
            fields.entry(k.clone()).or_insert(*v);
        }
    }
    Ok(Some(CapturedProvider {
        id,
        fields,
        type_ptr: Some(frozen_pi.type_ptr),
    }))
}

/// Resolve a captured `AttrValue` for use by the impl. Phase 5 doesn't
fn alloc_attr_value<'v>(
    heap: starlark::values::Heap<'v>,
    v: AttrValue,
) -> starlark::values::Value<'v> {
    match v {
        AttrValue::None => starlark::values::Value::new_none(),
        AttrValue::Bool(b) => starlark::values::Value::new_bool(b),
        AttrValue::Int(i) => heap.alloc(i),
        AttrValue::String(s) => heap.alloc(s),
        AttrValue::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(alloc_attr_value(heap, item));
            }
            heap.alloc(out)
        }
        AttrValue::Dict(map) => {
            // Surface as a real dict so `ctx.attr.<x>.items()` /
            // `ctx.attr.<x>[k]` etc. work the way rule impls expect.
            // Previously this was an `AllocStruct` which made
            // `attr.string_dict`-typed inputs read like a struct (no
            // `.items()`); `aspect_bazel_lib`'s `expand_template`
            // calls `substitutions.items()` and tripped on that.
            let pairs: Vec<(String, AttrValue)> = map.into_iter().collect();
            let alloc_pairs: Vec<(String, starlark::values::Value<'v>)> = pairs
                .into_iter()
                .map(|(k, v)| (k, alloc_attr_value(heap, v)))
                .collect();
            heap.alloc(starlark::values::dict::AllocDict(
                alloc_pairs.iter().map(|(k, v)| (k.as_str(), *v)),
            ))
        }
        AttrValue::Opaque(s) => heap.alloc(s),
        // Fallback only — `compute_analysis` substitutes a `Target`
        // value for label-typed attrs before we get here. Reaching
        // this branch means analysis was driven without
        // `resolved_deps`, e.g. from a unit test that bypasses DICE;
        // surface the canonical-string form so the impl can still
        // string-compare for diagnostics.
        AttrValue::Label(l) => heap.alloc(l.to_string()),
        AttrValue::LabelList(ls) => {
            let xs: Vec<starlark::values::Value<'v>> =
                ls.into_iter().map(|l| heap.alloc(l.to_string())).collect();
            heap.alloc(xs)
        }
        // Fallback only — `compute_analysis` substitutes a real
        // `dict[Target, str]` for `attr.label_keyed_string_dict`
        // before we get here. Reaching this branch means analysis
        // was driven without `resolved_deps`; surface label strings
        // as keys so impls can still string-compare.
        AttrValue::LabelKeyedStringDict(map) => {
            let pairs: Vec<(String, String)> =
                map.into_iter().map(|(l, v)| (l.to_string(), v)).collect();
            heap.alloc(starlark::values::dict::AllocDict(
                pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            ))
        }
        // Should never reach here — `compute_analysis` resolves
        // selectors against the configuration before run_analysis.
        // If it does (legacy callers), surface as a Display string
        // rather than crash.
        AttrValue::Selector(sl) => heap.alloc(sl.to_string()),
    }
}

fn provider_id_from_type<'v>(type_value: starlark::values::Value<'v>) -> Result<ProviderId> {
    // `type_ptr` is always a frozen `ProviderType` (set at provider
    // instance construction from `me.unpack_frozen()`), but we accept
    // either variant defensively so newer call sites that construct
    // unfrozen instances stay working.
    let id = if let Some(pt) = type_value.downcast_ref::<FrozenProviderType>() {
        pt.id.get().cloned()
    } else if let Some(pt) = type_value.downcast_ref::<ProviderType>() {
        pt.id.get().cloned()
    } else {
        return Err(anyhow!(
            "internal: ProviderInstance.type_ptr is not a ProviderType (got {})",
            type_value.get_type()
        ));
    };
    id.ok_or_else(|| anyhow!("internal: ProviderType is missing its ProviderId stamp"))
}

fn analysis_globals() -> Globals {
    GlobalsBuilder::standard()
        .with(crate::starlark_types::register_for_analysis)
        .build()
}

/// Build an empty frozen analysis heap for synthetic targets that
/// don't run an impl (e.g. `source_file` from `exports_files`). Such
/// targets still need `analysis_heap` plumbed so the type stays
/// uniform — they just don't allocate any FrozenValue-backed
/// providers.
pub fn empty_analysis_heap() -> Arc<FrozenModule> {
    Module::with_temp_heap(|module| {
        Arc::new(
            module
                .freeze()
                .expect("freezing an empty Module never fails"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use loupe_labels::{CanonicalRepo, PackagePath};
    use std::collections::BTreeMap;

    fn pkg() -> loupe_labels::PackageId {
        loupe_labels::PackageId::new(CanonicalRepo::MAIN, PackagePath::root())
    }

    fn pid(symbol: &str) -> ProviderId {
        ProviderId {
            defining_bzl: Some(Label::in_package(pkg(), "defs.bzl")),
            symbol: symbol.to_string(),
        }
    }

    #[test]
    fn configuration_dupe_yields_equal_value() {
        // DICE values must implement Dupe. Smoke-check that cloning
        // preserves the configured segment.
        let a = Configuration::default();
        let b = a.clone();
        assert_eq!(a, b);
        // Default config: derives `<cpu>-<mode>` from build_settings —
        // `cpu=k8`, `compilation_mode=fastbuild` (cf. `default_build_settings`),
        // so segment is `"k8-fastbuild"`, identical to what Bazel emits
        // on Linux-x86_64.
        assert_eq!(a.segment(), "k8-fastbuild");
    }

    #[test]
    fn segment_derivation_matches_bazel_output_dir_name() {
        // `<cpu>-<mode>` for target configs.
        let m = |cpu: &str, mode: &str| {
            let mut m = std::collections::BTreeMap::new();
            m.insert("cpu".to_string(), cpu.to_string());
            m.insert("compilation_mode".to_string(), mode.to_string());
            m
        };
        assert_eq!(
            derive_output_segment(&m("k8", "fastbuild"), false),
            "k8-fastbuild"
        );
        assert_eq!(derive_output_segment(&m("k8", "opt"), false), "k8-opt");
        assert_eq!(derive_output_segment(&m("k8", "dbg"), false), "k8-dbg");
        assert_eq!(
            derive_output_segment(&m("darwin_arm64", "opt"), false),
            "darwin_arm64-opt"
        );
        assert_eq!(
            derive_output_segment(&m("armeabi-v7a", "fastbuild"), false),
            "armeabi-v7a-fastbuild"
        );
    }

    #[test]
    fn segment_derivation_exec_emits_opt_exec_suffix() {
        // Exec config: `<cpu>-opt-exec-<hash>` regardless of parent's
        // compilation_mode. Hash is STUB (`0000000`) until exec
        // transition options are surfaced through Configuration.
        let mut m = std::collections::BTreeMap::new();
        m.insert("cpu".to_string(), "k8".to_string());
        m.insert("compilation_mode".to_string(), "fastbuild".to_string());
        assert_eq!(
            derive_output_segment(&m, true),
            "k8-opt-exec-0000000",
            "exec config forces opt mnemonic + hash suffix"
        );
    }

    #[test]
    fn segment_derivation_platform_suffix_uses_underscore() {
        // Bazel's `--platform_suffix=foo` joins via `_`:
        // `k8_foo-fastbuild`. Matches `BuildConfigurationValue`.
        let mut m = std::collections::BTreeMap::new();
        m.insert("cpu".to_string(), "k8".to_string());
        m.insert("compilation_mode".to_string(), "fastbuild".to_string());
        m.insert("platform_suffix".to_string(), "alpha".to_string());
        assert_eq!(derive_output_segment(&m, false), "k8_alpha-fastbuild");
    }

    #[test]
    fn segment_derivation_falls_back_when_settings_missing() {
        // Empty settings → defaults `k8-fastbuild`. Matters because
        // `Configuration::without_build_settings()` zeroes the map and
        // we still want a stable segment for the bazel-out path the
        // unconfigured-dep analyses produce.
        let empty = std::collections::BTreeMap::new();
        assert_eq!(derive_output_segment(&empty, false), "k8-fastbuild");
    }

    #[test]
    fn segment_override_bypasses_derivation() {
        // `with_segment_override` pins the segment exactly — useful for
        // fixtures that want non-Bazel-shaped output paths.
        let cfg = Configuration::default().with_segment_override("custom-cfg");
        assert_eq!(cfg.segment(), "custom-cfg");
        // unconfigured() uses the override path with `"none"`.
        assert_eq!(Configuration::unconfigured().segment(), "none");
    }

    #[test]
    fn as_exec_marks_configuration_and_flips_segment() {
        let cfg = Configuration::default().as_exec();
        assert!(cfg.is_exec());
        assert_eq!(cfg.segment(), "k8-opt-exec-0000000");
    }

    #[test]
    fn provider_lookup_finds_by_id() {
        // The provider list is searched by `ProviderId`; equality
        // tolerates the same `Arc<FrozenModule>` analysis_heap so the
        // synthetic target here doesn't allocate a fresh heap per
        // assertion.
        let heap = empty_analysis_heap();
        let target = AnalyzedTarget {
            label: Label::in_package(pkg(), "world"),
            configuration: Configuration::default(),
            rule_kind: "hello".into(),
            providers: vec![CapturedProvider {
                id: pid("HelloInfo"),
                fields: BTreeMap::new(),
                type_ptr: None,
            }],
            actions: Vec::new(),
            analysis_heap: heap,
        };
        let found = target.provider(&pid("HelloInfo")).unwrap();
        assert_eq!(found.id, pid("HelloInfo"));
        assert!(target.provider(&pid("NotThere")).is_none());
    }

    fn list(items: Vec<&str>) -> AttrValue {
        AttrValue::List(
            items
                .into_iter()
                .map(|s| AttrValue::String(s.into()))
                .collect(),
        )
    }

    fn empty_select_inputs() -> (
        loupe_labels::PackageId,
        loupe_labels::RepoMapping,
        SelectMatches,
    ) {
        let pkg = loupe_labels::PackageId::new(
            loupe_labels::CanonicalRepo::MAIN,
            loupe_labels::PackagePath::root(),
        );
        (
            pkg,
            loupe_labels::RepoMapping::empty(),
            SelectMatches::new(),
        )
    }

    #[test]
    fn resolve_selector_picks_default_branch_when_nothing_matches() {
        let mut branches = BTreeMap::new();
        branches.insert("//cpu:k8".into(), list(vec!["k8.cc"]));
        branches.insert("//conditions:default".into(), list(vec!["default.cc"]));
        let sel = SelectorList {
            pieces: vec![SelectorPiece::Selector(Selector {
                branches,
                no_match_error: None,
            })],
        };
        let (pkg, mapping, matches) = empty_select_inputs();
        assert_eq!(
            resolve_selector(&sel, &pkg, &mapping, &matches).unwrap(),
            list(vec!["default.cc"])
        );
    }

    #[test]
    fn resolve_selector_picks_first_matching_branch() {
        // `select_matches` contains `@@//cpu:k8`; the resolver picks
        // that branch over the default.
        let mut branches = BTreeMap::new();
        branches.insert("//cpu:k8".into(), list(vec!["k8.cc"]));
        branches.insert("//conditions:default".into(), list(vec!["default.cc"]));
        let sel = SelectorList {
            pieces: vec![SelectorPiece::Selector(Selector {
                branches,
                no_match_error: None,
            })],
        };
        let (pkg, mapping, mut matches) = empty_select_inputs();
        let k8 = loupe_labels::Label::new(
            loupe_labels::CanonicalRepo::MAIN,
            loupe_labels::PackagePath::new("cpu").unwrap(),
            "k8",
        );
        let mut set = std::collections::BTreeSet::new();
        set.insert(k8.clone());
        matches.insert(k8, set);
        assert_eq!(
            resolve_selector(&sel, &pkg, &mapping, &matches).unwrap(),
            list(vec!["k8.cc"])
        );
    }

    #[test]
    fn resolve_selector_picks_strict_superset_among_matching_branches() {
        // `//os:linux` is a constraint_value branch — its set is the
        // CV label itself: {linux}. `//cs:linux_x86` is a config_setting
        // branch listing two CVs — its set is {linux, x86}. The second
        // set is a strict superset → it wins.
        let mut branches = BTreeMap::new();
        branches.insert("//os:linux".into(), list(vec!["linux"]));
        branches.insert("//cs:linux_x86".into(), list(vec!["linux_x86"]));
        branches.insert("//conditions:default".into(), list(vec!["default"]));
        let sel = SelectorList {
            pieces: vec![SelectorPiece::Selector(Selector {
                branches,
                no_match_error: None,
            })],
        };
        let (pkg, mapping, mut matches) = empty_select_inputs();
        let linux = loupe_labels::Label::new(
            loupe_labels::CanonicalRepo::MAIN,
            loupe_labels::PackagePath::new("os").unwrap(),
            "linux",
        );
        let x86 = loupe_labels::Label::new(
            loupe_labels::CanonicalRepo::MAIN,
            loupe_labels::PackagePath::new("os").unwrap(),
            "x86",
        );
        let linux_x86 = loupe_labels::Label::new(
            loupe_labels::CanonicalRepo::MAIN,
            loupe_labels::PackagePath::new("cs").unwrap(),
            "linux_x86",
        );
        let mut s1 = std::collections::BTreeSet::new();
        s1.insert(linux.clone());
        matches.insert(linux.clone(), s1);
        let mut s2 = std::collections::BTreeSet::new();
        s2.insert(linux);
        s2.insert(x86);
        matches.insert(linux_x86, s2);
        assert_eq!(
            resolve_selector(&sel, &pkg, &mapping, &matches).unwrap(),
            list(vec!["linux_x86"])
        );
    }

    #[test]
    fn resolve_selector_errors_on_incomparable_matches() {
        // Two matching branches with incomparable sets: {linux, gcc}
        // and {linux, x86}. Neither refines the other → ambiguous.
        let mut branches = BTreeMap::new();
        branches.insert("//cs:a".into(), list(vec!["a"]));
        branches.insert("//cs:b".into(), list(vec!["b"]));
        branches.insert("//conditions:default".into(), list(vec!["default"]));
        let sel = SelectorList {
            pieces: vec![SelectorPiece::Selector(Selector {
                branches,
                no_match_error: None,
            })],
        };
        let (pkg, mapping, mut matches) = empty_select_inputs();
        let lbl = |pkg: &str, name: &str| {
            loupe_labels::Label::new(
                loupe_labels::CanonicalRepo::MAIN,
                loupe_labels::PackagePath::new(pkg).unwrap(),
                name,
            )
        };
        let cs_a = lbl("cs", "a");
        let cs_b = lbl("cs", "b");
        let linux = lbl("os", "linux");
        let gcc = lbl("os", "gcc");
        let x86 = lbl("os", "x86");
        let mut s_a = std::collections::BTreeSet::new();
        s_a.insert(linux.clone());
        s_a.insert(gcc);
        let mut s_b = std::collections::BTreeSet::new();
        s_b.insert(linux);
        s_b.insert(x86);
        matches.insert(cs_a, s_a);
        matches.insert(cs_b, s_b);
        let err = resolve_selector(&sel, &pkg, &mapping, &matches).unwrap_err();
        assert!(
            err.to_string().contains("multiple incomparable branches"),
            "expected ambiguity error, got: {err}"
        );
    }

    #[test]
    fn resolve_selector_errors_on_duplicate_match_sets() {
        // Two branches that match with identical constraint sets are
        // also ambiguous — equality fails the strict-superset check
        // in both directions.
        let mut branches = BTreeMap::new();
        branches.insert("//cs:dup_a".into(), list(vec!["a"]));
        branches.insert("//cs:dup_b".into(), list(vec!["b"]));
        let sel = SelectorList {
            pieces: vec![SelectorPiece::Selector(Selector {
                branches,
                no_match_error: None,
            })],
        };
        let (pkg, mapping, mut matches) = empty_select_inputs();
        let lbl = |pkg: &str, name: &str| {
            loupe_labels::Label::new(
                loupe_labels::CanonicalRepo::MAIN,
                loupe_labels::PackagePath::new(pkg).unwrap(),
                name,
            )
        };
        let mut shared = std::collections::BTreeSet::new();
        shared.insert(lbl("os", "linux"));
        matches.insert(lbl("cs", "dup_a"), shared.clone());
        matches.insert(lbl("cs", "dup_b"), shared);
        let err = resolve_selector(&sel, &pkg, &mapping, &matches).unwrap_err();
        assert!(
            err.to_string().contains("multiple incomparable branches"),
            "expected ambiguity error on duplicate sets, got: {err}"
        );
    }

    #[test]
    fn resolve_selector_concatenates_lists_in_piece_order() {
        // `[a] + select({default: [middle]}) + [c]` resolves to the
        // flat list `[a, middle, c]`.
        let mut branches = BTreeMap::new();
        branches.insert("//conditions:default".into(), list(vec!["middle"]));
        let sel = SelectorList {
            pieces: vec![
                SelectorPiece::Concrete(list(vec!["a"])),
                SelectorPiece::Selector(Selector {
                    branches,
                    no_match_error: None,
                }),
                SelectorPiece::Concrete(list(vec!["c"])),
            ],
        };
        let (pkg, mapping, matches) = empty_select_inputs();
        assert_eq!(
            resolve_selector(&sel, &pkg, &mapping, &matches).unwrap(),
            list(vec!["a", "middle", "c"])
        );
    }

    #[test]
    fn resolve_selector_errors_when_no_match_and_no_default() {
        let mut branches = BTreeMap::new();
        branches.insert("//cpu:k8".into(), list(vec!["k8.cc"]));
        let sel = SelectorList {
            pieces: vec![SelectorPiece::Selector(Selector {
                branches,
                no_match_error: Some("explicit error".into()),
            })],
        };
        let (pkg, mapping, matches) = empty_select_inputs();
        let err = resolve_selector(&sel, &pkg, &mapping, &matches).unwrap_err();
        assert!(err.to_string().contains("explicit error"));
    }

    #[test]
    fn provider_id_display_uses_bazel_percent_form() {
        // `<bzl>%<symbol>` matches Bazel's cquery output: e.g.
        // `//:defs.bzl%HelloInfo`. The `bzl` half displays canonically
        // as `@@//:defs.bzl`.
        let id = pid("HelloInfo");
        assert_eq!(id.to_string(), "@@//:defs.bzl%HelloInfo");
    }
}
