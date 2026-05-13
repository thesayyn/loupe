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
use starlark::values::ValueLike;

#[starlark_module]
pub fn register_label(b: &mut GlobalsBuilder) {
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

    fn get_methods() -> Option<&'static starlark::environment::Methods> {
        static RES: starlark::environment::MethodsStatic =
            starlark::environment::MethodsStatic::new();
        RES.methods(label_value_methods)
    }
}

#[starlark::starlark_module]
fn label_value_methods(builder: &mut starlark::environment::MethodsBuilder) {
    /// `Label.relative(s)` — Bazel's relative-resolution helper.
    /// Re-bases a label string against this label's package. Same
    /// shorthand surface a BUILD file's `load(":foo")` resolves
    /// through. Vendored `wrappers.bzl::ctx_expand_location` uses it
    /// to canonicalise `$(location //pkg:tgt)` directives' targets
    /// against the rule's package.
    ///
    /// `s` shapes:
    /// - `:name` — same package, named `name`.
    /// - `//pkg:name` — same repo, absolute package.
    /// - `@repo//pkg:name` — different repo.
    /// - bare `name` — same package as a target.
    fn relative<'v>(
        this: &LabelValue,
        #[starlark(require = pos)] s: &str,
    ) -> anyhow::Result<LabelValue> {
        // Canonicalise to `@@<repo>//<pkg>:<name>` shape so the
        // resulting `raw` matches `File::label`'s `@@…//pkg:base`
        // format. Same key shape lets vendored `expand_location`'s
        // `all_targets[ctx.label.relative(...)]` lookup hit.
        let canon = |repo: &str, pkg: &str, name: &str| -> String {
            format!("@@{}//{}:{}", repo, pkg, name)
        };
        // Same-package shortcut: `:name` → `@@<repo>//<this_pkg>:name`.
        if let Some(name) = s.strip_prefix(':') {
            return Ok(LabelValue::parse(&canon(&this.repo, &this.package, name)));
        }
        // Absolute, same repo: `//pkg:name`. Anchor on `this.repo`.
        if let Some(rest) = s.strip_prefix("//") {
            let (pkg, name) = match rest.split_once(':') {
                Some((p, n)) => (p, n),
                None => {
                    let last = rest.rsplit('/').next().unwrap_or(rest);
                    (rest, last)
                }
            };
            return Ok(LabelValue::parse(&canon(&this.repo, pkg, name)));
        }
        // Absolute different-repo: `@…` / `@@…`.
        if s.starts_with('@') {
            return Ok(LabelValue::parse(s));
        }
        // Bare name `foo` — interpreted as `:foo` in the current
        // package (Bazel's relative-label shorthand).
        Ok(LabelValue::parse(&canon(&this.repo, &this.package, s)))
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

    /// `iter(wildcard)` / `for x in wildcard:` / list comprehensions —
    /// return an empty iterator. The vendored
    /// `builtins_bzl/common/cc/cc_shared_library.bzl:24` reads
    /// `[[k] for k in _builtins.toplevel.proto_common_do_not_use.external_proto_infos()]`
    /// at module-eval time; the inner call lands on a `WildcardStruct`
    /// (Loupe doesn't model the proto-common host machinery), and
    /// without iteration support the list comprehension would
    /// hard-error. Empty iteration is the only honest fallback —
    /// we don't know what would be in the list — and it lets the
    /// module freeze with an empty downstream effect.
    fn iterate_collect(
        &self,
        _heap: starlark::values::Heap<'v>,
    ) -> starlark::Result<Vec<starlark::values::Value<'v>>> {
        Ok(Vec::new())
    }
}

// `native.*` rules Loupe needs are registered Rust-side via
// `crate::starlark_types::native_rules::native_rules_globals`
// (`alias`, `label_flag`, `label_setting`, plus accepted-and-ignored
// stubs like `package` / `licenses` / `visibility`). The engine
// registers them as bare BUILD globals; embedded `exports.bzl`'s
// `"alias": native.alias` re-export resolves through this same set.
