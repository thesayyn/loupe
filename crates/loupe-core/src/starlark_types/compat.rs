//! Compatibility no-op stubs for Bazel/Starlark APIs Loupe doesn't
//! model but rules and macros call defensively (or for visibility /
//! diagnostics that v1 doesn't enforce).
//!
//! Each entry here is a pragmatic STUB. Adding one is preferable to
//! teaching the loader to skip the call, because Starlark's evaluator
//! reports the error at the call site (where the user can see it),
//! while a loader-level skip would silently drop semantics.

use starlark::environment::GlobalsBuilder;
use starlark::starlark_module;
use starlark::values::dict::AllocDict;
use starlark::values::none::NoneType;
use starlark::values::{Value, ValueLike};

#[starlark_module]
#[allow(clippy::too_many_arguments)] // Bazel surface; STUBs match upstream
pub fn compat_globals(b: &mut GlobalsBuilder) {
    /// `visibility(value)` — Starlark macro-author primitive that
    /// restricts which packages may `load()` the enclosing `.bzl`.
    /// v1 doesn't enforce visibility (DESIGN.md "what not to
    /// implement at v1"), so we accept and ignore.
    fn visibility<'v>(#[starlark(require = pos)] _value: Value<'v>) -> anyhow::Result<NoneType> {
        Ok(NoneType)
    }

    /// `repository_rule(implementation, attrs=…, …)` — declares a
    /// repository rule. v1 doesn't fetch (deps assumed vendored), so
    /// the constructor returns `None`. `.bzl`s that bind the result
    /// to a name (e.g. `http_archive = repository_rule(...)`) end up
    /// with a `None` value; that's fine because nothing in the load
    /// graph subsequently *calls* the result during BUILD/analysis.
    /// STUB. Accepts `implementation` either positional or named.
    fn repository_rule<'v>(
        #[starlark(require = pos)] implementation_pos: Option<Value<'v>>,
        #[starlark(require = named)] implementation: Option<Value<'v>>,
        #[starlark(require = named)] attrs: Option<Value<'v>>,
        #[starlark(require = named)] local: Option<bool>,
        #[starlark(require = named)] configure: Option<bool>,
        #[starlark(require = named)] environ: Option<Value<'v>>,
        #[starlark(require = named)] doc: Option<&str>,
        #[starlark(require = named)] remotable: Option<bool>,
    ) -> anyhow::Result<NoneType> {
        let _ = (
            implementation_pos,
            implementation,
            attrs,
            local,
            configure,
            environ,
            doc,
            remotable,
        );
        Ok(NoneType)
    }

    /// `module_extension(implementation, tag_classes=…, …)` —
    /// bzlmod module extension declaration. STUB: returns `None`.
    /// `implementation` accepted positional or named.
    fn module_extension<'v>(
        #[starlark(require = pos)] implementation_pos: Option<Value<'v>>,
        #[starlark(require = named)] implementation: Option<Value<'v>>,
        #[starlark(require = named)] tag_classes: Option<Value<'v>>,
        #[starlark(require = named)] doc: Option<&str>,
        #[starlark(require = named)] environ: Option<Value<'v>>,
        #[starlark(require = named)] os_dependent: Option<bool>,
        #[starlark(require = named)] arch_dependent: Option<bool>,
    ) -> anyhow::Result<NoneType> {
        let _ = (
            implementation_pos,
            implementation,
            tag_classes,
            doc,
            environ,
            os_dependent,
            arch_dependent,
        );
        Ok(NoneType)
    }

    /// `tag_class(attrs=…, doc=…)` — declares a tag class for
    /// module extensions. STUB.
    fn tag_class<'v>(
        #[starlark(require = named)] attrs: Option<Value<'v>>,
        #[starlark(require = named)] doc: Option<&str>,
    ) -> anyhow::Result<NoneType> {
        let _ = (attrs, doc);
        Ok(NoneType)
    }

    /// `licenses(["notice"])` — legacy Bazel BUILD-level declaration.
    /// STUB: accept-and-ignore.
    fn licenses<'v>(#[starlark(require = pos)] _kinds: Value<'v>) -> anyhow::Result<NoneType> {
        Ok(NoneType)
    }

    /// `package(default_visibility=…, …)` — BUILD-file-level
    /// directive. STUB.
    fn package<'v>(
        #[starlark(require = named)] default_visibility: Option<Value<'v>>,
        #[starlark(require = named)] default_testonly: Option<bool>,
        #[starlark(require = named)] default_deprecation: Option<&str>,
        #[starlark(require = named)] features: Option<Value<'v>>,
        #[starlark(require = named)] default_applicable_licenses: Option<Value<'v>>,
        #[starlark(require = named)] default_package_metadata: Option<Value<'v>>,
    ) -> anyhow::Result<NoneType> {
        let _ = (
            default_visibility,
            default_testonly,
            default_deprecation,
            features,
            default_applicable_licenses,
            default_package_metadata,
        );
        Ok(NoneType)
    }

    /// `package_group(name, packages=…, includes=…)`. STUB.
    fn package_group<'v>(
        #[starlark(require = named)] name: Option<&str>,
        #[starlark(require = named)] packages: Option<Value<'v>>,
        #[starlark(require = named)] includes: Option<Value<'v>>,
    ) -> anyhow::Result<NoneType> {
        let _ = (name, packages, includes);
        Ok(NoneType)
    }

    /// `Label("//pkg:name")` — Bazel's Label-value constructor.
    /// Returns a [`LabelValue`] with `name`, `package`, `workspace_name`,
    /// `repo_name` attrs that mirror Bazel. Stringifies back to the
    /// canonical form. Construction doesn't canonicalise (we don't have
    /// the caller's `repo_mapping` here); analysis-time consumers that
    /// need canonical labels should use the recorded
    /// `UnconfiguredTarget.attrs` instead.
    fn Label<'v>(#[starlark(require = pos)] label_str: &str) -> anyhow::Result<LabelValue> {
        Ok(LabelValue::parse(label_str))
    }

    /// `macro(implementation, attrs=…, doc=…, finalizer=…)` —
    /// Bazel's symbolic macro declaration. STUB: returns the impl
    /// callable so `mymacro = macro(implementation = _impl)` then
    /// invoking `mymacro(...)` reaches the impl. Good enough for v1,
    /// where macro semantics (per-package state, rule-call
    /// interception) aren't modelled.
    fn r#macro<'v>(
        #[starlark(require = named)] implementation: Option<Value<'v>>,
        #[starlark(require = named)] attrs: Option<Value<'v>>,
        #[starlark(require = named)] doc: Option<&str>,
        #[starlark(require = named)] finalizer: Option<bool>,
        #[starlark(require = named)] inherit_attrs: Option<Value<'v>>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (attrs, doc, finalizer, inherit_attrs);
        Ok(implementation.unwrap_or_else(starlark::values::Value::new_none))
    }

    /// `subrule(implementation, …)` — Bazel's experimental subrule.
    /// STUB.
    fn subrule<'v>(
        #[starlark(require = named)] implementation: Option<Value<'v>>,
        #[starlark(require = named)] attrs: Option<Value<'v>>,
        #[starlark(require = named)] toolchains: Option<Value<'v>>,
        #[starlark(require = named)] fragments: Option<Value<'v>>,
        #[starlark(require = named)] subrules: Option<Value<'v>>,
    ) -> anyhow::Result<NoneType> {
        let _ = (implementation, attrs, toolchains, fragments, subrules);
        Ok(NoneType)
    }

    /// `transition(implementation, inputs=…, outputs=…)` — Bazel's
    /// configuration-transition declaration. STUB: returns the
    /// implementation callable so `mytrans = transition(...)` and
    /// `attr.label(cfg = mytrans)` both work syntactically.
    fn transition<'v>(
        #[starlark(require = named)] implementation: Option<Value<'v>>,
        #[starlark(require = named)] inputs: Option<Value<'v>>,
        #[starlark(require = named)] outputs: Option<Value<'v>>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (inputs, outputs);
        Ok(implementation.unwrap_or_else(starlark::values::Value::new_none))
    }

    /// `analysis_test_transition(settings)`. STUB.
    fn analysis_test_transition<'v>(
        #[starlark(require = named)] settings: Option<Value<'v>>,
    ) -> anyhow::Result<NoneType> {
        let _ = settings;
        Ok(NoneType)
    }

    /// `exec_group(toolchains=…, exec_compatible_with=…)`. STUB.
    fn exec_group<'v>(
        #[starlark(require = named)] toolchains: Option<Value<'v>>,
        #[starlark(require = named)] exec_compatible_with: Option<Value<'v>>,
    ) -> anyhow::Result<NoneType> {
        let _ = (toolchains, exec_compatible_with);
        Ok(NoneType)
    }

    // The following are pre-declared Bazel built-in providers that
    // some bzlmod helpers reference by bare name in `getattr(...,
    // default = X)` patterns (e.g. `bazel_features_globals/globals.bzl`).
    // v1 declares them as `None` placeholders so the bare-name lookup
    // resolves; rules that actually consume these will see `None` and
    // either fail loudly or pick a fallback path, which is the right
    // behaviour for "not implemented" at v1.

    #[starlark(speculative_exec_safe)]
    const PackageSpecificationInfo: NoneType = NoneType;
    #[starlark(speculative_exec_safe)]
    const RunEnvironmentInfo: NoneType = NoneType;
    #[starlark(speculative_exec_safe)]
    const ProtoInfo: NoneType = NoneType;
    #[starlark(speculative_exec_safe)]
    const PyInfo: NoneType = NoneType;
    #[starlark(speculative_exec_safe)]
    const PyRuntimeInfo: NoneType = NoneType;
    #[starlark(speculative_exec_safe)]
    const PyCcLinkParamsProvider: NoneType = NoneType;
    #[starlark(speculative_exec_safe)]
    const JavaInfo: NoneType = NoneType;
    #[starlark(speculative_exec_safe)]
    const JavaPluginInfo: NoneType = NoneType;
    #[starlark(speculative_exec_safe)]
    const CcSharedLibraryInfo: NoneType = NoneType;
    #[starlark(speculative_exec_safe)]
    const CcSharedLibraryHintInfo: NoneType = NoneType;
    #[starlark(speculative_exec_safe)]
    const cc_proto_aspect: NoneType = NoneType;
}

/// Simple Starlark `Label` value type. Holds the original label
/// string verbatim and a parsed view of its components.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    allocative::Allocative,
    starlark::any::ProvidesStaticType,
    derive_more::Display,
    starlark::values::NoSerialize,
)]
#[display("{}", raw)]
pub struct LabelValue {
    pub raw: String,
    pub repo: String,
    pub package: String,
    pub name: String,
}

impl LabelValue {
    pub fn parse(s: &str) -> Self {
        let (repo, rest) = if let Some(no_at) = s.strip_prefix("@@") {
            match no_at.find("//") {
                Some(i) => (no_at[..i].to_string(), &no_at[i + 2..]),
                None => (String::new(), no_at),
            }
        } else if let Some(no_at) = s.strip_prefix('@') {
            match no_at.find("//") {
                Some(i) => (no_at[..i].to_string(), &no_at[i + 2..]),
                None => (String::new(), no_at),
            }
        } else if let Some(rest) = s.strip_prefix("//") {
            (String::new(), rest)
        } else {
            (String::new(), s)
        };
        let (pkg, name) = match rest.split_once(':') {
            Some((p, n)) => (p.to_string(), n.to_string()),
            None => {
                let last = rest.rsplit('/').next().unwrap_or(rest);
                (rest.to_string(), last.to_string())
            }
        };
        LabelValue {
            raw: s.to_string(),
            repo,
            package: pkg,
            name,
        }
    }
}

starlark::starlark_simple_value!(LabelValue);

#[starlark::values::starlark_value(type = "Label")]
impl<'v> starlark::values::StarlarkValue<'v> for LabelValue {
    fn get_attr(
        &self,
        attribute: &str,
        heap: starlark::values::Heap<'v>,
    ) -> Option<starlark::values::Value<'v>> {
        match attribute {
            "name" => Some(heap.alloc(self.name.as_str())),
            "package" => Some(heap.alloc(self.package.as_str())),
            "workspace_root" | "workspace_name" | "repo_name" => {
                Some(heap.alloc(self.repo.as_str()))
            }
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        ["name", "package", "workspace_name", "repo_name"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn equals(&self, other: starlark::values::Value<'v>) -> starlark::Result<bool> {
        Ok(other
            .downcast_ref::<LabelValue>()
            .map(|o| self == o)
            .unwrap_or_else(|| {
                other
                    .unpack_str()
                    .map(|s| s == self.raw.as_str())
                    .unwrap_or(false)
            }))
    }

    /// `Label` values are hashable in Bazel — rulesets use them as
    /// `select({Label("..."): ...})` dict keys (e.g. rules_js's
    /// `enable_runfiles` config setting). Hash off `raw` so the
    /// `equals` round-trip through string-form lines up.
    fn write_hash(
        &self,
        hasher: &mut starlark::collections::StarlarkHasher,
    ) -> starlark::Result<()> {
        std::hash::Hash::hash(&self.raw, hasher);
        Ok(())
    }
}

/// Pass-through "any field works" struct. Used as a STUB stand-in
/// for `ctx.toolchains[<type>]` when Loupe has no real toolchain data
/// for the type — every attribute access returns another
/// `WildcardStruct`, so a chain like `regctl_info.crane_info.version`
/// doesn't error.
///
/// **STUB:** real toolchain resolution is the next milestone. This
/// type exists so the rules_oci probe progresses past the
/// `Object of type 'struct' has no attribute 'regctl_info'` wall;
/// the values it returns are syntactically present but semantically
/// empty (stringify as `"<toolchain-stub>"`, equal nothing, falsy on
/// truthiness checks). Expect rules that *use* these values
/// downstream (e.g. pass `version` as an action arg) to misbehave.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    allocative::Allocative,
    starlark::any::ProvidesStaticType,
    derive_more::Display,
    starlark::values::NoSerialize,
)]
#[display("<toolchain-stub>")]
pub struct WildcardStruct;

starlark::starlark_simple_value!(WildcardStruct);

#[starlark::values::starlark_value(type = "wildcard_struct")]
impl<'v> starlark::values::StarlarkValue<'v> for WildcardStruct {
    fn get_attr(
        &self,
        _attribute: &str,
        heap: starlark::values::Heap<'v>,
    ) -> Option<starlark::values::Value<'v>> {
        // STUB: any attribute resolves to another WildcardStruct so
        // arbitrarily-deep `a.b.c.d` chains work.
        Some(heap.alloc(WildcardStruct))
    }

    fn dir_attr(&self) -> Vec<String> {
        Vec::new()
    }

    fn at(
        &self,
        _index: starlark::values::Value<'v>,
        heap: starlark::values::Heap<'v>,
    ) -> starlark::Result<starlark::values::Value<'v>> {
        // STUB: `wildcard[k]` returns another WildcardStruct.
        Ok(heap.alloc(WildcardStruct))
    }

    fn length(&self) -> starlark::Result<i32> {
        Ok(0)
    }

    fn to_bool(&self) -> bool {
        // STUB: falsy by default — most rules guard `if toolchain:`
        // with truthiness, so falsy keeps them on the unconfigured
        // path.
        false
    }

    fn equals(&self, other: starlark::values::Value<'v>) -> starlark::Result<bool> {
        Ok(other.downcast_ref::<WildcardStruct>().is_some())
    }

    /// `WildcardStruct(...)` — callable. Used as the return value of
    /// `use_extension(...)` so chains like
    /// `ext = use_extension(...); ext.tag_class_method(...)` and
    /// `ext.attr.subattr()` work without modelling the extension's
    /// tag classes. Always returns another WildcardStruct, ignoring
    /// args.
    fn invoke(
        &self,
        _me: starlark::values::Value<'v>,
        _args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> starlark::Result<starlark::values::Value<'v>> {
        Ok(eval.heap().alloc(WildcardStruct))
    }
}

/// Lenient struct: real fields fall through normally, unknown fields
/// resolve to a fresh [`WildcardStruct`]. Used as the runtime shape
/// of `_builtins.toplevel` / `_builtins.internal`: vendored Bazel
/// `builtins_bzl/*.bzl` files reach for both modelled names
/// (`provider`, `rule`, `DefaultInfo`, …) and unmodelled ones
/// (`exec_transition`, `cc_internal`, `py_builtins`, …) through that
/// namespace. Real-name lookups must return the actual primitive so
/// `_builtins.toplevel.provider(...)` constructs a real `ProviderType`;
/// unknown lookups return a wildcard so the chain doesn't crash.
#[derive(
    Debug,
    Clone,
    allocative::Allocative,
    starlark::any::ProvidesStaticType,
    derive_more::Display,
    starlark::values::NoSerialize,
)]
#[display("<lenient struct>")]
pub struct LenientStruct {
    /// Modelled names → frozen value. `FrozenValue` is `'static`, so
    /// this map is heap-anchored on whichever `Module` originally
    /// allocated the values (typically the embedded `wrappers.bzl`
    /// module — `inject_builtins_into` anchors that heap on the user
    /// module, keeping the values valid for the user module's life.).
    pub fields: starlark::collections::SmallMap<String, starlark::values::FrozenValue>,
}

starlark::starlark_simple_value!(LenientStruct);

#[starlark::values::starlark_value(type = "lenient_struct")]
impl<'v> starlark::values::StarlarkValue<'v> for LenientStruct {
    fn get_attr(
        &self,
        attribute: &str,
        heap: starlark::values::Heap<'v>,
    ) -> Option<starlark::values::Value<'v>> {
        if let Some(v) = self.fields.get(attribute) {
            Some(v.to_value())
        } else {
            // Unknown attribute → wildcard; vendored bazel files'
            // `_builtins.internal.cc_common` and similar resolve here.
            Some(heap.alloc(WildcardStruct))
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        self.fields.keys().cloned().collect()
    }

    fn equals(&self, other: starlark::values::Value<'v>) -> starlark::Result<bool> {
        Ok(other.downcast_ref::<LenientStruct>().is_some())
    }
}

/// `native.*` — the legacy macro API. Pre-`.bzl`, BUILD files used
/// built-in functions like `glob`, `package_name`. When `.bzl`
/// arrived, those functions needed to be reachable from macros (which
/// run during loading), so they got exposed under `native.`.
///
/// v1 ships the read-only handful: `package_name`, `repository_name`,
/// `existing_rule(s)`. `native.glob` lives on the BUILD-phase globals
/// only (not under `native.` in `.bzl` because Loupe doesn't reach
/// the active BUILD context from a macro yet — STUB the namespace
/// entry for now).
#[starlark_module]
pub fn native_namespace(b: &mut GlobalsBuilder) {
    /// `native.package_name()` — name of the BUILD package the
    /// macro is being called from. STUB: returns `""` (root package).
    /// Real plumbing requires the BUILD evaluator to thread the
    /// package id into the `.bzl` macro frame.
    fn package_name<'v>() -> anyhow::Result<String> {
        Ok(String::new())
    }

    /// `native.repository_name()` — the canonical repo name of the
    /// BUILD package. STUB: returns `"@@"` (main repo).
    fn repository_name<'v>() -> anyhow::Result<String> {
        Ok("@@".to_string())
    }

    /// `native.existing_rule(name)` — returns the rule dict if
    /// `name` was already declared, else `None`. STUB: always `None`
    /// (Loupe evaluates BUILD files in declaration order; a real
    /// implementation reads the in-flight BUILD evaluation's target
    /// table). Per DETAILS.md, "order-dependent and rarely critical."
    fn existing_rule<'v>(#[starlark(require = pos)] _name: &str) -> anyhow::Result<NoneType> {
        Ok(NoneType)
    }

    /// `native.existing_rules()` — all already-declared rules.
    /// STUB: empty dict.
    fn existing_rules<'v>(heap: starlark::values::Heap<'v>) -> anyhow::Result<Value<'v>> {
        let entries: starlark::collections::SmallMap<&str, Value<'v>> =
            starlark::collections::SmallMap::new();
        Ok(heap.alloc(AllocDict(entries)))
    }
}

/// Wires the `native.*` namespace into both bzl- and analysis-phase
/// globals. BUILD-phase globals already expose `glob`, `package_name`,
/// etc. as bare names; `native.` is the same surface re-exposed under
/// the namespace so macros can reach them.
pub fn register_native(builder: &mut GlobalsBuilder) {
    builder.namespace("native", native_namespace);
}
