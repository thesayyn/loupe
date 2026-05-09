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
pub fn compat_globals(b: &mut GlobalsBuilder) {
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

/// Pass-through "any field works" struct. Narrowly scoped: only
/// used as the runtime shape of `_builtins.internal` during
/// embedded-tree evaluation. Vendored Bazel `builtins_bzl/`
/// files reach for `_builtins.internal.cc_common`, `cc_internal`,
/// `py_builtins`, `apple_common`, `java_common_internal_do_not_use`
/// at module-eval time and call methods on the resulting nested
/// structs from inside function bodies. None of those host
/// machinery values are modelled in Loupe; the wildcard lets the
/// embedded files freeze without a per-name stub avalanche.
///
/// **Scope:** `pub(crate)` — never reachable from user `.bzl` /
/// BUILD code. `target[<provider>]` misses, `ctx.toolchains[T]`
/// misses, and depset entry-type errors raise instead.
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
#[display("<wildcard-stub>")]
pub(crate) struct WildcardStruct;

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
