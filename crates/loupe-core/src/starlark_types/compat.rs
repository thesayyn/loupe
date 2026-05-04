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
use starlark::values::Value;
use starlark::values::dict::AllocDict;
use starlark::values::none::NoneType;

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
    /// STUB.
    fn repository_rule<'v>(
        #[starlark(require = named)] implementation: Option<Value<'v>>,
        #[starlark(require = named)] attrs: Option<Value<'v>>,
        #[starlark(require = named)] local: Option<bool>,
        #[starlark(require = named)] configure: Option<bool>,
        #[starlark(require = named)] environ: Option<Value<'v>>,
        #[starlark(require = named)] doc: Option<&str>,
        #[starlark(require = named)] remotable: Option<bool>,
    ) -> anyhow::Result<NoneType> {
        let _ = (
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
    fn module_extension<'v>(
        #[starlark(require = named)] implementation: Option<Value<'v>>,
        #[starlark(require = named)] tag_classes: Option<Value<'v>>,
        #[starlark(require = named)] doc: Option<&str>,
        #[starlark(require = named)] environ: Option<Value<'v>>,
        #[starlark(require = named)] os_dependent: Option<bool>,
        #[starlark(require = named)] arch_dependent: Option<bool>,
    ) -> anyhow::Result<NoneType> {
        let _ = (
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

    /// `Label("//pkg:name")` — Bazel's Label-value constructor.
    /// v1 returns the label as a string (Loupe doesn't yet ship a
    /// real Starlark `Label` value type). Loadtime macros that
    /// pass the result through to attr defaults still get the right
    /// canonicalisation because Loupe canonicalises label-typed attr
    /// strings at analysis time.
    #[starlark(as_type = LabelString)]
    fn Label<'v>(#[starlark(require = pos)] label_str: &str) -> anyhow::Result<String> {
        Ok(label_str.to_string())
    }
}

#[derive(
    Debug,
    Clone,
    allocative::Allocative,
    starlark::any::ProvidesStaticType,
    derive_more::Display,
    starlark::values::NoSerialize,
)]
#[display("<LabelString>")]
struct LabelString;
starlark::starlark_simple_value!(LabelString);
#[starlark::values::starlark_value(type = "Label")]
impl<'v> starlark::values::StarlarkValue<'v> for LabelString {}

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
