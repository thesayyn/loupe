//! `.bzl` evaluator.
//!
//! Phase 3: evaluates a single `.bzl` file with the Loupe globals
//! (`provider`, `attr.*`, `rule`, `aspect`) plus the Starlark standard
//! library. `load()` statements within `.bzl` files are honored
//! through a `ReturnFileLoader` populated by the caller (PackageKey /
//! BzlLoadKey resolves them ahead of time through DICE).
//!
//! Returns a `FrozenModule` whose exported globals are whatever the
//! `.bzl` bound at top level — provider types, rule constructors, etc.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use starlark::environment::{FrozenModule, Globals, GlobalsBuilder, Module};
use starlark::eval::{Evaluator, ReturnFileLoader};
use starlark::syntax::{AstModule, Dialect};
use starlark::values::ValueLike;

use crate::starlark_types;
use crate::starlark_types::rule::FrozenRuleClass;
use loupe_labels::Label;

/// Evaluate a parsed `.bzl` AST and return the frozen module.
///
/// `defining_label` is the canonical Label of this `.bzl` itself —
/// used to stamp [`ProviderId`] on every `ProviderType` exported by
/// the frozen module so analysis can speak about provider identity
/// without holding Starlark heap pointers.
///
/// `repo_mapping` is the effective repo mapping of the
/// `.bzl`'s defining repo (`graph.effective_repo_mapping(label.package)`).
/// Stamped on every `RuleClass` so `attr.label(default = "@<apparent>...")`
/// resolves through the .bzl's perspective at rule-invoke time, not
/// the BUILD's. Held behind `Arc` so the stamp is pointer-cheap and
/// every rule defined in this `.bzl` shares the same instance.
pub fn evaluate(
    defining_label: &Label,
    repo_mapping: &std::sync::Arc<loupe_labels::RepoMapping>,
    ast: AstModule,
    loaded: &HashMap<String, FrozenModule>,
) -> Result<FrozenModule> {
    let globals = bzl_globals();
    let frozen = Module::with_temp_heap(|module| -> Result<FrozenModule> {
        // Anchor every transitively-loaded frozen heap so frozen values
        // stay valid through the freeze of *this* module's heap.
        for fm in loaded.values() {
            module.frozen_heap().add_reference(fm.frozen_heap());
        }
        // Bazel-compat surface authored in user-space Starlark
        // (see `crate::builtins`). Injected as pre-bound names so user
        // `.bzl`s see `coverage_common`, `InstrumentedFilesInfo`, etc.
        // as if they were native globals — same shape as Bazel's
        // `builtins.zip` injection.
        crate::builtins::inject_builtins_into(&module)?;
        let loader_modules: HashMap<&str, &FrozenModule> =
            loaded.iter().map(|(k, v)| (k.as_str(), v)).collect();
        let loader = ReturnFileLoader {
            modules: &loader_modules,
        };
        {
            // Carry the `Label(...)` canonicalisation context on
            // `eval.extra`: `Label("@apparent//…")` inside the .bzl
            // resolves the apparent repo against the .bzl's own
            // repo_mapping (Bazel semantics). Owner package is the
            // defining .bzl's package so `Label("//…")` / `@//`
            // self-refs anchor correctly. `eval.extra` is otherwise
            // unused during plain `.bzl` eval.
            let label_ctx = loupe_labels::starlark::LabelEvalContext {
                mapping: (**repo_mapping).clone(),
                package: loupe_labels::PackageId::new(
                    defining_label.package.repo.clone(),
                    defining_label.package.path.clone(),
                ),
            };
            let mut eval = Evaluator::new(&module);
            eval.extra = Some(&label_ctx);
            eval.set_loader(&loader);
            eval.eval_module(ast, &globals)
                .map_err(|e| anyhow!("evaluating .bzl: {e}"))?;
        }
        // Capture all names — including private ones — before freeze.
        // `Module::get` filters out underscore-prefixed names; we want
        // them so private rule bindings (`_my_rule = rule(...)` wrapped
        // by a public macro) get stamped too. Without this stamp, the
        // inner rule fires with `<unstamped_rule>` and
        // `compute_analysis` falls through to the source-file
        // early-return path (no impl runs).
        let all_names: Vec<String> = module
            .names_and_visibilities()
            .map(|(n, _)| n.as_str().to_string())
            .collect();
        let frozen_module = module
            .freeze()
            .map_err(|e| anyhow!("freezing .bzl module: {e:?}"))?;
        stamp_origin(&frozen_module, &all_names, defining_label, repo_mapping);
        Ok(frozen_module)
    })?;
    Ok(frozen)
}

fn stamp_origin(
    frozen: &FrozenModule,
    all_names: &[String],
    defining_label: &Label,
    repo_mapping: &std::sync::Arc<loupe_labels::RepoMapping>,
) {
    stamp_origin_with_names(frozen, all_names, defining_label, repo_mapping)
}

/// Public alternative to [`stamp_rule_class_origin`] that also reaches
/// private (underscore-prefixed) bindings. Callers walk
/// `Module::names_and_visibilities()` pre-freeze (FrozenModule's
/// public iterator skips private names) and pass the list here. Used
/// by both the user `.bzl` evaluator and the embedded-builtins loader
/// so `_my_rule = rule(...)` wrapped by a public macro still gets
/// stamped with its binding name.
pub(crate) fn stamp_origin_with_names(
    frozen: &FrozenModule,
    all_names: &[String],
    defining_label: &Label,
    repo_mapping: &std::sync::Arc<loupe_labels::RepoMapping>,
) {
    use crate::starlark_types::provider::{FrozenProviderType, ProviderId};
    // Bazel built-in providers the engine introspects and synthesises;
    // stamped native (`defining_bzl: None`) so they display by bare name
    // (`cquery`) and unify with engine-synthesised instances by identity.
    const NATIVE_PROVIDER_SYMBOLS: &[&str] =
        &["DefaultInfo", "OutputGroupInfo", "InstrumentedFilesInfo"];
    for name in all_names {
        let Ok((owned, _vis)) = frozen.get_any_visibility(name) else {
            continue;
        };
        let frozen_value = unsafe { owned.unchecked_frozen_value() };
        if let Some(rc) = frozen_value.downcast_ref::<FrozenRuleClass>() {
            let _ = rc.kind.set(name.clone());
            let _ = rc.defining_bzl.set(defining_label.clone());
            let _ = rc.repo_mapping.set(std::sync::Arc::clone(repo_mapping));
        }
        if let Some(asp) =
            frozen_value.downcast_ref::<crate::starlark_types::aspect::FrozenAspect>()
        {
            let _ = asp.symbol.set(name.clone());
            let _ = asp.defining_bzl.set(defining_label.clone());
            let _ = asp.repo_mapping.set(std::sync::Arc::clone(repo_mapping));
        }
        if let Some(pt) = frozen_value.downcast_ref::<FrozenProviderType>() {
            // Bazel's universal providers are *native* — `cquery`
            // surfaces them by bare name and the engine synthesises
            // `DefaultInfo` instances with `defining_bzl: None`. Stamp
            // the canonical type (defined in the embedded `_builtins`
            // tree) with that same native id so real, synthetic, and
            // `target[DefaultInfo]`-key providers unify under
            // identity-match — no symbol-string fall-through (see
            // `starlark_types::target`). User `.bzl` providers, and
            // every other builtin, keep the `(defining_bzl, symbol)`
            // id.
            let native = defining_label.repo().as_str() == "_builtins"
                && NATIVE_PROVIDER_SYMBOLS.contains(&name.as_str());
            let _ = pt.id.set(ProviderId {
                defining_bzl: (!native).then(|| defining_label.clone()),
                symbol: name.clone(),
            });
        }
    }
}

/// Parse a `.bzl` file at `path` into an AST.
pub fn parse(path: &std::path::Path) -> Result<AstModule> {
    parse_with_source(path).map(|(ast, _)| ast)
}

/// Parse a `.bzl` file at `path` into an AST plus its raw source bytes.
/// The source string is what callers digest into the marker file's
/// `predeclaredInputHash` so an edit to the rule's defining `.bzl`
/// busts every cached materialisation that used it.
pub fn parse_with_source(path: &std::path::Path) -> Result<(AstModule, String)> {
    let src =
        std::fs::read_to_string(path).map_err(|e| anyhow!("reading {}: {e}", path.display()))?;
    let filename = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    let ast = AstModule::parse(&filename, src.clone(), &bazel_dialect())
        .map_err(|e| anyhow!("parsing {}: {e}", path.display()))?;
    Ok((ast, src))
}

/// Parse `.bzl` source text into an AST. `display_origin` is the
/// label-like string surfaced in parse-error messages (e.g.
/// `@bazel_tools//tools/cpp:toolchain_utils.bzl`).
pub fn parse_source(src: String, filename: &str, display_origin: &str) -> Result<AstModule> {
    let src = monkey_patch_for_loupe(&src);
    AstModule::parse(filename, src, &bazel_dialect())
        .map_err(|e| anyhow!("parsing {display_origin}: {e}"))
}

/// Source rewrites applied at parse time to paper over starlark-rust
/// deviations from Bazel's spec that vendored `.bzl` files happen to
/// exercise. Each rule is one source-pattern compensation:
///
/// 1. **`<expr>.elems()` → `_loupe_elems_to_list(<expr>)`**.
///    starlark-rust's `str.elems()` returns a `StarlarkIter<String>`
///    that doesn't support slicing (`[:]`) or `len()`. Bazel's
///    `str.elems()` returns a real list. rules_python's
///    `parse_requirements_txt.bzl` does `content.elems()[:]`, which
///    blows up. The rewrite swaps the method call for a global
///    function (registered in `starlark_types::stub::compat_globals`)
///    that returns the materialised list.
///
/// The receiver pattern is a conservative "simple identifier chain"
/// (`<word>(\.<word>)*`) — covers `content.elems()`, `attr.x.elems()`,
/// `mod.foo.bar.elems()`. Doesn't try to handle parenthesised
/// expressions; vendored code we've seen so far doesn't need it. A
/// future failure with a complex receiver can extend this.
fn monkey_patch_for_loupe(src: &str) -> String {
    use std::sync::OnceLock;
    static ELEMS_RE: OnceLock<regex::Regex> = OnceLock::new();
    static SELECT_OR_RE: OnceLock<regex::Regex> = OnceLock::new();
    let elems = ELEMS_RE.get_or_init(|| {
        regex::Regex::new(r"\b([a-zA-Z_][a-zA-Z0-9_]*(?:\.[a-zA-Z_][a-zA-Z0-9_]*)*)\.elems\(\)")
            .unwrap()
    });
    let out = elems.replace_all(src, "_loupe_elems_to_list($1)");

    // 2. `<expr> | select(...)` → `<expr> + select(...)`. Bazel's
    //    `dict | select(...)` and `list | select(...)` build a
    //    deferred `SelectorList`. starlark-rust dispatches `|`
    //    (`bit_or`) solely to the LEFT operand (the builtin dict/list)
    //    with no right-operand fallback, so we can't intercept it from
    //    `SelectorList`. `+` (`add`) *does* fall back to the right
    //    operand's `radd`, which `SelectorList` implements. Rewriting
    //    `| select(` to `+ select(` routes the concatenation through
    //    that path. The `select(` token immediately after `|` is an
    //    unambiguous marker — bitwise-or on an actual int/set never
    //    has `select(` as its RHS in vendored Bazel code.
    let select_or = SELECT_OR_RE.get_or_init(|| regex::Regex::new(r"\|(\s*)select\(").unwrap());
    select_or.replace_all(&out, "+${1}select(").into_owned()
}

/// Bazel's `.bzl` Starlark dialect: Standard plus keyword-only and
/// positional-only argument syntax (`def f(a, *, b)`/`def f(a, /, b)`).
/// Bazel's own toolchain helpers (`bazel_tools/tools/cpp/toolchain_utils.bzl`)
/// rely on `*` as a keyword-only marker, and pure `Standard` rejects
/// that.
fn bazel_dialect() -> Dialect {
    Dialect {
        enable_keyword_only_arguments: true,
        enable_positional_only_arguments: true,
        ..Dialect::Standard
    }
}

#[starlark::starlark_module]
pub(crate) fn bazel_json_namespace(globals: &mut starlark::environment::GlobalsBuilder) {
    fn encode(#[starlark(require = pos)] x: starlark::values::Value) -> anyhow::Result<String> {
        x.to_json()
    }

    fn decode<'v>(
        #[starlark(require = pos)] x: &str,
        heap: starlark::values::Heap<'v>,
    ) -> anyhow::Result<starlark::values::Value<'v>> {
        Ok(heap.alloc(serde_json::from_str::<serde_json::Value>(x)?))
    }

    /// `json.encode_indent(value, *, prefix="", indent="\t")` — Bazel's
    /// pretty-printer over the same value space `encode` accepts. Mirrors
    /// the [Bazel reference](https://bazel.build/rules/lib/json#encode_indent):
    /// `prefix` is prepended to each new line (after the first), `indent`
    /// is the per-level chunk.
    fn encode_indent<'v>(
        #[starlark(require = pos)] x: starlark::values::Value<'v>,
        #[starlark(require = named, default = "")] prefix: &str,
        #[starlark(require = named, default = "\t")] indent: &str,
    ) -> anyhow::Result<String> {
        let compact: String = x.to_json()?;
        let json: serde_json::Value = serde_json::from_str(&compact)?;
        Ok(json_pretty_print(&json, prefix, indent))
    }

    /// `json.indent(s, *, prefix="", indent="\t")` — starlark-go's
    /// name for `encode_indent`. Vendored upstream files reach for
    /// this.
    fn indent<'v>(
        #[starlark(require = pos)] s: &str,
        #[starlark(require = named, default = "")] prefix: &str,
        #[starlark(require = named, default = "\t")] indent: &str,
    ) -> anyhow::Result<String> {
        let json: serde_json::Value = serde_json::from_str(s)?;
        Ok(json_pretty_print(&json, prefix, indent))
    }
}

/// Bazel's `json.encode_indent` pretty-print convention. Matches the
/// reference implementation in Bazel
/// (`src/main/java/net/starlark/java/lib/json/Json.java::encodeIndent`)
/// byte-for-byte:
///
/// - empty containers (`{}`, `[]`) stay on one line, no inner newline;
/// - non-empty containers open with the bracket on the same line as
///   the parent context, newline-prefix-indent for each entry,
///   newline-prefix-(parent indent) before the closing bracket;
/// - object keys are JSON-quoted; key/value separator is `": "`.
///
/// `serde_json::to_string_pretty` uses 2 spaces and emits a slightly
/// different shape (empty container on multiple lines in some edge
/// cases), so we hand-roll the writer.
fn json_pretty_print(value: &serde_json::Value, prefix: &str, indent: &str) -> String {
    let mut out = String::new();
    json_pretty_write(value, prefix, indent, 0, &mut out);
    out
}

fn json_pretty_write(
    value: &serde_json::Value,
    prefix: &str,
    indent: &str,
    level: usize,
    out: &mut String,
) {
    match value {
        serde_json::Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            let inner_level = level + 1;
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('\n');
                out.push_str(prefix);
                for _ in 0..inner_level {
                    out.push_str(indent);
                }
                json_pretty_write(
                    &serde_json::Value::String(k.clone()),
                    prefix,
                    indent,
                    0,
                    out,
                );
                out.push_str(": ");
                json_pretty_write(v, prefix, indent, inner_level, out);
            }
            out.push('\n');
            out.push_str(prefix);
            for _ in 0..level {
                out.push_str(indent);
            }
            out.push('}');
        }
        serde_json::Value::Array(arr) => {
            if arr.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            let inner_level = level + 1;
            for (i, v) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('\n');
                out.push_str(prefix);
                for _ in 0..inner_level {
                    out.push_str(indent);
                }
                json_pretty_write(v, prefix, indent, inner_level, out);
            }
            out.push('\n');
            out.push_str(prefix);
            for _ in 0..level {
                out.push_str(indent);
            }
            out.push(']');
        }
        // Scalars: compact-encoded the same way serde_json renders
        // them in `to_string`. JSON's number/string/bool/null syntax
        // is identical between Bazel and serde_json.
        _ => {
            out.push_str(&value.to_string());
        }
    }
}

fn bzl_globals() -> Globals {
    // Bazel macros pervasively call `struct(...)`, `json.encode`,
    // `print`, etc. Enable the LibraryExtensions Loupe needs.
    //
    // `json` is *not* in the LibraryExtension set: starlark-rust ships
    // a 2-method (`encode`, `decode`) namespace; Bazel adds
    // `encode_indent`, which rules_js's npm_package generator relies
    // on. We register our own `json` namespace below.
    use starlark::environment::LibraryExtension::*;
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
    .with(starlark_types::register_for_bzl)
    .with(loupe_bzlmod::register_for_bzlmod)
    .with_namespace("json", bazel_json_namespace)
    .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn evaluates_a_bzl_with_provider_and_rule_definitions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("defs.bzl");
        fs::write(
            &path,
            r#"
HelloInfo = provider(doc = "demo", fields = ["greeting"])

def _hello_impl(ctx):
    return [HelloInfo(greeting = ctx.attr.greeting)]

hello = rule(
    implementation = _hello_impl,
    attrs = {"greeting": attr.string(default = "hello")},
)
"#,
        )
        .unwrap();
        let ast = parse(&path).unwrap();
        let label = Label::new(
            loupe_labels::CanonicalRepo::main(),
            loupe_labels::PackagePath::root(),
            "test.bzl",
        );
        let frozen = evaluate(
            &label,
            &std::sync::Arc::new(loupe_labels::RepoMapping::empty()),
            ast,
            &HashMap::new(),
        )
        .unwrap();
        // The frozen module should expose `HelloInfo` and `hello`.
        let names: std::collections::BTreeSet<String> =
            frozen.names().map(|n| n.as_str().to_string()).collect();
        assert!(names.contains("HelloInfo"), "names = {names:?}");
        assert!(names.contains("hello"), "names = {names:?}");
    }

    /// `json.encode_indent` defaults to a tab indent and emits empty
    /// containers on the same line (`{}`, `[]`). Pinned against
    /// Bazel's reference behaviour — rules_js's `copy_to_directory`
    /// rule writes a `<name>_config.json` config file via
    /// `json.encode_indent(config)` and its byte-identical hash flows
    /// into the action's input merkle tree, so even a 2-space vs tab
    /// indent diverges the downstream `Action.input_root_digest`.
    #[test]
    fn json_encode_indent_uses_tab_default_and_keeps_empty_containers_inline() {
        let v: serde_json::Value = serde_json::json!({
            "exclude_srcs_packages": [],
            "files": [
                {"package": "", "path": "lib.js"},
            ],
            "replace_prefixes": {},
        });
        let out = super::json_pretty_print(&v, "", "\t");
        let expected = "{\n\t\"exclude_srcs_packages\": [],\n\t\"files\": [\n\t\t{\n\t\t\t\"package\": \"\",\n\t\t\t\"path\": \"lib.js\"\n\t\t}\n\t],\n\t\"replace_prefixes\": {}\n}";
        assert_eq!(out, expected);
    }

    /// `prefix=` is prepended to each newline (after the first); `indent=`
    /// is the per-level chunk. Pinned against Bazel's reference behaviour
    /// (`Json.java::encodeIndent`).
    #[test]
    fn json_encode_indent_honours_explicit_prefix_and_indent() {
        let v: serde_json::Value = serde_json::json!({
            "a": [1, 2],
            "b": {"c": 3},
        });
        let out = super::json_pretty_print(&v, ">>", "  ");
        let expected =
            "{\n>>  \"a\": [\n>>    1,\n>>    2\n>>  ],\n>>  \"b\": {\n>>    \"c\": 3\n>>  }\n>>}";
        assert_eq!(out, expected);
    }

    #[test]
    fn aspect_is_a_noop_accept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bzl");
        fs::write(
            &path,
            r#"
def _impl(target, ctx):
    return []

my_aspect = aspect(implementation = _impl)
"#,
        )
        .unwrap();
        let ast = parse(&path).unwrap();
        let label = Label::new(
            loupe_labels::CanonicalRepo::main(),
            loupe_labels::PackagePath::root(),
            "test.bzl",
        );
        let frozen = evaluate(
            &label,
            &std::sync::Arc::new(loupe_labels::RepoMapping::empty()),
            ast,
            &HashMap::new(),
        )
        .unwrap();
        let names: std::collections::BTreeSet<String> =
            frozen.names().map(|n| n.as_str().to_string()).collect();
        assert!(names.contains("my_aspect"), "names = {names:?}");
    }
}
