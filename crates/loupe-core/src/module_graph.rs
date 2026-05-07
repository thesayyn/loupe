//! `MODULE.bazel` parsing and the workspace-wide repo mapping.
//!
//! Each `bazel_dep(name=X, version=Y)` call is recorded and contributes
//! one entry to the workspace's apparent → canonical repo mapping.
//! Loupe doesn't fetch — DESIGN.md mandates "all deps assumed vendored
//! on disk." The engine's external-repo root supplies the on-disk path
//! for each canonical repo (typically derived from `bazel info
//! output_base` or a `bazel mod vendor` output directory).
//!
//! ## Canonical naming
//!
//! Bazel 7's bzlmod uses the convention `<apparent>+` for canonical
//! repo names when a single version of a module is in the graph
//! (`rules_oci+`, `aspect_bazel_lib+`, …). Older bzlmod versions used
//! `<apparent>~<version>` and Bazel 6's WORKSPACE used the apparent
//! name directly. v1 mirrors the Bazel 7 `+` convention; if a future
//! caller needs a different scheme we add an override hook then.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use starlark::any::ProvidesStaticType;
use starlark::environment::{GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::Value;
use starlark::values::none::NoneType;

use crate::label::{CanonicalRepo, Label, PackageId, PackagePath};

#[derive(Debug, Clone, PartialEq, Eq, allocative::Allocative)]
pub struct ModuleDecl {
    pub name: String,
    pub version: String,
    pub canonical: CanonicalRepo,
}

/// One `bazel_dep(...)` declaration from `MODULE.bazel`.
#[derive(Debug, Clone, PartialEq, Eq, allocative::Allocative)]
pub struct ModuleDep {
    /// `name=` arg — what BUILD/`.bzl` files reference as `@<name>`.
    pub apparent: String,
    /// `version=` arg. Loupe doesn't enforce version selection at v1
    /// (deps are assumed vendored), but the field rides along so a
    /// future MVS pass can land without re-parsing.
    pub version: String,
    /// Canonical repo name. Matches Bazel 7's `<apparent>+` convention
    /// so on-disk paths line up with `<output_base>/external/<canonical>`
    /// (or a vendored equivalent).
    pub canonical: CanonicalRepo,
}

/// Compute the canonical repo name Bazel 7 bzlmod assigns to a
/// `bazel_dep` call. Public so callers wiring an external-repo root
/// can predict the directory name without re-parsing `MODULE.bazel`.
pub fn canonical_for_apparent(apparent: &str) -> CanonicalRepo {
    CanonicalRepo::new(format!("{apparent}+"))
}

#[derive(Debug, Clone, PartialEq, Eq, allocative::Allocative)]
pub struct ModuleGraph {
    pub main: ModuleDecl,
    /// Every `bazel_dep` declaration in declaration order.
    pub deps: Vec<ModuleDep>,
    /// apparent name → canonical repo. Contains the main module and
    /// every `bazel_dep`.
    pub repo_mapping: BTreeMap<String, CanonicalRepo>,
    /// Per-repo apparent → canonical mappings sourced from
    /// `MODULE.bazel.lock`. Each external repo carries its own view of
    /// transitive deps; resolving a load from inside repo X consults
    /// `per_repo_mapping[X]` first, then falls back to the main
    /// `repo_mapping`. See [`crate::lockfile`].
    pub per_repo_mapping: BTreeMap<CanonicalRepo, BTreeMap<String, CanonicalRepo>>,
    /// Toolchains registered across the resolved module graph, in
    /// declaration order: main module's `register_toolchains(...)`
    /// calls first, then each `bazel_dep`'s in graph order. Labels
    /// are pre-canonicalised against the originating module's
    /// per-repo mapping (from `MODULE.bazel.lock`) so the resolver
    /// can match by `Label` equality without re-walking the mapping.
    /// Transitive parsing requires `EngineBuilder::external_repo_root`
    /// to be set; without it only the main module's calls are
    /// captured. v1 doesn't expand `:all` patterns or honour
    /// `dev_dependency` filtering.
    pub registered_toolchains: Vec<Label>,
    /// Execution platforms registered across the module graph. Same
    /// shape as `registered_toolchains`. v1 captures only — platform
    /// resolution itself isn't run.
    pub registered_execution_platforms: Vec<Label>,
}

impl ModuleGraph {
    pub fn resolve_apparent(&self, apparent: &str) -> Option<&CanonicalRepo> {
        self.repo_mapping.get(apparent)
    }

    /// Effective `apparent → canonical` mapping for `pkg`'s repo.
    /// Starts from the main module's mapping, then layers
    /// `per_repo_mapping[pkg.repo]` on top — so the rule's defining
    /// repo's view (lockfile-recorded + bazel_dep-derived) wins for
    /// names main never declared. Use everywhere a label string is
    /// canonicalised in a non-main context: `toolchains=[...]`
    /// declarations on a rule loaded from an external `.bzl`,
    /// select() branch keys inside external BUILDs, etc.
    pub fn effective_repo_mapping(&self, pkg: &PackageId) -> BTreeMap<String, CanonicalRepo> {
        let mut out = self.repo_mapping.clone();
        if let Some(per) = self.per_repo_mapping.get(&pkg.repo) {
            for (k, v) in per {
                out.insert(k.clone(), v.clone());
            }
        }
        out
    }
}

#[derive(Debug, Default, ProvidesStaticType)]
struct ModuleBuilder {
    main: RefCell<Option<ModuleDecl>>,
    deps: RefCell<Vec<ModuleDep>>,
    registered_toolchains: RefCell<Vec<String>>,
    registered_execution_platforms: RefCell<Vec<String>>,
}

fn builder_from_eval<'a, 'v>(eval: &'a Evaluator<'v, '_, '_>) -> anyhow::Result<&'a ModuleBuilder> {
    eval.extra
        .ok_or_else(|| anyhow::anyhow!("internal: no ModuleBuilder on evaluator"))?
        .downcast_ref::<ModuleBuilder>()
        .ok_or_else(|| anyhow::anyhow!("internal: extra is not a ModuleBuilder"))
}

fn unpack_label_args<'v>(
    args: &starlark::eval::Arguments<'v, '_>,
    eval: &Evaluator<'v, '_, '_>,
    fn_name: &str,
) -> anyhow::Result<Vec<String>> {
    let positions = args
        .positions(eval.heap())
        .map_err(|e| anyhow::anyhow!("{fn_name}: {e}"))?;
    let mut out = Vec::new();
    for v in positions {
        let s = v.unpack_str().ok_or_else(|| {
            anyhow::anyhow!(
                "{fn_name}: positional arg must be a string label, got {}",
                v.get_type()
            )
        })?;
        out.push(s.to_string());
    }
    Ok(out)
}

#[starlark_module]
fn module_bazel_globals(b: &mut GlobalsBuilder) {
    fn module<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named, default = "")] version: &str,
        #[starlark(require = named)] compatibility_level: Option<Value<'v>>,
        #[starlark(require = named, default = "")] repo_name: &str,
        #[starlark(require = named)] bazel_compatibility: Option<Value<'v>>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        // `compatibility_level` is an int in Bazel; `bazel_compatibility`
        // is a list of version-spec strings. Both are accepted but not
        // used in v1 — the analysis engine doesn't enforce module-graph
        // compatibility.
        let _ = (compatibility_level, repo_name, bazel_compatibility);
        let builder = eval
            .extra
            .ok_or_else(|| anyhow::anyhow!("internal: no ModuleBuilder on evaluator"))?
            .downcast_ref::<ModuleBuilder>()
            .ok_or_else(|| anyhow::anyhow!("internal: extra is not a ModuleBuilder"))?;
        let mut slot = builder.main.borrow_mut();
        if slot.is_some() {
            return Err(anyhow::anyhow!("MODULE.bazel called module(...) twice"));
        }
        *slot = Some(ModuleDecl {
            name: name.to_string(),
            version: version.to_string(),
            canonical: CanonicalRepo::MAIN,
        });
        Ok(NoneType)
    }

    /// `register_toolchains(*labels)` — Bazel's bzlmod toolchain
    /// registration. v1 captures the labels in declaration order; the
    /// toolchain resolver walks this list at `ctx.toolchains[T]`
    /// lookup time. STUB: doesn't expand `:all`-style patterns; each
    /// label is treated as exact.
    fn register_toolchains<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let labels = unpack_label_args(args, eval, "register_toolchains")?;
        let builder = builder_from_eval(eval)?;
        builder.registered_toolchains.borrow_mut().extend(labels);
        Ok(NoneType)
    }

    /// `register_execution_platforms(*labels)`. v1 captures the
    /// labels; platform resolution itself isn't run.
    fn register_execution_platforms<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let labels = unpack_label_args(args, eval, "register_execution_platforms")?;
        let builder = builder_from_eval(eval)?;
        builder
            .registered_execution_platforms
            .borrow_mut()
            .extend(labels);
        Ok(NoneType)
    }

    /// `use_extension(bzl_label, extension_name, dev_dependency=False, isolate=False)`.
    /// STUB: returns a placeholder; bzlmod extension semantics aren't
    /// modelled. The lockfile already gives us per-repo mappings and
    /// generated repos, so we don't need to execute the extension.
    fn use_extension<'v>(
        #[starlark(require = pos)] _bzl_label: &str,
        #[starlark(require = pos)] _extension_name: &str,
        #[starlark(require = named, default = false)] _dev_dependency: bool,
        #[starlark(require = named, default = false)] _isolate: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        // Return a `WildcardStruct` — callable + arbitrary attribute
        // access — so subsequent `ext.tag_class(...)` calls in the
        // module file don't trip up the eval. We don't model the
        // extension's tag classes; the lockfile already records the
        // extension's outputs (generated repos + repo mappings).
        Ok(eval
            .heap()
            .alloc(crate::starlark_types::compat::WildcardStruct))
    }

    /// `use_repo(extension_proxy, *names)`. STUB.
    fn use_repo<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (args, eval);
        Ok(NoneType)
    }

    /// `single_version_override(...)`. STUB — v1 doesn't run MVS.
    fn single_version_override<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (args, eval);
        Ok(NoneType)
    }

    /// `archive_override(...)`. STUB.
    fn archive_override<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (args, eval);
        Ok(NoneType)
    }

    /// `local_path_override(module_name, path)`. STUB.
    fn local_path_override<'v>(
        #[starlark(require = named)] _module_name: Option<&str>,
        #[starlark(require = named)] _path: Option<&str>,
    ) -> anyhow::Result<NoneType> {
        Ok(NoneType)
    }

    /// `git_override(...)`. STUB.
    fn git_override<'v>(
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = (args, eval);
        Ok(NoneType)
    }

    /// `bazel_dep(name, version=…, repo_name=…, dev_dependency=…)`.
    /// Recorded into the module graph; `repo_name` (the apparent-name
    /// override) lets `MODULE.bazel` re-bind a dep under a different
    /// short name. v1 honors `repo_name` if set, falls back to `name`.
    fn bazel_dep<'v>(
        #[starlark(require = named)] name: &str,
        #[starlark(require = named, default = "")] version: &str,
        #[starlark(require = named, default = "")] repo_name: &str,
        #[starlark(require = named, default = false)] dev_dependency: bool,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let _ = dev_dependency; // accepted; v1 doesn't distinguish
        let builder = eval
            .extra
            .ok_or_else(|| anyhow::anyhow!("internal: no ModuleBuilder on evaluator"))?
            .downcast_ref::<ModuleBuilder>()
            .ok_or_else(|| anyhow::anyhow!("internal: extra is not a ModuleBuilder"))?;
        let apparent = if repo_name.is_empty() {
            name
        } else {
            repo_name
        };
        builder.deps.borrow_mut().push(ModuleDep {
            apparent: apparent.to_string(),
            version: version.to_string(),
            canonical: canonical_for_apparent(name),
        });
        Ok(NoneType)
    }
}

/// Result of evaluating one MODULE.bazel file. Doesn't canonicalise —
/// the caller decides which package + repo mapping to resolve against
/// (main vs transitive).
struct EvalResult {
    main: Option<ModuleDecl>,
    deps: Vec<ModuleDep>,
    raw_toolchains: Vec<String>,
    raw_execution_platforms: Vec<String>,
}

fn run_module_eval(src: &str, display_name: &str, lenient: bool) -> Result<EvalResult> {
    let ast = AstModule::parse(display_name, src.to_string(), &Dialect::Standard)
        .map_err(|e| anyhow::anyhow!("parsing {display_name}: {e}"))?;
    let globals = GlobalsBuilder::standard()
        .with(module_bazel_globals)
        .build();
    let builder = ModuleBuilder::default();
    // `lenient` mode: eval errors mid-file (e.g. an unmodelled
    // extension tag-class method, an unknown override) shouldn't
    // discard results from statements that already ran. The
    // builder's RefCells captured everything up to the error.
    // Used for transitive `external/<canonical>/MODULE.bazel` walks
    // where we just want to harvest `register_toolchains` calls and
    // would otherwise lose them to the first unmodelled call.
    // Strict mode (the main MODULE.bazel) propagates errors so
    // misuse like `register_toolchains(123)` still surfaces.
    let eval_outcome = Module::with_temp_heap(|module| {
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&builder);
        eval.eval_module(ast, &globals)
            .map(|_| ())
            .map_err(|e| anyhow!("evaluating {display_name}: {e}"))
    });
    if !lenient {
        eval_outcome?;
    }
    Ok(EvalResult {
        main: builder.main.into_inner(),
        deps: builder.deps.into_inner(),
        raw_toolchains: builder.registered_toolchains.into_inner(),
        raw_execution_platforms: builder.registered_execution_platforms.into_inner(),
    })
}

/// Stamp the lockfile's canonical separator onto a `bazel_dep`-derived
/// canonical name. Bazel's `bazel_dep` impl always emits `<name>+`;
/// older lockfiles (`lockFileVersion: 11`) actually use `~`, and a
/// small set of "well-known" modules (`bazel_tools`, `platforms`,
/// `local_config_platform`, `rules_cc`) keep their bare name as
/// canonical regardless of the lockfile separator — Bazel hardcodes
/// these.
fn canonicalise_with_sep(raw: &str, sep: char) -> CanonicalRepo {
    let trimmed = raw.trim_end_matches(['+', '~']);
    if matches!(
        trimmed,
        "bazel_tools" | "platforms" | "local_config_platform" | "rules_cc"
    ) {
        CanonicalRepo::new(trimmed)
    } else {
        CanonicalRepo::new(format!("{trimmed}{sep}"))
    }
}

/// Resolve each raw label string into a canonical `Label`. Strings
/// that fail to parse are dropped; v1 doesn't expand `:all` patterns.
fn canonicalise_labels(
    raw: &[String],
    current_package: &PackageId,
    repo_mapping: &BTreeMap<String, CanonicalRepo>,
) -> Vec<Label> {
    raw.iter()
        .filter_map(|s| Label::resolve(s, current_package, repo_mapping).ok())
        .collect()
}

pub fn parse(workspace_root: &Path) -> Result<ModuleGraph> {
    parse_with_lockfile_override(workspace_root, None, None)
}

/// Same as [`parse`] but lets the caller override the
/// `MODULE.bazel.lock` path and supply an `external_repo_root`. The
/// external root unlocks transitive `register_toolchains` /
/// `register_execution_platforms` parsing — without it only the main
/// module's calls are captured. Bazel 7 writes the rich lockfile to
/// `<output_base>/MODULE.bazel.lock` rather than the workspace, so
/// callers needing the per-repo records point `lockfile_path` at the
/// `output_base` copy and `external_repo_root` at
/// `<output_base>/external/`.
pub fn parse_with_lockfile_override(
    workspace_root: &Path,
    lockfile_path: Option<std::path::PathBuf>,
    external_repo_root: Option<&Path>,
) -> Result<ModuleGraph> {
    let path = workspace_root.join("MODULE.bazel");
    let src =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let main_eval = run_module_eval(&src, "MODULE.bazel", /* lenient */ false)?;
    let main = main_eval
        .main
        .ok_or_else(|| anyhow::anyhow!("MODULE.bazel did not call module(...)"))?;
    let lockfile =
        match lockfile_path {
            Some(p) => crate::lockfile::load_from(p)
                .with_context(|| "loading overridden MODULE.bazel.lock")?,
            None => crate::lockfile::load(workspace_root)
                .with_context(|| "loading MODULE.bazel.lock")?,
        };
    // Re-canonicalise bazel_dep canonicals using the lockfile's
    // separator. `module_bazel_globals` hardcodes `+` (Bazel 7's
    // convention); older lockfiles (lockFileVersion 11) use `~`.
    // Without this fix, on-disk paths and lockfile per-repo mappings
    // are keyed under `<name>~` while we'd look them up by `<name>+`.
    let sep = lockfile.canonical_separator();
    let deps: Vec<ModuleDep> = main_eval
        .deps
        .into_iter()
        .map(|d| ModuleDep {
            canonical: canonicalise_with_sep(d.canonical.as_str(), sep),
            ..d
        })
        .collect();
    let mut repo_mapping = BTreeMap::new();
    repo_mapping.insert(main.name.clone(), main.canonical.clone());
    for dep in &deps {
        repo_mapping.insert(dep.apparent.clone(), dep.canonical.clone());
    }

    let main_pkg = PackageId::new(main.canonical.clone(), PackagePath::root());
    let mut registered_toolchains =
        canonicalise_labels(&main_eval.raw_toolchains, &main_pkg, &repo_mapping);
    let mut registered_execution_platforms =
        canonicalise_labels(&main_eval.raw_execution_platforms, &main_pkg, &repo_mapping);

    // Start from lockfile-derived per-repo mappings (rich: includes
    // module-extension-generated repos and `repo_name=` overrides).
    // The external walk below augments each module's entry with its
    // own `bazel_dep(...)` declarations — needed because the lockfile
    // only records mappings reachable from the main module's
    // extension closure, leaving transitively-loaded `.bzl`s
    // (rules_oci's `@aspect_bazel_lib` references) without a mapping.
    let mut per_repo_mapping: BTreeMap<CanonicalRepo, BTreeMap<String, CanonicalRepo>> =
        lockfile.per_repo_mapping;

    if let Some(ext_root) = external_repo_root {
        // Walk every directory in the external root that has a
        // `MODULE.bazel` — that's the full transitive set of bzlmod
        // modules in the resolved graph (Bazel materialises them all).
        // Iterating `deps` alone would miss transitive deps:
        // rules_oci's MODULE.bazel registers toolchains, but so does
        // aspect_bazel_lib's, and that's only reachable through
        // rules_oci → aspect_bazel_lib.
        // Skip silently when the external root is unreadable.
        // Collect canonical names first.
        let mut canonicals: Vec<String> = Vec::new();
        let mut all_repo_names: Vec<String> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(ext_root) {
            for entry in entries.flatten() {
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if !entry.path().is_dir() {
                    continue;
                }
                all_repo_names.push(name.clone());
                if entry.path().join("MODULE.bazel").is_file() {
                    canonicals.push(name);
                }
            }
        }
        // Synthesise per-repo mappings for extension-generated repos
        // straight from their on-disk canonical names. Bazel encodes
        // each as `<owner><sep><sep><ext>(<sep><repo>)+`, where
        // `<sep>` matches the lockfile's separator. The lockfile
        // only records mappings actually USED during its creation
        // run; this directory walk fills in the rest so loads from
        // inside generated repos resolve.
        //
        // For each generated repo we register three classes of
        // entries:
        //   - the **owner** module's view → the generated repo
        //     (load `@<repo_short>` from the owner's bzl)
        //   - the generated repo's own view → its owner module
        //     (load `@<owner_apparent>` from the generated repo's
        //     bzl, e.g. for shared constants)
        //   - sibling generated repos in the same extension →
        //     each other (toolchain lists in `toolchains_repo`
        //     reference per-platform `@<platform_repo_short>`).
        let dbl = format!("{sep}{sep}");
        let mut by_extension: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for name in &all_repo_names {
            let Some((owner_short, rest)) = name.split_once(&dbl) else {
                continue;
            };
            let Some((ext_name, repo_short)) = rest.rsplit_once(sep) else {
                continue;
            };
            let owner_canonical = CanonicalRepo::new(format!("{owner_short}{sep}"));
            let full_canonical = CanonicalRepo::new(name.clone());
            per_repo_mapping
                .entry(owner_canonical.clone())
                .or_default()
                .entry(repo_short.to_string())
                .or_insert(full_canonical.clone());
            per_repo_mapping
                .entry(full_canonical.clone())
                .or_default()
                .entry(owner_short.to_string())
                .or_insert(owner_canonical);
            let group_key = format!("{owner_short}{dbl}{ext_name}");
            by_extension
                .entry(group_key)
                .or_default()
                .push((repo_short.to_string(), name.clone()));
        }
        for members in by_extension.values() {
            for (sibling_short, sibling_canonical) in members {
                let sibling_canonical = CanonicalRepo::new(sibling_canonical.clone());
                for (other_short, other_canonical) in members {
                    if other_short == sibling_short {
                        continue;
                    }
                    per_repo_mapping
                        .entry(sibling_canonical.clone())
                        .or_default()
                        .entry(other_short.clone())
                        .or_insert_with(|| CanonicalRepo::new(other_canonical.clone()));
                }
            }
        }
        canonicals.sort(); // deterministic order
        for canonical_name in canonicals {
            let canonical = CanonicalRepo::new(canonical_name.clone());
            // Skip the main repo (already handled above) — main's
            // canonical never appears in `external/` anyway, but be
            // defensive.
            if canonical.is_main() {
                continue;
            }
            let module_path = ext_root.join(&canonical_name).join("MODULE.bazel");
            let Ok(src) = std::fs::read_to_string(&module_path) else {
                continue;
            };
            let display_name = format!("external/{canonical_name}/MODULE.bazel");
            let Ok(dep_eval) = run_module_eval(&src, &display_name, /* lenient */ true) else {
                continue;
            };
            let pkg = PackageId::new(canonical.clone(), PackagePath::root());
            // Merge synthesised mapping (this module's `bazel_dep`s)
            // with the lockfile entry (which wins on conflict — it
            // carries `repo_name=` overrides + extension-generated
            // names the synthesis can't see).
            let entry = per_repo_mapping.entry(canonical.clone()).or_default();
            for d in &dep_eval.deps {
                let dep_canon = canonicalise_with_sep(d.canonical.as_str(), sep);
                entry.entry(d.apparent.clone()).or_insert(dep_canon);
            }
            let mapping = entry.clone();
            registered_toolchains.extend(canonicalise_labels(
                &dep_eval.raw_toolchains,
                &pkg,
                &mapping,
            ));
            registered_execution_platforms.extend(canonicalise_labels(
                &dep_eval.raw_execution_platforms,
                &pkg,
                &mapping,
            ));
        }
    }
    Ok(ModuleGraph {
        main,
        deps,
        repo_mapping,
        per_repo_mapping,
        registered_toolchains,
        registered_execution_platforms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_minimal_module_bazel() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"module(name = "demo", version = "1.2.3")"#,
        )
        .unwrap();
        let graph = parse(dir.path()).unwrap();
        assert_eq!(graph.main.name, "demo");
        assert_eq!(graph.main.version, "1.2.3");
        assert!(graph.main.canonical.is_main());
        assert_eq!(
            graph.resolve_apparent("demo").unwrap(),
            &CanonicalRepo::MAIN
        );
    }

    #[test]
    fn bazel_dep_populates_repo_mapping_with_canonical_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"
module(name = "demo", version = "0.0.0")
bazel_dep(name = "rules_python", version = "0.31.0")
bazel_dep(name = "rules_oci", version = "2.2.6", repo_name = "oci")
"#,
        )
        .unwrap();
        let graph = parse(dir.path()).unwrap();
        assert_eq!(graph.deps.len(), 2);
        // Default: apparent equals name.
        assert_eq!(
            graph.resolve_apparent("rules_python").unwrap(),
            &CanonicalRepo::new("rules_python+")
        );
        // `repo_name` overrides the apparent: BUILD code can reach
        // `@oci//...` even though the canonical is still `rules_oci+`.
        assert_eq!(
            graph.resolve_apparent("oci").unwrap(),
            &CanonicalRepo::new("rules_oci+")
        );
        assert!(graph.resolve_apparent("rules_oci").is_none());
    }

    #[test]
    fn register_toolchains_captures_labels_in_order() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"
module(name = "demo", version = "0.0.0")
register_toolchains(
    "//toolchains:tc_first",
    "@aspect_bazel_lib//toolchains:tar_toolchain",
    ":tc_local",
)
"#,
        )
        .unwrap();
        let graph = parse(dir.path()).unwrap();
        let names: Vec<String> = graph
            .registered_toolchains
            .iter()
            .map(Label::to_string)
            .collect();
        // `@aspect_bazel_lib` isn't in the main module's mapping (no
        // `bazel_dep`), so it falls back to the `<apparent>+` canonical
        // convention via `Label::resolve` — same caveat as elsewhere.
        assert_eq!(
            names,
            vec![
                "@@//toolchains:tc_first".to_string(),
                "@@aspect_bazel_lib+//toolchains:tar_toolchain".to_string(),
                "@@//:tc_local".to_string(),
            ],
        );
    }

    #[test]
    fn register_execution_platforms_captures_labels() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"
module(name = "demo", version = "0.0.0")
register_execution_platforms("//platforms:linux_x86_64")
"#,
        )
        .unwrap();
        let graph = parse(dir.path()).unwrap();
        let names: Vec<String> = graph
            .registered_execution_platforms
            .iter()
            .map(Label::to_string)
            .collect();
        assert_eq!(names, vec!["@@//platforms:linux_x86_64".to_string()]);
    }

    #[test]
    fn register_toolchains_rejects_non_string_arg() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("MODULE.bazel"),
            r#"
module(name = "demo", version = "0.0.0")
register_toolchains(123)
"#,
        )
        .unwrap();
        let err = parse(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("must be a string label"),
            "expected string-label error, got: {err}"
        );
    }

    #[test]
    fn transitive_module_bazel_files_contribute_register_toolchains() {
        // Layout:
        //   <ws>/MODULE.bazel — main, depends on `dep_a`
        //   <ext>/dep_a+/MODULE.bazel — registers ":local_tc"
        // The dep's `:local_tc` should canonicalise via the dep's
        // own canonical, not the main module's repo.
        let workspace = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join("MODULE.bazel"),
            r#"
module(name = "main_mod", version = "0.0.0")
bazel_dep(name = "dep_a", version = "1.0")
register_toolchains("//main_pkg:tc_main")
"#,
        )
        .unwrap();
        let dep_a_dir = ext.path().join("dep_a+");
        fs::create_dir_all(&dep_a_dir).unwrap();
        fs::write(
            dep_a_dir.join("MODULE.bazel"),
            r#"
module(name = "dep_a", version = "1.0")
register_toolchains(":local_tc", "//dep_pkg:tc_b")
"#,
        )
        .unwrap();
        // Empty workspace lockfile is fine — per_repo_mapping
        // for `dep_a+` falls back to empty, which `Label::resolve`
        // tolerates because both labels are repo-prefix-free.
        let graph = parse_with_lockfile_override(workspace.path(), None, Some(ext.path())).unwrap();
        let names: Vec<String> = graph
            .registered_toolchains
            .iter()
            .map(Label::to_string)
            .collect();
        // Main first, then dep — declaration order across the graph.
        // Each canonicalises against its own module's repo.
        assert_eq!(
            names,
            vec![
                "@@//main_pkg:tc_main".to_string(),
                "@@dep_a+//:local_tc".to_string(),
                "@@dep_a+//dep_pkg:tc_b".to_string(),
            ],
        );
    }

    #[test]
    fn walks_transitive_of_transitive_module_bazel_files() {
        // Layout:
        //   <ws>/MODULE.bazel — main, depends only on dep_a
        //   <ext>/dep_a+/MODULE.bazel — module(...) only, no deps
        //   <ext>/dep_b+/MODULE.bazel — register_toolchains(":local_tc")
        //
        // dep_b is NOT a direct bazel_dep of main; in real Bazel it
        // would be reachable through dep_a → dep_b. We don't model
        // graph traversal — we just walk every directory under the
        // external root that has a MODULE.bazel. dep_b's
        // registrations should land in `registered_toolchains`.
        let workspace = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join("MODULE.bazel"),
            r#"
module(name = "main_mod", version = "0.0.0")
bazel_dep(name = "dep_a", version = "1.0")
"#,
        )
        .unwrap();
        let dep_a = ext.path().join("dep_a+");
        std::fs::create_dir_all(&dep_a).unwrap();
        fs::write(
            dep_a.join("MODULE.bazel"),
            r#"module(name = "dep_a", version = "1.0")"#,
        )
        .unwrap();
        let dep_b = ext.path().join("dep_b+");
        std::fs::create_dir_all(&dep_b).unwrap();
        fs::write(
            dep_b.join("MODULE.bazel"),
            r#"
module(name = "dep_b", version = "1.0")
register_toolchains(":local_tc")
"#,
        )
        .unwrap();
        let graph = parse_with_lockfile_override(workspace.path(), None, Some(ext.path())).unwrap();
        let names: Vec<String> = graph
            .registered_toolchains
            .iter()
            .map(Label::to_string)
            .collect();
        assert!(
            names.iter().any(|n| n == "@@dep_b+//:local_tc"),
            "expected dep_b's toolchain registration to be picked up via the on-disk \
             walk; got {names:?}"
        );
    }

    #[test]
    fn lenient_eval_keeps_partial_register_toolchains_on_unknown_call() {
        // `aspect_bazel_lib`-style real-world MODULE.bazel files do
        // a `register_toolchains(...)` then later call something we
        // don't model (a tag-class method on a use_extension proxy,
        // an obscure override). Without lenient eval the whole
        // module is dropped; with it, the registrations recorded
        // before the error survive.
        let workspace = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join("MODULE.bazel"),
            r#"
module(name = "main_mod", version = "0.0.0")
bazel_dep(name = "dep_a", version = "1.0")
"#,
        )
        .unwrap();
        let dep_a = ext.path().join("dep_a+");
        std::fs::create_dir_all(&dep_a).unwrap();
        // First a valid registration, then an unmodelled call —
        // `register_toolchains` with a non-string positional errors
        // at *runtime*, not at compile time, so the prior call
        // survives. Real-world lenient cases (extension methods on
        // a use_extension proxy) hit this same runtime-error path.
        fs::write(
            dep_a.join("MODULE.bazel"),
            r#"
module(name = "dep_a", version = "1.0")
register_toolchains(":early_tc")
register_toolchains(123)
register_toolchains(":late_tc")
"#,
        )
        .unwrap();
        let graph = parse_with_lockfile_override(workspace.path(), None, Some(ext.path())).unwrap();
        let names: Vec<String> = graph
            .registered_toolchains
            .iter()
            .map(Label::to_string)
            .collect();
        assert!(
            names.iter().any(|n| n == "@@dep_a+//:early_tc"),
            "expected dep_a's `:early_tc` to survive lenient eval; got {names:?}"
        );
    }

    #[test]
    fn transitive_parsing_skips_deps_without_module_bazel() {
        // dep_b+ has no MODULE.bazel in the external root — should be
        // a silent skip, not an error.
        let workspace = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join("MODULE.bazel"),
            r#"
module(name = "main_mod", version = "0.0.0")
bazel_dep(name = "dep_b", version = "1.0")
"#,
        )
        .unwrap();
        // Note: no dep_b+ directory created.
        let graph = parse_with_lockfile_override(workspace.path(), None, Some(ext.path())).unwrap();
        assert!(graph.registered_toolchains.is_empty());
    }

    #[test]
    fn missing_module_call_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("MODULE.bazel"), "").unwrap();
        let err = parse(dir.path()).unwrap_err();
        assert!(err.to_string().contains("did not call module"));
    }

    #[test]
    fn well_known_modules_keep_bare_canonical() {
        // `platforms`, `bazel_tools`, `local_config_platform`,
        // `rules_cc` are special-cased on disk: Bazel doesn't suffix
        // them with the canonical separator, so neither should we
        // when re-canonicalising via the lockfile sep.
        assert_eq!(
            canonicalise_with_sep("platforms+", '~').as_str(),
            "platforms"
        );
        assert_eq!(
            canonicalise_with_sep("bazel_tools+", '~').as_str(),
            "bazel_tools"
        );
        assert_eq!(
            canonicalise_with_sep("rules_cc+", '+').as_str(),
            "rules_cc"
        );
        // Everything else gets the lockfile's separator stamped on.
        assert_eq!(
            canonicalise_with_sep("aspect_bazel_lib+", '~').as_str(),
            "aspect_bazel_lib~"
        );
        assert_eq!(
            canonicalise_with_sep("rules_oci~", '+').as_str(),
            "rules_oci+"
        );
    }
}
