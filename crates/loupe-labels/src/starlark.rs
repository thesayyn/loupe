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

