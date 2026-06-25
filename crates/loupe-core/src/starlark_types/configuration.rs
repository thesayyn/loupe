//! Native `ctx.configuration` and `ctx.fragments.<name>` views.
//!
//! Bazel rules read config-level state two ways: `ctx.configuration` for
//! predicates that don't belong to any single fragment
//! (`is_tool_configuration()`, `host_path_separator`), and
//! `ctx.fragments.<name>` for a fragment's own knobs
//! (`java.use_ijars()`, `cpp.copts`, …).
//!
//! Loupe builds both **natively in Rust** off the analyzed target's
//! [`crate::analysis::Configuration`] — there is no
//! `@bazel_tools//fragments:*` target to analyse and no `FragmentInfo`
//! provider to read back. The engine owns this surface because the
//! values are derived from engine-side configuration state, not from
//! user `.bzl`.
//!
//! v1 models the subset of fields that analyzed rules actually reach
//! (the `rules_oci` / `rules_js` surface plus the wiring test). Anything
//! else is left undefined, so the access raises (`AttributeError`) per
//! DESIGN.md's "real or fail; never silent stubs" — surfacing the gap
//! rather than returning a structurally-valid-but-wrong default. The cc
//! / objc / python fragment knobs the vendored tree reads are not in v1
//! scope (those rules aren't analyzed); they raise here until a fixture
//! makes one of them unavoidable, exactly like the toolchain path did.

use allocative::Allocative;
use anyhow::anyhow;
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::environment::{Methods, MethodsBuilder, MethodsStatic};
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::{Heap, NoSerialize, StarlarkValue, Value, starlark_value};

/// Host path-list separator: `:` on Unix, `;` on Windows. Bazel's
/// `ctx.configuration.host_path_separator` reports the exec host's, not
/// a configured flag.
fn host_path_sep() -> &'static str {
    if cfg!(windows) { ";" } else { ":" }
}

/// `ctx.configuration` — config-level state not owned by a fragment.
/// Built fresh per access from the analyzed target's `Configuration`.
#[derive(Debug, Clone, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("configuration")]
pub struct CtxConfiguration {
    /// Whether this is the exec (tool) configuration. Drives
    /// `is_tool_configuration()`.
    pub is_exec: bool,
}

starlark_simple_value!(CtxConfiguration);

#[starlark_value(type = "configuration")]
impl<'v> StarlarkValue<'v> for CtxConfiguration {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(configuration_methods)
    }
}

#[starlark_module]
fn configuration_methods(builder: &mut MethodsBuilder) {
    /// `ctx.configuration.is_tool_configuration()` — `True` in the exec
    /// configuration. v1 derives it from `Configuration::is_exec`.
    fn is_tool_configuration<'v>(
        #[starlark(this)] this: &CtxConfiguration,
    ) -> anyhow::Result<bool> {
        Ok(this.is_exec)
    }

    /// `ctx.configuration.is_sibling_repository_layout()` — whether
    /// external-repo exec paths use `--experimental_sibling_repository_layout`.
    /// Loupe defaults to the non-sibling layout (Bazel's default), so this
    /// is `False`. The cc toolchain provider helper reads it to derive
    /// solib / runtime-library directories.
    fn is_sibling_repository_layout<'v>(
        #[starlark(this)] this: &CtxConfiguration,
    ) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    /// `ctx.configuration.stamp_binaries()` — whether `--stamp` is set
    /// (embed build info in binaries). Loupe doesn't model `--stamp`;
    /// Bazel's default is off, and the cc toolchain only reads it to
    /// decide whether to wire build-info files, so `False` is the honest
    /// default.
    fn stamp_binaries<'v>(#[starlark(this)] this: &CtxConfiguration) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    /// `ctx.configuration.has_separate_genfiles_directory()` — modern
    /// Bazel merged the legacy genfiles tree into `bin/`, so this is
    /// `False` (matching the `bin_dir`/`genfiles_dir` aliasing Loupe
    /// already models).
    fn has_separate_genfiles_directory<'v>(
        #[starlark(this)] this: &CtxConfiguration,
    ) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    /// `ctx.configuration.host_path_separator` — `:` / `;` from the
    /// exec host OS. Honest default (matches Bazel with no override),
    /// not a stub.
    #[starlark(attribute)]
    fn host_path_separator<'v>(
        #[starlark(this)] this: &CtxConfiguration,
    ) -> anyhow::Result<String> {
        let _ = this;
        Ok(host_path_sep().to_string())
    }

    /// `ctx.configuration.coverage_enabled` — `False` unless
    /// `--collect_code_coverage`, which v1 doesn't model. Honest
    /// Bazel default.
    #[starlark(attribute)]
    fn coverage_enabled<'v>(#[starlark(this)] this: &CtxConfiguration) -> anyhow::Result<bool> {
        let _ = this;
        Ok(false)
    }

    /// `ctx.configuration.default_shell_env` — empty dict. Bazel
    /// populates it from `--action_env`; none modelled, so the honest
    /// default is empty.
    #[starlark(attribute)]
    fn default_shell_env<'v>(
        #[starlark(this)] this: &CtxConfiguration,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = this;
        Ok(heap.alloc(SmallMap::<Value<'v>, Value<'v>>::new()))
    }
}

/// `ctx.fragments.<name>` — one configuration fragment's knobs. Carries
/// the fragment name so method impls can key off it; the configuration
/// state each knob needs is read here, not from an analysed target.
#[derive(Debug, Clone, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("fragment({name})")]
pub struct CtxFragment {
    pub name: String,
    /// `--compilation_mode` carried from the analyzed `Configuration` —
    /// the same value `ctx.compilation_mode` reports. Threaded in at every
    /// construction site so `ctx.fragments.cpp.compilation_mode()` returns
    /// the truth rather than a hardcoded constant. Config-level (identical
    /// across fragments); each fragment view carries it for convenience.
    pub compilation_mode: String,
}

starlark_simple_value!(CtxFragment);

#[starlark_value(type = "fragment")]
impl<'v> StarlarkValue<'v> for CtxFragment {
    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(fragment_methods)
    }
}

#[starlark_module]
fn fragment_methods(builder: &mut MethodsBuilder) {
    /// `ctx.fragments.java.use_ijars()` — whether interface jars are
    /// used. Bazel's `--use_ijars` defaults to `True`; v1 reports that
    /// honest default and raises for any non-java fragment (none of
    /// which expose `use_ijars`).
    fn use_ijars<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        if this.name == "java" {
            Ok(true)
        } else {
            Err(anyhow!(
                "ctx.fragments.{}.use_ijars() is not modeled in v1",
                this.name
            ))
        }
    }

    /// `ctx.fragments.python.python_path` (and the legacy `bazel_py`
    /// fragment alias py_executable.bzl also reads) — the Python
    /// interpreter path (Bazel's `--python_path`, default `python3`).
    /// py_executable's `_get_runtime_details` reads it; analysis is
    /// read-only and never execs the interpreter, so the default is inert
    /// but must be a real string. Raises for any other fragment.
    #[starlark(attribute)]
    fn python_path<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<String> {
        if this.name == "python" || this.name == "bazel_py" {
            Ok("python3".to_string())
        } else {
            Err(anyhow!(
                "ctx.fragments.{}.python_path is not modeled in v1",
                this.name
            ))
        }
    }

    /// `--python_import_all_repositories`, Bazel default on. (attribute on
    /// the `bazel_py` fragment — drives the stub launcher's `%import_all%`)
    #[starlark(attribute)]
    fn python_import_all_repositories<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<bool> {
        if this.name == "bazel_py" || this.name == "python" {
            Ok(true)
        } else {
            Err(anyhow!(
                "ctx.fragments.{}.python_import_all_repositories is not modeled in v1",
                this.name
            ))
        }
    }

    /// `ctx.fragments.apple.single_arch_platform` — the single-arch Apple
    /// platform being built for. The autoconfigured darwin cc toolchain
    /// config (`_target_os_version`) reads `.platform_type` off it. Loupe
    /// analyses the host cc toolchain, so the darwin toolchain's platform
    /// is the host's: `platform_type = "macos"`. Returns a struct with
    /// that field. Raises for non-apple fragments.
    #[starlark(attribute)]
    fn single_arch_platform<'v>(
        #[starlark(this)] this: &CtxFragment,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        if this.name == "apple" {
            Ok(heap.alloc(starlark::values::structs::AllocStruct([(
                "platform_type",
                "macos",
            )])))
        } else {
            Err(anyhow!(
                "ctx.fragments.{}.single_arch_platform is not modeled in v1",
                this.name
            ))
        }
    }

    /// `ctx.fragments.cpp.compilation_mode()` — the C++ compilation mode
    /// (`--compilation_mode`). Returns the real value threaded from the
    /// analyzed `Configuration` (same as `ctx.compilation_mode`), not a
    /// constant — `fastbuild` by default since Loupe doesn't model
    /// `--compilation_mode` being set. Raises for non-cpp fragments.
    fn compilation_mode<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<String> {
        if this.name == "cpp" {
            Ok(this.compilation_mode.clone())
        } else {
            Err(anyhow!(
                "ctx.fragments.{}.compilation_mode() is not modeled in v1",
                this.name
            ))
        }
    }

    /// `ctx.fragments.cpp.minimum_os_version()` — `--minimum_os_version`,
    /// `None` unless set. The cc toolchain provider helper only adds a
    /// `minimum_os_version` build variable when it's non-None, so the
    /// honest default (unset → `None`) is inert. Raises for non-cpp.
    fn minimum_os_version<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        cpp_none_default(this, "minimum_os_version")
    }

    /// `ctx.fragments.cpp.force_pic()` — `--force_pic`, default off.
    /// `CcToolchainInfo`'s init reads it to pick the
    /// `needs_pic_for_dynamic_libraries` strategy; Bazel's default is
    /// `False`. Raises for non-cpp fragments.
    fn force_pic<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        cpp_bool_default(this, "force_pic", false)
    }

    // The cpp-fragment knobs below are read by `configure_features` (run
    // by both the cc_toolchain rule and cc_library/cc_binary consumers)
    // to gate optional features. `CtxFragment` carries no `Configuration`,
    // so each returns Bazel's default for the unset flag — inert for
    // read-only analysis (no cc actions). All raise for non-cpp.

    /// `--apple_generate_dsym`, default off. (attribute)
    #[starlark(attribute)]
    fn apple_generate_dsym<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        cpp_bool_default(this, "apple_generate_dsym", false)
    }

    /// `--objc_generate_linkmap`, default off. (attribute)
    #[starlark(attribute)]
    fn objc_generate_linkmap<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        cpp_bool_default(this, "objc_generate_linkmap", false)
    }

    /// Whether objc binaries are stripped, default off. (attribute)
    #[starlark(attribute)]
    fn objc_should_strip_binary<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        cpp_bool_default(this, "objc_should_strip_binary", false)
    }

    /// Internal host/nonhost feature toggle, default off. (attribute)
    #[starlark(attribute)]
    fn _dont_enable_host_nonhost<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        cpp_bool_default(this, "_dont_enable_host_nonhost", false)
    }

    /// `--fdo_prefetch_hints` label, unset → `None`. (attribute)
    #[starlark(attribute)]
    fn _fdo_prefetch_hints_label<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        cpp_none_default(this, "_fdo_prefetch_hints_label")
    }

    /// `--fdo_instrument`, unset → `None`. (method)
    fn fdo_instrument<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        cpp_none_default(this, "fdo_instrument")
    }

    /// `--cs_fdo_instrument`, unset → `None`. (method)
    fn cs_fdo_instrument<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        cpp_none_default(this, "cs_fdo_instrument")
    }

    /// `--grte_top` label, unset → `None`. (method)
    fn grte_top<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        cpp_none_default(this, "grte_top")
    }

    /// `--experimental_cc_implementation_deps`, default off. (method)
    fn experimental_cc_implementation_deps<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<bool> {
        cpp_bool_default(this, "experimental_cc_implementation_deps", false)
    }

    /// `--custom_malloc` label, unset → `None`. (attribute)
    #[starlark(attribute)]
    fn custom_malloc<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        cpp_none_default(this, "custom_malloc")
    }

    /// `--disable_nocopts`, default off. (method)
    fn disable_nocopts<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        cpp_bool_default(this, "disable_nocopts", false)
    }

    /// `--experimental_cpp_modules`, default off. (method)
    fn experimental_cpp_modules<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        cpp_bool_default(this, "experimental_cpp_modules", false)
    }

    /// `--experimental_link_static_libraries_once`, default on. (method)
    fn experimental_link_static_libraries_once<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<bool> {
        cpp_bool_default(this, "experimental_link_static_libraries_once", true)
    }

    /// `--experimental_use_llvm_covmap`, default off. (method)
    fn use_llvm_coverage_map_format<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<bool> {
        cpp_bool_default(this, "use_llvm_coverage_map_format", false)
    }

    // `--save_temps` artifacts are produced by compile actions Loupe never
    // runs, so report off. `.d` dependency files, however, are reflected in
    // the recorded compile argv (`-MD -MF …/foo.d`), so this matches Bazel's
    // default-on (`CppConfiguration.shouldGenerateDotdFiles`).

    /// Whether `.d` dependency files are generated, default on (Bazel
    /// default; drives the `-MD -MF` flags in the recorded argv). (method)
    fn should_generate_dotd_files<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<bool> {
        cpp_bool_default(this, "should_generate_dotd_files", true)
    }

    /// objc variant of `should_generate_dotd_files`, default off. (method)
    fn objc_should_generate_dotd_files<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<bool> {
        cpp_bool_default(this, "objc_should_generate_dotd_files", false)
    }

    /// `--process_headers_in_dependencies`, default off. (method)
    fn process_headers_in_dependencies<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<bool> {
        cpp_bool_default(this, "process_headers_in_dependencies", false)
    }

    /// `--save_temps`, default off. (method)
    fn save_temps<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        cpp_bool_default(this, "save_temps", false)
    }

    // Per-language extra-copt flags from `--copt`/`--cxxopt`/etc. They
    // only feed compile command lines Loupe never emits; empty is faithful
    // for the default config.

    /// `--copt`, default empty. (attribute — accessed as `cpp.copts`)
    #[starlark(attribute)]
    fn copts<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<Vec<String>> {
        cpp_empty_list(this, "copts")
    }

    /// `--conlyopt`, default empty. (attribute)
    #[starlark(attribute)]
    fn conlyopts<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<Vec<String>> {
        cpp_empty_list(this, "conlyopts")
    }

    /// `--cxxopt`, default empty. (attribute)
    #[starlark(attribute)]
    fn cxxopts<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<Vec<String>> {
        cpp_empty_list(this, "cxxopts")
    }

    /// `--objccopt`, default empty. (attribute)
    #[starlark(attribute)]
    fn objccopts<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<Vec<String>> {
        cpp_empty_list(this, "objccopts")
    }

    /// `--save_feature_state`, default off. (method)
    fn save_feature_state<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        cpp_bool_default(this, "save_feature_state", false)
    }

    /// `--build_test_dwp`, default off. (method)
    fn build_test_dwp<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        cpp_bool_default(this, "build_test_dwp", false)
    }

    /// `--stripopt`, default empty. (method)
    fn strip_opts<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<Vec<String>> {
        cpp_empty_list(this, "strip_opts")
    }

    /// `--fission` active for the current compilation mode, default off
    /// (fastbuild never enables fission). (method)
    fn fission_active_for_current_compilation_mode<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<bool> {
        cpp_bool_default(this, "fission_active_for_current_compilation_mode", false)
    }

    /// `--experimental_layering_check_allowlisted_macros`, default empty.
    /// (method)
    fn extra_allowlisted_feature_layering_check_macros<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<Vec<String>> {
        cpp_empty_list(this, "extra_allowlisted_feature_layering_check_macros")
    }

    /// Features forced on for layering-check, default empty. (method)
    fn force_layering_check_features<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<Vec<String>> {
        cpp_empty_list(this, "force_layering_check_features")
    }

    /// `--linkopt`, default empty. (attribute — `cpp.linkopts`)
    #[starlark(attribute)]
    fn linkopts<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<Vec<String>> {
        cpp_empty_list(this, "linkopts")
    }

    /// `--lto_backend_option`, default empty. (attribute)
    #[starlark(attribute)]
    fn lto_backend_options<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<Vec<String>> {
        cpp_empty_list(this, "lto_backend_options")
    }

    /// `--lto_index_option`, default empty. (method)
    fn lto_index_options<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<Vec<String>> {
        cpp_empty_list(this, "lto_index_options")
    }

    /// `--interface_shared_objects`, Bazel default on. (method)
    fn interface_shared_objects<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        cpp_bool_default(this, "interface_shared_objects", true)
    }

    /// `--share_native_deps`, Bazel default on. (method)
    fn share_native_deps<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        cpp_bool_default(this, "share_native_deps", true)
    }

    /// `--start_end_lib`, Bazel default on. (method)
    fn start_end_lib<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        cpp_bool_default(this, "start_end_lib", true)
    }

    /// `--incompatible_use_specific_tool_files`, default on. (method)
    fn incompatible_use_specific_tool_files<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<bool> {
        cpp_bool_default(this, "incompatible_use_specific_tool_files", true)
    }

    /// `--incompatible_remove_legacy_whole_archive`, default on. (method)
    fn incompatible_remove_legacy_whole_archive<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<bool> {
        cpp_bool_default(this, "incompatible_remove_legacy_whole_archive", true)
    }

    /// `--legacy_whole_archive`, deprecated, default off. (method)
    fn legacy_whole_archive<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        cpp_bool_default(this, "legacy_whole_archive", false)
    }

    /// Whether binaries are stripped — fastbuild never strips. (method)
    fn should_strip_binaries<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        cpp_bool_default(this, "should_strip_binaries", false)
    }

    /// `--experimental_generate_llvm_lcov`, default off. (method)
    fn generate_llvm_lcov<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        cpp_bool_default(this, "generate_llvm_lcov", false)
    }

    /// `--incompatible_use_python_toolchains` — modern Python resolves its
    /// runtime via toolchains; non-configurable and on in current Bazel.
    /// (attribute on the `py` fragment)
    #[starlark(attribute)]
    fn use_toolchains<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        py_bool_default(this, "use_toolchains", true)
    }

    /// `--build_python_zip`, default off (auto → off on non-Windows hosts).
    /// (attribute on the `py` fragment)
    #[starlark(attribute)]
    fn build_python_zip<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        py_bool_default(this, "build_python_zip", false)
    }

    /// `--incompatible_default_to_explicit_init_py`, default off.
    /// (attribute on the `py` fragment)
    #[starlark(attribute)]
    fn default_to_explicit_init_py<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<bool> {
        py_bool_default(this, "default_to_explicit_init_py", false)
    }

    /// Whether the `py` fragment is disabled, default off. (attribute on
    /// the `py` fragment)
    #[starlark(attribute)]
    fn disable_py<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<bool> {
        py_bool_default(this, "disable_py", false)
    }

    /// `--protocopt` extra protoc flags, default empty. (attribute on the
    /// `proto` fragment — `ctx.fragments.proto.experimental_protoc_opts`)
    #[starlark(attribute)]
    fn experimental_protoc_opts<'v>(
        #[starlark(this)] this: &CtxFragment,
    ) -> anyhow::Result<Vec<String>> {
        if this.name == "proto" {
            Ok(Vec::new())
        } else {
            Err(anyhow!(
                "ctx.fragments.{}.experimental_protoc_opts is not modeled in v1",
                this.name
            ))
        }
    }

    /// `--dynamic_mode`, Bazel default `DEFAULT`. (method)
    fn dynamic_mode<'v>(#[starlark(this)] this: &CtxFragment) -> anyhow::Result<String> {
        cpp_str_default(this, "dynamic_mode", "DEFAULT")
    }
}

/// cpp-fragment string knob with a fixed default; raises for non-cpp.
fn cpp_str_default(this: &CtxFragment, knob: &str, default: &str) -> anyhow::Result<String> {
    if this.name == "cpp" {
        Ok(default.to_string())
    } else {
        Err(anyhow!(
            "ctx.fragments.{}.{} is not modeled in v1",
            this.name,
            knob
        ))
    }
}

/// cpp-fragment list knob, always empty in v1's default config; raises
/// for non-cpp.
fn cpp_empty_list(this: &CtxFragment, knob: &str) -> anyhow::Result<Vec<String>> {
    if this.name == "cpp" {
        Ok(Vec::new())
    } else {
        Err(anyhow!(
            "ctx.fragments.{}.{} is not modeled in v1",
            this.name,
            knob
        ))
    }
}

/// py-fragment bool knob with a fixed default; raises for non-py.
fn py_bool_default(this: &CtxFragment, knob: &str, default: bool) -> anyhow::Result<bool> {
    if this.name == "py" {
        Ok(default)
    } else {
        Err(anyhow!(
            "ctx.fragments.{}.{} is not modeled in v1",
            this.name,
            knob
        ))
    }
}

/// cpp-fragment bool knob with a fixed default; raises for non-cpp.
fn cpp_bool_default(this: &CtxFragment, knob: &str, default: bool) -> anyhow::Result<bool> {
    if this.name == "cpp" {
        Ok(default)
    } else {
        Err(anyhow!(
            "ctx.fragments.{}.{} is not modeled in v1",
            this.name,
            knob
        ))
    }
}

/// cpp-fragment knob that is `None` when its flag is unset; raises for non-cpp.
fn cpp_none_default(
    this: &CtxFragment,
    knob: &str,
) -> anyhow::Result<starlark::values::none::NoneType> {
    if this.name == "cpp" {
        Ok(starlark::values::none::NoneType)
    } else {
        Err(anyhow!(
            "ctx.fragments.{}.{} is not modeled in v1",
            this.name,
            knob
        ))
    }
}
