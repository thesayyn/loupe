//! Repository-rule Starlark machinery.
//!
//! - [`RepositoryRule`] / [`FrozenRepositoryRule`] — what `repository_rule(...)`
//!   returns inside a `.bzl`. Holds the impl function and the attr schema.
//! - [`RepoCtx`] — the `ctx` passed to a repo rule's impl. Methods do
//!   real on-disk and (via the captured tokio handle) network I/O.
//!   Non-lifetime: stores resolved attrs as plain Rust data and
//!   rebuilds the Starlark struct on each `ctx.attr` access.
//! - [`register_for_bzlmod`] — installs `repository_rule`, `tag_class`,
//!   `module_extension` into a `GlobalsBuilder` so loupe-core's
//!   `wrappers_globals()` can reach them.
//! - [`eval_repository_rule`] — synchronous evaluator: builds the ctx,
//!   calls `impl(ctx)`, surfaces side effects to the workdir.
//!
//! Async I/O is bridged via `tokio::runtime::Handle::block_on` because
//! Starlark eval is sync. The driver runs inside `spawn_blocking` so
//! `block_on` is legal.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use allocative::Allocative;
use anyhow::anyhow;
use starlark::any::ProvidesStaticType;
use starlark::environment::FrozenModule;
use starlark::environment::GlobalsBuilder;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::environment::Module;
use starlark::eval::Evaluator;
use starlark::starlark_complex_values;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::list::UnpackList;
use starlark::values::none::NoneType;
use starlark::values::structs::AllocStruct;
use starlark::values::{
    Freeze, FreezeResult, Freezer, FrozenValue, Heap, NoSerialize, StarlarkValue, Trace, Value,
    ValueLifetimeless, ValueLike, starlark_value,
};

use crate::archive::ArchiveKind;
use crate::archive::extract;
use crate::fetch::download;
use crate::integrity::Integrity;
use crate::materialize::MaterializeError;
use crate::recorded_input::DirentsValue;
use crate::recorded_input::FileValue;
use crate::recorded_input::RepoRecordedInput;
use crate::recorded_input::observe_dirents;
use crate::recorded_input::observe_dirtree;
use crate::recorded_input::observe_file;

/// Attr values for a repo-rule invocation. Intentionally narrower than
/// loupe-core's `AttrValue` — repo rules only need the JSON-friendly
/// scalar set. Module extensions populate these from their tag
/// invocations; today, the loupe-core test helper builds them from a
/// label kwargs dict.
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub enum RepoAttrValue {
    None,
    String(String),
    Int(i64),
    Bool(bool),
    StringList(Vec<String>),
    StringDict(BTreeMap<String, String>),
    /// `attr.string_list_dict` surface — keys to lists of strings.
    /// Bazel's `globals_repo` (in `bazel_features`) stores its
    /// `(min_version, max_version)` tuples in this shape.
    StringListDict(BTreeMap<String, Vec<String>>),
    /// `attr.label`-typed value. Stored as the raw label string (with
    /// any `@apparent//pkg:name` or `//pkg:name` form preserved) and
    /// rendered as a Starlark `Label` value when the tag struct is
    /// built. Distinct from `String` so extension impls can call
    /// `.same_package_label(...)` / `.package` / `.name` on it the
    /// way Bazel's impls do.
    Label(String),
    /// `attr.label_list`-typed value. Each element is the raw label
    /// string of one entry; rendered as a Starlark list of `Label`
    /// values so impls can `for l in tag.foo: l.package` like Bazel.
    /// Distinct from `StringList` for the same reason `Label` is
    /// distinct from `String`.
    LabelList(Vec<String>),
    /// `attr.label_keyed_string_dict`-typed value. Keys are raw label
    /// strings, values are plain strings. Rendered as a Starlark dict
    /// keyed by `Label` values so impls can iterate
    /// `for label, s in tag.foo.items(): label.workspace_name` like
    /// Bazel. Default for `attr.label_keyed_string_dict()` is an
    /// empty map.
    LabelKeyedStringDict(BTreeMap<String, String>),
}

#[derive(Debug, Trace, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[repr(C)]
#[display("<repository_rule>")]
pub struct RepositoryRuleGen<V: ValueLifetimeless> {
    /// The user-supplied `implementation = _impl` callable. Captured
    /// as `Value<'v>` during `.bzl` eval; frozen alongside the `.bzl`.
    pub implementation: V,
    /// The `attrs={...}` dict, preserved as an opaque Starlark dict.
    /// v1 doesn't validate against it — invocations pass attrs through
    /// verbatim and the impl reads `ctx.attr.<name>`.
    pub attrs: V,
    #[trace(static)]
    pub local: bool,
    #[trace(static)]
    pub configure: bool,
    /// `environ=[...]` declaration. These names enter the marker's
    /// `predeclaredInputHash` — adding or removing one busts the cache
    /// unconditionally — and are also pre-recorded as `EnvVar`
    /// observations at rule entry so the marker tracks them even
    /// without an explicit `ctx.getenv` call.
    #[trace(static)]
    pub environ: Vec<String>,
}

pub type RepositoryRule<'v> = RepositoryRuleGen<Value<'v>>;
pub type FrozenRepositoryRule = RepositoryRuleGen<FrozenValue>;

starlark_complex_values!(RepositoryRule);

impl<'v> Freeze for RepositoryRule<'v> {
    type Frozen = FrozenRepositoryRule;
    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(RepositoryRuleGen {
            implementation: self.implementation.freeze(freezer)?,
            attrs: self.attrs.freeze(freezer)?,
            local: self.local,
            configure: self.configure,
            environ: self.environ,
        })
    }
}

#[starlark_value(type = "repository_rule")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for RepositoryRuleGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    /// `http_archive(name="foo", url=...)` from inside a module_extension
    /// impl. Captures the invocation into the [`ModuleCtxCaptures`]
    /// stashed on `Evaluator::extra` rather than running the impl —
    /// the rule's `_impl(ctx)` runs later, when `MaterializedRepoKey`
    /// materialises the captured spec.
    fn invoke(
        &self,
        me: Value<'v>,
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let captures = eval
            .extra
            .ok_or_else(|| {
                starlark::Error::new_other(anyhow!(
                    "repository_rule invoked outside a module_extension impl \
                     (no ModuleCtxCaptures on evaluator)"
                ))
            })?
            .downcast_ref::<ModuleCtxCaptures>()
            .ok_or_else(|| {
                starlark::Error::new_other(anyhow!(
                    "repository_rule invoked with wrong eval.extra type"
                ))
            })?;
        let named = args
            .names_map()
            .map_err(|e| starlark::Error::new_other(anyhow!("repo rule kwargs: {e}")))?;
        let mut attrs: BTreeMap<String, RepoAttrValue> = BTreeMap::new();
        let mut name: Option<String> = None;
        for (k, v) in named.iter() {
            let key = k.as_str().to_string();
            if key == "name" {
                name = v.unpack_str().map(str::to_string);
                continue;
            }
            if let Some(attr) = repo_attr_from_value(*v) {
                attrs.insert(key, attr);
            } else {
                return Err(starlark::Error::new_other(anyhow!(
                    "repo rule arg `{}`: unsupported value type {} (v1 accepts string/int/bool/string_list/string_dict)",
                    key,
                    v.get_type()
                )));
            }
        }
        let name = name.ok_or_else(|| {
            starlark::Error::new_other(anyhow!("repo rule call missing required `name = ...`"))
        })?;
        captures.push(CapturedRepoCall {
            name,
            rule_fv: me.unpack_frozen().ok_or_else(|| {
                starlark::Error::new_other(anyhow!(
                    "repo rule must be a frozen value when invoked from module_extension"
                ))
            })?,
            attrs,
        });
        Ok(Value::new_none())
    }
}

/// Classify a Starlark sequence (list or tuple) as either a
/// `StringList` (every item is a string) or a `LabelList` (every item
/// is a `Label`). Mixed-type sequences and sequences containing other
/// kinds return `None` so the caller raises with a clear "unsupported
/// value type" instead of silently losing data.
fn classify_sequence<'v>(items: impl Iterator<Item = Value<'v>>) -> Option<RepoAttrValue> {
    enum Mode {
        Unknown,
        Strings(Vec<String>),
        Labels(Vec<String>),
    }
    let mut mode = Mode::Unknown;
    for item in items {
        if let Some(s) = item.unpack_str() {
            match mode {
                Mode::Unknown => mode = Mode::Strings(vec![s.to_string()]),
                Mode::Strings(ref mut v) => v.push(s.to_string()),
                Mode::Labels(_) => return None,
            }
        } else if let Some(lv) = item.downcast_ref::<loupe_labels::LabelValue>() {
            match mode {
                Mode::Unknown => mode = Mode::Labels(vec![lv.raw.clone()]),
                Mode::Labels(ref mut v) => v.push(lv.raw.clone()),
                Mode::Strings(_) => return None,
            }
        } else {
            return None;
        }
    }
    Some(match mode {
        Mode::Unknown => RepoAttrValue::StringList(Vec::new()),
        Mode::Strings(v) => RepoAttrValue::StringList(v),
        Mode::Labels(v) => RepoAttrValue::LabelList(v),
    })
}

fn repo_attr_from_value<'v>(v: Value<'v>) -> Option<RepoAttrValue> {
    if v.is_none() {
        return Some(RepoAttrValue::None);
    }
    if let Some(lv) = v.downcast_ref::<loupe_labels::LabelValue>() {
        return Some(RepoAttrValue::Label(lv.raw.clone()));
    }
    if let Some(s) = v.unpack_str() {
        return Some(RepoAttrValue::String(s.to_string()));
    }
    if let Some(b) = v.unpack_bool() {
        return Some(RepoAttrValue::Bool(b));
    }
    if let Some(i) = v.unpack_i32() {
        return Some(RepoAttrValue::Int(i as i64));
    }
    if let Some(list) = starlark::values::list::ListRef::from_value(v) {
        return classify_sequence(list.iter());
    }
    if let Some(tuple) = starlark::values::tuple::TupleRef::from_value(v) {
        return classify_sequence(tuple.iter());
    }
    if let Some(dict) = starlark::values::dict::DictRef::from_value(v) {
        // Try string→string first (covers `string_dict` cleanly);
        // fall back to string→list-of-strings (`string_list_dict`,
        // also accepting tuples since bazel_features stores
        // `(min_version, max_version)` that way).
        let mut as_strings: BTreeMap<String, String> = BTreeMap::new();
        let mut as_lists: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut all_strings = true;
        for (k, val) in dict.iter() {
            let key = k.unpack_str()?.to_string();
            if all_strings && let Some(s) = val.unpack_str() {
                as_strings.insert(key.clone(), s.to_string());
                as_lists.insert(key, vec![s.to_string()]);
                continue;
            }
            all_strings = false;
            if let Some(list) = starlark::values::list::ListRef::from_value(val) {
                let strs: Option<Vec<String>> = list
                    .iter()
                    .map(|i| i.unpack_str().map(str::to_string))
                    .collect();
                as_lists.insert(key, strs?);
                continue;
            }
            if let Some(tuple) = starlark::values::tuple::TupleRef::from_value(val) {
                let strs: Option<Vec<String>> = tuple
                    .iter()
                    .map(|i| i.unpack_str().map(str::to_string))
                    .collect();
                as_lists.insert(key, strs?);
                continue;
            }
            return None;
        }
        if all_strings {
            return Some(RepoAttrValue::StringDict(as_strings));
        }
        return Some(RepoAttrValue::StringListDict(as_lists));
    }
    if let Some(dict) = starlark::values::dict::DictRef::from_value(v) {
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        for (k, val) in dict.iter() {
            out.insert(k.unpack_str()?.to_string(), val.unpack_str()?.to_string());
        }
        return Some(RepoAttrValue::StringDict(out));
    }
    None
}

/// One repo declared by a module_extension impl (i.e. one
/// `http_archive(name="foo", ...)` call inside the extension). Captured
/// at impl-invocation time; converted to [`RepoSpec`] after eval.
#[derive(Debug, Clone)]
pub struct CapturedRepoCall {
    pub name: String,
    pub rule_fv: FrozenValue,
    pub attrs: BTreeMap<String, RepoAttrValue>,
}

/// Shared capture sink stashed on `Evaluator::extra` while a
/// module_extension impl runs. `RepositoryRule::invoke` pushes repo
/// calls; `module_ctx.register_toolchains` pushes toolchain labels.
/// The driver drains both after eval completes.
#[derive(Default, Allocative, ProvidesStaticType)]
pub struct ModuleCtxCaptures {
    #[allocative(skip)]
    inner: std::cell::RefCell<Vec<CapturedRepoCall>>,
    #[allocative(skip)]
    toolchains: std::cell::RefCell<Vec<String>>,
}

impl ModuleCtxCaptures {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&self, call: CapturedRepoCall) {
        self.inner.borrow_mut().push(call);
    }
    pub fn take(&self) -> Vec<CapturedRepoCall> {
        std::mem::take(&mut *self.inner.borrow_mut())
    }
    pub fn push_toolchain(&self, label: String) {
        self.toolchains.borrow_mut().push(label);
    }
    pub fn take_toolchains(&self) -> Vec<String> {
        std::mem::take(&mut *self.toolchains.borrow_mut())
    }
}

#[derive(Debug, Trace, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("<module_extension>")]
#[repr(C)]
pub struct ModuleExtensionGen<V: ValueLifetimeless> {
    pub implementation: V,
    pub tag_classes: V,
    #[trace(static)]
    pub os_dependent: bool,
    #[trace(static)]
    pub arch_dependent: bool,
}

pub type ModuleExtension<'v> = ModuleExtensionGen<Value<'v>>;
pub type FrozenModuleExtension = ModuleExtensionGen<FrozenValue>;

starlark_complex_values!(ModuleExtension);

impl<'v> Freeze for ModuleExtension<'v> {
    type Frozen = FrozenModuleExtension;
    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(ModuleExtensionGen {
            implementation: self.implementation.freeze(freezer)?,
            tag_classes: self.tag_classes.freeze(freezer)?,
            os_dependent: self.os_dependent,
            arch_dependent: self.arch_dependent,
        })
    }
}

#[starlark_value(type = "module_extension")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ModuleExtensionGen<V> where
    Self: ProvidesStaticType<'v>
{
}

#[derive(Debug, Trace, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("<tag_class>")]
#[repr(C)]
pub struct TagClassGen<V: ValueLifetimeless> {
    pub attrs: V,
}

pub type TagClass<'v> = TagClassGen<Value<'v>>;
pub type FrozenTagClass = TagClassGen<FrozenValue>;

starlark_complex_values!(TagClass);

impl<'v> Freeze for TagClass<'v> {
    type Frozen = FrozenTagClass;
    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(TagClassGen {
            attrs: self.attrs.freeze(freezer)?,
        })
    }
}

#[starlark_value(type = "tag_class")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for TagClassGen<V> where Self: ProvidesStaticType<'v> {}

/// State shared between `RepoCtx` and `RepoPath` for one repo-rule
/// invocation. Cloned by `Arc` so `ctx.path(...)` can mint a `RepoPath`
/// pointing at the same recorder + env without copying the env map.
#[derive(Clone)]
pub struct RepoCtxState {
    /// The repo this rule is materialising into. Used as the
    /// `source_repo` in any `RepoMapping` records the resolver emits.
    pub canonical: String,
    pub workdir: PathBuf,
    /// Optional workspace root — surfaced through
    /// `repository_ctx.workspace_root`. Rules that probe the user's
    /// source tree (telemetry, devcontainer) read this; rules that
    /// don't touch it shouldn't notice when `None`.
    pub workspace_root: Option<PathBuf>,
    pub client: reqwest::Client,
    pub handle: tokio::runtime::Handle,
    /// Effective environment: host snapshot with `--repo_env` overrides
    /// layered on. The rule sees this map via `ctx.getenv` and (its
    /// `PATH` entry) via `ctx.which`.
    pub declared_env: Arc<BTreeMap<String, String>>,
    /// `apparent → canonical` for this rule's defining `.bzl`.
    /// Consulted when a label-shaped argument is passed to a ctx
    /// method. The mapping's `owner` is the defining `.bzl`'s repo,
    /// so `Label("@//pkg:x")` self-refs land on that canonical.
    pub repo_mapping: Arc<loupe_labels::RepoMapping>,
    /// Sink for `RepoRecordedInput`s. Pushed to by observation methods
    /// (`ctx.getenv`, `ctx.read`, `ctx.watch`, `ctx.watch_tree`, label
    /// resolution). Drained by `eval_repository_rule` and returned to
    /// `materialize_starlark_rule`, which writes the marker.
    pub recorder: Arc<Mutex<Vec<RepoRecordedInput>>>,
    /// Bridge to DICE for `ctx.path(Label(...))` and friends. Either
    /// a [`ChannelLabelResolver`] (production) or an
    /// [`UnresolvedLabelResolver`] (tests / pre-channel callers).
    pub label_resolver: Arc<dyn crate::label_resolver::LabelResolver>,
}

impl std::fmt::Debug for RepoCtxState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoCtxState")
            .field("canonical", &self.canonical)
            .field("workdir", &self.workdir)
            .finish_non_exhaustive()
    }
}

impl RepoCtxState {
    pub fn record(&self, r: RepoRecordedInput) {
        if let Ok(mut g) = self.recorder.lock() {
            g.push(r);
        }
    }

    /// Resolve a Bazel-style path argument against the workdir. Absolute
    /// paths are kept as-is; relative paths are anchored at the workdir.
    pub fn resolve(&self, raw: &str) -> PathBuf {
        // Bazel: ctx.path(".") and ctx.path("") both return the
        // workdir. Without this collapse, str(ctx.path(".")) renders
        // as "<workdir>/." which breaks vendored sanity checks like
        // http.bzl::_http_file_impl's
        // `str(download_path).startswith(str(repo_root))`.
        if raw == "." || raw.is_empty() {
            return self.workdir.clone();
        }
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else {
            self.workdir.join(p)
        }
    }

    /// Resolve a label-shaped argument via the channel actor. Records
    /// a `RepoMapping` observation on success so the marker captures
    /// the apparent → canonical mapping the rule traversed.
    pub fn resolve_label(
        &self,
        apparent: &str,
        package: &str,
        name: &str,
    ) -> anyhow::Result<PathBuf> {
        let resolved = self
            .label_resolver
            .resolve(apparent, package, name, &self.repo_mapping)
            .map_err(|e| anyhow!(e))?;
        self.record(RepoRecordedInput::RepoMapping {
            source_repo: self.canonical.clone(),
            apparent: resolved.apparent,
            canonical: resolved.canonical,
        });
        Ok(resolved.path)
    }
}

#[derive(Debug, Clone, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("<repository_ctx name={}>", name)]
pub struct RepoCtx {
    pub name: String,
    pub attrs: BTreeMap<String, RepoAttrValue>,
    #[allocative(skip)]
    pub state: Arc<RepoCtxState>,
}

starlark_simple_value!(RepoCtx);

#[starlark_value(type = "repository_ctx")]
impl<'v> StarlarkValue<'v> for RepoCtx {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "name" => Some(heap.alloc(self.name.as_str())),
            "attr" => {
                // Bazel exposes the implicit `name` attr through
                // `ctx.attr.name` alongside the user-declared attrs.
                // Vendored repo_rules (rules_nodejs's
                // `nodejs_toolchains_repo.bzl`) read it; mirror.
                let mut with_name = self.attrs.clone();
                with_name
                    .entry("name".to_string())
                    .or_insert_with(|| RepoAttrValue::String(self.name.clone()));
                Some(build_attr_struct(heap, &with_name))
            }
            "workspace_root" => {
                // Bazel: `repository_ctx.workspace_root` is a `path`
                // pointing at the user's main-module source tree.
                // Vendored repo_rules that probe the user's checkout
                // (telemetry, devcontainer, …) read it. Returns a
                // `RepoPath` so `.path(...).exists` / `str(path)` /
                // path arithmetic work as in Bazel. Falls through to
                // `None` (so `getattr` raises with the proper "no
                // attribute" message) when the engine wasn't given
                // a workspace root.
                let root = self.state.workspace_root.clone()?;
                Some(heap.alloc(RepoPath {
                    path: root,
                    state: Arc::clone(&self.state),
                }))
            }
            "os" => Some(build_os_struct(heap, &self.state.declared_env)),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        ["name", "attr", "os", "workspace_root"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(repo_ctx_methods)
    }
}

/// Map Rust's `std::env::consts::OS` to the strings vendored rules see
/// from Bazel's `repository_ctx.os.name` (and `module_ctx.os.name`).
/// Bazel passes Java's `System.getProperty("os.name")` verbatim — that's
/// `"Mac OS X"`, `"Linux"`, `"Windows 10"`, `"FreeBSD"`, etc., which
/// vendored repo_utils.bzl logic probes with `.startswith("mac os")`,
/// `.startswith("linux")`, `.find("windows")`. Rust's raw `"macos"` /
/// `"windows"` slip through every check and hit `fail("unrecognized os")`,
/// so we translate. Match Bazel's case (rules typically lowercase before
/// matching anyway).
fn bazel_os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "mac os x",
        "windows" => "windows 10",
        other => other,
    }
}

/// `repository_ctx.os.arch` — Bazel returns `System.getProperty("os.arch")`.
/// Rust's `std::env::consts::ARCH` is close enough for the architectures
/// rulesets actually probe (`x86_64`, `aarch64`, `arm`, …).
fn bazel_os_arch() -> &'static str {
    std::env::consts::ARCH
}

/// Build the `.os` Starlark struct shared by `repository_ctx.os` and
/// `module_ctx.os`. Bazel exposes `{name, arch, environ}` on both —
/// `name`/`arch` come from the host, `environ` is a dict snapshot of
/// the engine's `declared_env` (host snapshot layered with `--repo_env`
/// overrides). Vendored repo_rules + module_extensions probe
/// `os.environ.get("RULES_PYTHON_BZLMOD_DEBUG")` and similar; both
/// surfaces must expose the same shape.
fn build_os_struct<'v>(heap: Heap<'v>, declared_env: &BTreeMap<String, String>) -> Value<'v> {
    let env_pairs: Vec<(Value<'v>, Value<'v>)> = declared_env
        .iter()
        .map(|(k, v)| {
            let k_val: Value<'v> = heap.alloc(k.as_str());
            let v_val: Value<'v> = heap.alloc(v.as_str());
            (k_val, v_val)
        })
        .collect();
    let environ_value: Value<'v> = heap.alloc(starlark::values::dict::AllocDict(env_pairs));
    let pairs: Vec<(&'static str, Value<'v>)> = vec![
        ("name", heap.alloc(bazel_os_name())),
        ("arch", heap.alloc(bazel_os_arch())),
        ("environ", environ_value),
    ];
    heap.alloc(AllocStruct(pairs))
}

/// Resolve a `file:` URL to a local path, or `None` for other schemes.
///
/// Bazel's `repository_ctx.download` accepts `file:` URLs — pnpm `file:`
/// dependencies point at vendored tarballs this way. reqwest only
/// speaks http(s), so these are read straight off disk. A relative
/// `file:` path (e.g. `file:npm/private/test/vendored/x.tgz`) resolves
/// against the workspace root, matching pnpm's convention; an absolute
/// one (`file:///abs` or `file:/abs`) is taken verbatim.
fn file_url_to_path(url: &str, workspace_root: Option<&Path>, workdir: &Path) -> Option<PathBuf> {
    let rest = if let Some(after) = url.strip_prefix("file://") {
        // `file://host/path` or `file:///path`; drop the (usually
        // empty) authority up to the first `/`, keeping the leading
        // slash so the path stays absolute.
        match after.find('/') {
            Some(i) => &after[i..],
            None => after,
        }
    } else {
        url.strip_prefix("file:")?
    };
    let path = Path::new(rest);
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        Some(workspace_root.unwrap_or(workdir).join(rest))
    }
}

/// Shared body of `repository_ctx.download` / `module_ctx.download`.
/// Both ctxs operate on a workdir + an HTTP client + a runtime handle
/// to bridge sync starlark into the async `download(...)` fetcher.
/// Returns a Starlark struct mirroring Bazel's
/// `{success, sha256, integrity}` shape.
#[allow(clippy::too_many_arguments)]
fn do_download<'v>(
    urls: &[String],
    output: PathBuf,
    integrity: Option<&Integrity>,
    executable: bool,
    allow_fail: bool,
    workdir: &Path,
    workspace_root: Option<&Path>,
    client: &reqwest::Client,
    handle: &tokio::runtime::Handle,
    heap: Heap<'v>,
) -> anyhow::Result<Value<'v>> {
    if urls.is_empty() {
        return Err(anyhow!("ctx.download: url= must be non-empty"));
    }
    let bytes_result = {
        // Try `file:` URLs off disk first; collect the rest for the
        // async http path. URLs are tried in order, first success wins.
        let mut last_err: Option<String> = None;
        let mut http_urls: Vec<String> = Vec::new();
        let mut file_hit: Option<Result<bytes::Bytes, crate::fetch::FetchError>> = None;
        for url in urls {
            if let Some(path) = file_url_to_path(url, workspace_root, workdir) {
                match std::fs::read(&path) {
                    Ok(b) => {
                        let bytes = bytes::Bytes::from(b);
                        file_hit = Some(match integrity {
                            Some(expected) => {
                                expected.verify(&bytes).map(|_| bytes).map_err(Into::into)
                            }
                            None => Ok(bytes),
                        });
                        break;
                    }
                    Err(e) => last_err = Some(format!("{url}: {} ({e})", path.display())),
                }
            } else {
                http_urls.push(url.clone());
            }
        }
        match file_hit {
            Some(r) => r,
            None if !http_urls.is_empty() => {
                handle.block_on(download(client, &http_urls, integrity))
            }
            None => Err(crate::fetch::FetchError::AllFailed {
                tried: urls.len(),
                last: last_err.unwrap_or_else(|| "(none)".to_string()),
            }),
        }
    };
    let bytes = match bytes_result {
        Ok(b) => b,
        Err(_) if allow_fail => {
            // `allow_fail=True` tells the caller to inspect `success`
            // rather than raise. Bazel returns the same struct shape
            // with `success=False`; vendored rules_python's
            // simpleapi_download checks `download.success` after
            // calling.
            return Ok(heap.alloc(DownloadResult {
                success: false,
                sha256: String::new(),
                integrity: String::new(),
            }));
        }
        Err(e) => {
            // Loupe is a read-only analyzer: a tool binary fetched by a repo
            // rule is never executed and its bytes never enter the analysis
            // graph — only the repo's generated structure (BUILD, toolchain
            // targets) matters. A failed fetch (e.g. egress-restricted CI that
            // reaches git/BCR but not release CDNs) must not kill repo
            // materialization, which cascades to "no resolved toolchain"
            // (rules_multitool shfmt, …). Write an empty placeholder at the
            // output and report success so the repo rule proceeds; warn so the
            // skipped fetch stays visible. `download_and_extract` is separate.
            tracing::warn!(
                error = %e, output = %output.display(),
                "ctx.download failed; writing placeholder (analysis never executes downloaded tools)"
            );
            bytes::Bytes::new()
        }
    };
    let target = if output.as_os_str().is_empty() {
        workdir.to_path_buf()
    } else if output.is_absolute() {
        output
    } else {
        workdir.join(output)
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, &bytes)?;
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&target)?.permissions();
        perm.set_mode(perm.mode() | 0o111);
        std::fs::set_permissions(&target, perm)?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(heap.alloc(DownloadResult {
        success: true,
        sha256: sha256_hex_of(&bytes),
        integrity: integrity.map(Integrity::to_sri_string).unwrap_or_default(),
    }))
}

/// Result of `ctx.download(...)`. Mirrors Bazel's struct shape
/// (`success`, `sha256`, `integrity`) and additionally exposes a
/// `.wait()` method so the same value can be used both as the
/// synchronous result (when `block=True`, the default) and as a
/// pending token (when `block=False`). Loupe always downloads
/// synchronously regardless of `block`; `.wait()` returns `self` so
/// vendored code that branches on `block` works either way.
#[derive(
    Debug, Clone, allocative::Allocative, NoSerialize, ProvidesStaticType, derive_more::Display,
)]
#[display("<download_result success={}>", success)]
pub struct DownloadResult {
    pub success: bool,
    pub sha256: String,
    pub integrity: String,
}

starlark::starlark_simple_value!(DownloadResult);

#[starlark::values::starlark_value(type = "download_result")]
impl<'v> starlark::values::StarlarkValue<'v> for DownloadResult {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "success" => Some(Value::new_bool(self.success)),
            "sha256" => Some(heap.alloc(self.sha256.as_str())),
            "integrity" => Some(heap.alloc(self.integrity.as_str())),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        ["success", "sha256", "integrity", "wait", "cancel"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn get_methods() -> Option<&'static starlark::environment::Methods> {
        static RES: starlark::environment::MethodsStatic =
            starlark::environment::MethodsStatic::new();
        RES.methods(download_result_methods)
    }
}

#[starlark::starlark_module]
fn download_result_methods(b: &mut starlark::environment::MethodsBuilder) {
    /// `download_result.wait()` — Bazel returns a pending download
    /// token when `block=False`; the token's `.wait()` blocks and
    /// returns the actual `{success, sha256, integrity}` struct.
    /// Loupe downloads synchronously regardless, so `.wait()` just
    /// returns `self`.
    fn wait<'v>(#[starlark(this)] this: Value<'v>) -> anyhow::Result<Value<'v>> {
        Ok(this)
    }

    /// `download_result.cancel()` — Bazel-side cancel for the
    /// non-blocking variant. Loupe runs synchronously, so by the
    /// time the caller could invoke this the work is already done.
    /// No-op.
    fn cancel<'v>(
        #[starlark(this)] _this: Value<'v>,
    ) -> anyhow::Result<starlark::values::none::NoneType> {
        Ok(starlark::values::none::NoneType)
    }
}

/// Shared body of `repository_ctx.file` / `module_ctx.file`. Writes
/// `content` to a file under `workdir`; sets `0o755` permissions on
/// Unix when `executable` is true.
fn do_file(workdir: &Path, path: &Path, content: &str, executable: bool) -> anyhow::Result<()> {
    let target = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workdir.join(path)
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, content)?;
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[starlark_module]
fn repo_ctx_methods(b: &mut MethodsBuilder) {
    /// `ctx.file(path, content="", executable=False)` — write a
    /// literal file under the workdir.
    fn file<'v>(
        #[starlark(this)] this: &RepoCtx,
        // Bazel accepts `path` as a string OR a `path` value (from
        // `ctx.path(...)`) OR a `Label`. rules_python's
        // `prebuilt_toolchains_repository.bzl` passes the `path` it got
        // from `ctx.path("toolchains/BUILD.bazel")`. Resolve via
        // `unpack_path_arg` so all three forms work.
        #[starlark(require = pos)] path: Value<'v>,
        #[starlark(default = "")] content: &str,
        #[starlark(require = named, default = true)] executable: bool,
        #[starlark(require = named, default = false)] _legacy_utf8: bool,
    ) -> anyhow::Result<NoneType> {
        let target = unpack_path_arg(path, &this.state)?;
        do_file(&this.state.workdir, &target, content, executable)?;
        Ok(NoneType)
    }

    /// `ctx.report_progress(message)` — non-fatal status. Surfaces as
    /// a tracing event.
    fn report_progress(
        #[starlark(this)] this: &RepoCtx,
        #[starlark(require = pos)] status: &str,
    ) -> anyhow::Result<NoneType> {
        tracing::debug!(target = %this.name, status = %status, "repo rule progress");
        Ok(NoneType)
    }

    /// `ctx.download_and_extract(url, output, sha256, type, strip_prefix, ...)`.
    /// Bazel's spec accepts the first five positionally; the vendored
    /// `@bazel_tools//tools/build_defs/repo:http.bzl` calls download_and_extract
    /// that way and additionally passes `auth=` / `canonical_id=` named.
    fn download_and_extract<'v>(
        #[starlark(this)] this: &RepoCtx,
        url: Value<'v>,
        #[starlark(default = "")] output: &str,
        #[starlark(default = "")] sha256: &str,
        #[starlark(default = "")] r#type: &str,
        #[starlark(default = "")] strip_prefix: &str,
        #[starlark(require = named, default = "")] integrity: &str,
        // Bazel's public API uses camelCase here; vendored rules
        // (`aspect_bazel_lib`'s `coreutils_toolchain.bzl`) call it
        // with `stripPrefix=`. Accept the alias.
        #[starlark(require = named, default = "")]
        #[allow(non_snake_case)]
        stripPrefix: &str,
        // Bazel-compat: `auth={url: {...credentials...}}` for
        // private downloads. We don't model authentication for
        // analysis-only fetches — accept-and-ignore.
        #[starlark(require = named)] auth: Option<Value<'v>>,
        // Bazel-compat: extra arbitrary opaque ctx kwargs.
        #[starlark(require = named, default = "")] canonical_id: &str,
        // Bazel-compat: `headers={url: {...}}` to attach request headers
        // (rules_multitool's `_env_specific_tools_impl`). Analysis-only
        // fetches don't send custom headers — accept-and-ignore.
        #[starlark(require = named)] headers: Option<Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (sha256, auth, canonical_id, headers);
        let strip_prefix = if !stripPrefix.is_empty() {
            stripPrefix
        } else {
            strip_prefix
        };
        let urls = unpack_url_arg(url)?;
        if urls.is_empty() {
            return Err(anyhow!("ctx.download_and_extract: url= must be non-empty"));
        }
        let parsed_integrity = if integrity.is_empty() {
            None
        } else {
            Some(Integrity::parse(integrity)?)
        };
        let kind = if !r#type.is_empty() {
            archive_kind_from_explicit(r#type)
                .ok_or_else(|| anyhow!("ctx.download_and_extract: unknown type `{}`", r#type))?
        } else {
            urls.iter()
                .find_map(|u| ArchiveKind::from_url(u))
                .ok_or_else(|| {
                    anyhow!(
                        "ctx.download_and_extract: cannot infer archive type from urls; \
                         pass type=\"tar.gz\" etc."
                    )
                })?
        };
        let bytes = this
            .state
            .handle
            .block_on(download(
                &this.state.client,
                &urls,
                parsed_integrity.as_ref(),
            ))
            .map_err(|e| anyhow!("download: {e}"))?;
        let target = if output.is_empty() {
            this.state.workdir.clone()
        } else {
            this.state.workdir.join(output)
        };
        let strip = if strip_prefix.is_empty() {
            None
        } else {
            Some(strip_prefix.to_string())
        };
        extract(kind, &bytes, &target, strip.as_deref()).map_err(|e| anyhow!("extract: {e}"))?;
        // Bazel's ctx.download_and_extract result has {success, sha256,
        // integrity}. sha256 is the hex SHA-256 of the downloaded
        // content; rules_buf, rules_python, and others chain
        // `.sha256` to discover the actual checksum.
        let sha256_hex = sha256_hex_of(&bytes);
        let pairs: Vec<(&'static str, Value<'v>)> = vec![
            ("success", heap.alloc(true)),
            ("sha256", heap.alloc(sha256_hex)),
            (
                "integrity",
                heap.alloc(
                    parsed_integrity
                        .map(|i| i.to_sri_string())
                        .unwrap_or_default(),
                ),
            ),
        ];
        Ok(heap.alloc(AllocStruct(pairs)))
    }

    /// `ctx.download(url|urls, output, sha256=, executable=, integrity=)`
    /// — downloads a single file (no extraction). Bazel-compat surface
    /// for vendored toolchains that download a single binary (jq, yq,
    /// coreutils on Windows) directly via `rctx.download`.
    #[allow(clippy::too_many_arguments)]
    fn download<'v>(
        #[starlark(this)] this: &RepoCtx,
        url: Value<'v>,
        // Bazel accepts `output=` as a string, a `path` (from
        // `ctx.path(...)`), or a `Label`. Take the raw Value and let
        // `unpack_path_arg` resolve it against the rule's workdir +
        // label resolver.
        #[starlark(default = starlark::values::none::NoneOr::None)]
        output: starlark::values::none::NoneOr<Value<'v>>,
        #[starlark(default = "")] sha256: &str,
        #[starlark(default = false)] executable: bool,
        #[starlark(require = named, default = "")] integrity: &str,
        #[starlark(require = named, default = false)] allow_fail: bool,
        #[starlark(require = named, default = "")] canonical_id: &str,
        // Bazel-compat: `auth={url: {...credentials...}}` for private
        // downloads. Accept-and-ignore.
        #[starlark(require = named)] auth: Option<Value<'v>>,
        // Bazel-compat: `headers={url: {...}}` request headers
        // (rules_multitool's `_env_specific_tools_impl`). Not modelled
        // for analysis-only fetches — accept-and-ignore.
        #[starlark(require = named)] headers: Option<Value<'v>>,
        // Bazel `block=True` runs sync (default); `block=False`
        // returns a pending token. Loupe always runs sync, but
        // accepts the flag so vendored code that passes it doesn't
        // hit "extra named parameter" — the returned `DownloadResult`
        // exposes a `.wait()` that's a no-op identity.
        #[starlark(require = named, default = true)] block: bool,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (sha256, canonical_id, auth, headers, block);
        let urls = unpack_url_arg(url)?;
        let parsed_integrity = if integrity.is_empty() {
            None
        } else {
            Some(Integrity::parse(integrity)?)
        };
        let output_path = match output {
            starlark::values::none::NoneOr::Other(v) => unpack_path_arg(v, &this.state)?,
            starlark::values::none::NoneOr::None => PathBuf::new(),
        };
        do_download(
            &urls,
            output_path,
            parsed_integrity.as_ref(),
            executable,
            allow_fail,
            &this.state.workdir,
            this.state.workspace_root.as_deref(),
            &this.state.client,
            &this.state.handle,
            heap,
        )
    }

    /// `ctx.execute(arguments, timeout=600, environment={}, quiet=True,
    /// working_directory="")` — runs a subprocess with the declared
    /// environment merged with per-call overrides, captures
    /// stdout/stderr, and returns a struct with `return_code`, `stdout`,
    /// `stderr`. Records nothing implicitly — the rule must call
    /// `ctx.getenv` / `ctx.read` / `ctx.watch` to make any observation
    /// the subprocess relies on cache-relevant.
    fn execute<'v>(
        #[starlark(this)] this: &RepoCtx,
        #[starlark(require = pos)] arguments: Value<'v>,
        #[starlark(require = named, default = 600i32)] timeout: i32,
        #[starlark(require = named)] environment: Option<Value<'v>>,
        #[starlark(require = named, default = true)] quiet: bool,
        #[starlark(require = named, default = "")] working_directory: &str,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = quiet; // captured into the struct stdout/stderr regardless.
        let args = unpack_string_list_arg(arguments, "ctx.execute: arguments", &this.state)?;
        if args.is_empty() {
            return Err(anyhow!("ctx.execute: arguments must be non-empty"));
        }
        let mut env_overrides: BTreeMap<String, Option<String>> = BTreeMap::new();
        if let Some(env_val) = environment {
            if let Some(dict) = starlark::values::dict::DictRef::from_value(env_val) {
                for (k, v) in dict.iter() {
                    let key = k
                        .unpack_str()
                        .ok_or_else(|| anyhow!("ctx.execute: environment keys must be strings"))?;
                    if v.is_none() {
                        env_overrides.insert(key.to_string(), None);
                    } else if let Some(val) = v.unpack_str() {
                        env_overrides.insert(key.to_string(), Some(val.to_string()));
                    } else {
                        return Err(anyhow!(
                            "ctx.execute: environment values must be strings or None"
                        ));
                    }
                }
            } else {
                return Err(anyhow!(
                    "ctx.execute: environment must be a dict, got {}",
                    env_val.get_type()
                ));
            }
        }
        let mut effective: BTreeMap<String, String> = (*this.state.declared_env).clone();
        for (k, v) in env_overrides {
            match v {
                Some(val) => {
                    effective.insert(k, val);
                }
                None => {
                    effective.remove(&k);
                }
            }
        }
        let cwd = if working_directory.is_empty() {
            this.state.workdir.clone()
        } else {
            let raw = std::path::PathBuf::from(working_directory);
            if raw.is_absolute() {
                raw
            } else {
                this.state.workdir.join(raw)
            }
        };
        let result = run_subprocess(&args, &effective, &cwd, timeout)?;
        let pairs: Vec<(&'static str, Value<'v>)> = vec![
            ("return_code", heap.alloc(result.return_code)),
            ("stdout", heap.alloc(result.stdout)),
            ("stderr", heap.alloc(result.stderr)),
        ];
        Ok(heap.alloc(AllocStruct(pairs)))
    }

    /// `ctx.which(program)` — split the declared PATH and return the
    /// first executable named `program` (with PATHEXT-aware lookup on
    /// Windows). Returns `None` if not found. Bazel-faithful: splits
    /// the *declared* env's PATH, not the merged execute-time env, so
    /// the answer is stable w.r.t. per-call environment= arguments.
    fn which<'v>(
        #[starlark(this)] this: &RepoCtx,
        #[starlark(require = pos)] program: &str,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let path = this
            .state
            .declared_env
            .get("PATH")
            .cloned()
            .unwrap_or_default();
        #[cfg(windows)]
        let pathext: Vec<String> = this
            .state
            .declared_env
            .get("PATHEXT")
            .map(|s| s.split(';').map(|x| x.to_string()).collect())
            .unwrap_or_default();
        #[cfg(not(windows))]
        let pathext: Vec<String> = vec![String::new()];
        let separator = if cfg!(windows) { ';' } else { ':' };
        for dir in path.split(separator) {
            if dir.is_empty() {
                continue;
            }
            for ext in &pathext {
                let candidate = std::path::PathBuf::from(dir).join(format!("{program}{ext}"));
                if is_executable(&candidate) {
                    return Ok(heap.alloc(candidate.to_string_lossy().to_string()));
                }
            }
        }
        Ok(Value::new_none())
    }

    /// `ctx.getenv(name, default=None)` — reads the declared env (host
    /// snapshot + `--repo_env` overrides). Records an `EnvVar`
    /// observation so the marker tracks this variable.
    fn getenv<'v>(
        #[starlark(this)] this: &RepoCtx,
        #[starlark(require = pos)] name: &str,
        #[starlark(require = pos, default = NoneType)] default: Value<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let value = this.state.declared_env.get(name).cloned();
        this.state.record(RepoRecordedInput::EnvVar {
            name: name.to_string(),
            value: value.clone(),
        });
        match value {
            Some(v) => Ok(heap.alloc(v)),
            None => Ok(default),
        }
    }

    /// `ctx.read(path)` — read a file as a string and record its
    /// content digest. Path is resolved against the workdir if relative.
    fn read<'v>(
        #[starlark(this)] this: &RepoCtx,
        #[starlark(require = pos)] path: Value<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let p = unpack_path_arg(path, &this.state)?;
        let value = observe_file(&p);
        this.state.record(RepoRecordedInput::File {
            path: p.clone(),
            value: value.clone(),
        });
        match value {
            FileValue::Digest(_) => {
                let bytes =
                    std::fs::read(&p).map_err(|e| anyhow!("ctx.read({}): {e}", p.display()))?;
                let text = String::from_utf8_lossy(&bytes).into_owned();
                Ok(heap.alloc(text))
            }
            FileValue::Dir => Err(anyhow!("ctx.read({}): is a directory", p.display())),
            FileValue::Missing => Err(anyhow!("ctx.read({}): file not found", p.display())),
        }
    }

    /// `ctx.watch(path)` — record an observation of the file without
    /// reading its contents into Starlark. Used by rules that read via
    /// `ctx.execute` (e.g. `cat`) but still want cache invalidation.
    fn watch<'v>(
        #[starlark(this)] this: &RepoCtx,
        #[starlark(require = pos)] path: Value<'v>,
    ) -> anyhow::Result<NoneType> {
        let p = unpack_path_arg(path, &this.state)?;
        let value = observe_file(&p);
        this.state
            .record(RepoRecordedInput::File { path: p, value });
        Ok(NoneType)
    }

    /// `ctx.watch_tree(path)` — record a recursive digest of the
    /// directory tree. Use sparingly: scales with tree size.
    fn watch_tree<'v>(
        #[starlark(this)] this: &RepoCtx,
        #[starlark(require = pos)] path: Value<'v>,
    ) -> anyhow::Result<NoneType> {
        let p = unpack_path_arg(path, &this.state)?;
        let value = observe_dirtree(&p);
        this.state
            .record(RepoRecordedInput::DirTree { path: p, value });
        Ok(NoneType)
    }

    /// `ctx.path(s)` — construct a [`RepoPath`] handle for further
    /// `.read()` / `.watch()` / `.exists` / `.basename` operations.
    /// Accepts a path string (workdir-relative or absolute) or a
    /// `Label("@foo//pkg:f")` value — the latter resolves the apparent
    /// repo through the rule's defining `.bzl`'s `repo_mapping` and
    /// lazy-fetches the target repo via DICE.
    fn path<'v>(
        #[starlark(this)] this: &RepoCtx,
        #[starlark(require = pos)] arg: Value<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let p = unpack_path_arg(arg, &this.state)?;
        Ok(heap.alloc(RepoPath {
            path: p,
            state: Arc::clone(&this.state),
        }))
    }

    /// `ctx.template(path, template, substitutions={}, executable=False)`
    /// — read `template` (a workdir-relative path or absolute path),
    /// apply Bazel-style `{KEY}` substitutions, write to `path`.
    /// `template` is observed (records a File entry).
    fn template<'v>(
        #[starlark(this)] this: &RepoCtx,
        #[starlark(require = pos)] path: Value<'v>,
        #[starlark(require = pos)] template: Value<'v>,
        // Bazel: substitutions accepted positionally. Vendored
        // rules_fuzzing's `_oss_fuzz_repository` calls
        // `ctx.template("BUILD", ctx.path(...), {...})` positionally.
        substitutions: Option<Value<'v>>,
        #[starlark(require = named, default = false)] executable: bool,
    ) -> anyhow::Result<NoneType> {
        // Bazel: both `path` (destination) and `template` (source)
        // accept either str or path.
        let dest_path = unpack_path_arg(path, &this.state)?;
        let template_path = unpack_path_arg(template, &this.state)?;
        let template_value = observe_file(&template_path);
        this.state.record(RepoRecordedInput::File {
            path: template_path.clone(),
            value: template_value.clone(),
        });
        let raw = match template_value {
            FileValue::Digest(_) => std::fs::read_to_string(&template_path)
                .map_err(|e| anyhow!("ctx.template: read {}: {e}", template_path.display()))?,
            FileValue::Dir => {
                return Err(anyhow!(
                    "ctx.template: template `{}` is a directory",
                    template_path.display()
                ));
            }
            FileValue::Missing => {
                return Err(anyhow!(
                    "ctx.template: template `{}` not found",
                    template_path.display()
                ));
            }
        };
        let mut subs: Vec<(String, String)> = Vec::new();
        if let Some(sv) = substitutions
            && let Some(dict) = starlark::values::dict::DictRef::from_value(sv)
        {
            for (k, v) in dict.iter() {
                let key = k
                    .unpack_str()
                    .ok_or_else(|| anyhow!("ctx.template: substitution keys must be strings"))?;
                let val = v
                    .unpack_str()
                    .ok_or_else(|| anyhow!("ctx.template: substitution values must be strings"))?;
                subs.push((key.to_string(), val.to_string()));
            }
        }
        // Longest-key-first replacement so `{FOO_BAR}` substitutes
        // before `{FOO}` when both are defined.
        subs.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        let mut rendered = raw;
        for (k, v) in &subs {
            rendered = rendered.replace(k, v);
        }
        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest_path, rendered)?;
        #[cfg(unix)]
        if executable {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dest_path, std::fs::Permissions::from_mode(0o755))?;
        }
        #[cfg(not(unix))]
        let _ = executable;
        Ok(NoneType)
    }

    /// `ctx.delete(path)` — remove a file or directory under the
    /// workdir. Returns True on success, False if the entry didn't
    /// exist. No observation is recorded (deletes are outputs).
    fn delete<'v>(
        #[starlark(this)] this: &RepoCtx,
        #[starlark(require = pos)] path: Value<'v>,
    ) -> anyhow::Result<bool> {
        // Bazel: ctx.delete accepts either str or path (vendored
        // utils.bzl::patch passes `ctx.path(_REMOTE_PATCH_DIR)`).
        let p = unpack_path_arg(path, &this.state)?;
        let meta = match std::fs::symlink_metadata(&p) {
            Ok(m) => m,
            Err(_) => return Ok(false),
        };
        if meta.is_dir() {
            std::fs::remove_dir_all(&p)?;
        } else {
            std::fs::remove_file(&p)?;
        }
        Ok(true)
    }

    /// `ctx.symlink(target, link_path)` — raises on Windows
    /// ("real or fail, never silent stubs" — DESIGN.md), creates a real
    /// symlink on Unix.
    fn symlink<'v>(
        #[starlark(this)] this: &RepoCtx,
        #[starlark(require = pos)] target: Value<'v>,
        #[starlark(require = pos)] link_path: Value<'v>,
    ) -> anyhow::Result<NoneType> {
        // Bazel: ctx.symlink accepts str or path on both sides.
        let target_path = unpack_path_arg(target, &this.state)?;
        let link = unpack_path_arg(link_path, &this.state)?;
        #[cfg(windows)]
        {
            let _ = (target_path, link);
            return Err(anyhow!(
                "ctx.symlink is not supported on Windows in loupe v1"
            ));
        }
        #[cfg(unix)]
        {
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if link.exists() || link.is_symlink() {
                std::fs::remove_file(&link)?;
            }
            std::os::unix::fs::symlink(&target_path, &link)?;
            Ok(NoneType)
        }
    }
}

struct ExecResult {
    return_code: i32,
    stdout: String,
    stderr: String,
}

fn run_subprocess(
    args: &[String],
    env: &BTreeMap<String, String>,
    cwd: &Path,
    timeout_seconds: i32,
) -> anyhow::Result<ExecResult> {
    use std::io::Read;
    use std::process::Command;
    use std::process::Stdio;
    use std::time::Duration;
    use std::time::Instant;

    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    cmd.env_clear();
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.current_dir(cwd);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("ctx.execute spawn `{}`: {e}", args[0]))?;

    // Drain stdout/stderr on dedicated threads. Without this, a child
    // that writes more than the OS pipe buffer (~64KB) blocks on
    // `write` while the parent's `try_wait` poll loop never reads —
    // the classic subprocess deadlock. Real-world `yq pnpm-lock.yaml`
    // emits multi-MB JSON and reliably hangs without concurrent
    // drainage.
    let stdout_handle = child.stdout.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            buf
        })
    });
    let stderr_handle = child.stderr.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            buf
        })
    });

    let deadline = Instant::now() + Duration::from_secs(timeout_seconds.max(0) as u64);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            // Drain whatever the reader threads have so far; ignore
            // errors — the child got killed mid-write.
            let _ = stdout_handle.and_then(|h| h.join().ok());
            let _ = stderr_handle.and_then(|h| h.join().ok());
            return Ok(ExecResult {
                return_code: 256,
                stdout: String::new(),
                stderr: format!("timeout after {timeout_seconds}s"),
            });
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    let out = stdout_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let err = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let code = status.code().unwrap_or_else(|| {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            status.signal().map(|s| 128 + s).unwrap_or(-1)
        }
        #[cfg(not(unix))]
        {
            -1
        }
    });
    Ok(ExecResult {
        return_code: code,
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
    })
}

fn is_executable(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn unpack_string_list_arg<'v>(
    v: Value<'v>,
    label: &str,
    state: &RepoCtxState,
) -> anyhow::Result<Vec<String>> {
    if let Some(list) = starlark::values::list::ListRef::from_value(v) {
        let mut out = Vec::with_capacity(list.len());
        for item in list.iter() {
            if let Some(s) = item.unpack_str() {
                out.push(s.to_string());
            } else if let Some(p) = item.downcast_ref::<RepoPath>() {
                out.push(p.path.to_string_lossy().into_owned());
            } else if let Some(lv) = item.downcast_ref::<loupe_labels::LabelValue>() {
                // Bazel: Label entries in argv resolve via the
                // label-to-arg conversion (on-disk path of the
                // referenced file). Matches ctx.path(Label(...)).
                let path = state.resolve_label(&lv.repo, &lv.package, &lv.name)?;
                out.push(path.to_string_lossy().into_owned());
            } else {
                return Err(anyhow!(
                    "{label}: list entry must be a string, path, or Label, got {}",
                    item.get_type()
                ));
            }
        }
        return Ok(out);
    }
    if let Some(s) = v.unpack_str() {
        return Ok(vec![s.to_string()]);
    }
    Err(anyhow!(
        "{label}: must be a string or list of strings, got {}",
        v.get_type()
    ))
}

fn unpack_path_arg<'v>(v: Value<'v>, state: &RepoCtxState) -> anyhow::Result<PathBuf> {
    if let Some(p) = v.downcast_ref::<RepoPath>() {
        return Ok(p.path.clone());
    }
    if let Some(lv) = v.downcast_ref::<loupe_labels::LabelValue>() {
        return state.resolve_label(&lv.repo, &lv.package, &lv.name);
    }
    if let Some(s) = v.unpack_str() {
        // A *string* path is a literal repo-relative filesystem path,
        // not a label — Bazel only treats an actual `Label` object as a
        // label here. The one exception: label-typed repo-rule attrs
        // surface their default as a label-shaped *string* (see
        // engine.rs `resolve_relative_label_to_canonical_str`), and
        // `ctx.path(ctx.attr._template)` relies on it resolving. Every
        // such form carries the repo/package separator `//`
        // (`@@canon//pkg:f`, `@apparent//pkg:f`, `@//pkg:f`, `//pkg:f`),
        // so gate label resolution on `//`. Without it, an npm scoped
        // package path like `@types/node/resolved.json` — which
        // rules_js's npm `_hub_repo_impl` writes via `rctx.file` — was
        // misparsed as a label with an empty canonical and failed with
        // "no RepoSpec registered".
        let looks_like_label = s.starts_with("//") || (s.starts_with('@') && s.contains("//"));
        if looks_like_label {
            let parsed = loupe_labels::LabelValue::parse(s);
            return state.resolve_label(&parsed.repo, &parsed.package, &parsed.name);
        }
        return Ok(state.resolve(s));
    }
    Err(anyhow!(
        "expected string, path, or Label, got {}",
        v.get_type()
    ))
}

/// A handle returned by `ctx.path(...)`. Carries the resolved `PathBuf`
/// plus the shared `RepoCtxState` so `.read()` / `.watch()` calls
/// produce the same records they would going through `ctx.read` /
/// `ctx.watch` directly.
///
/// Bazel's `path.__str__` returns the bare filesystem path. The
/// vendored `http.bzl::_http_file_impl` does
/// `str(download_path).startswith(str(repo_root))` — wrapping the
/// path in `<path …>` breaks that check.
#[derive(Debug, Clone, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("{}", path.display())]
pub struct RepoPath {
    pub path: PathBuf,
    #[allocative(skip)]
    pub state: Arc<RepoCtxState>,
}

starlark_simple_value!(RepoPath);

#[starlark_value(type = "path")]
impl<'v> StarlarkValue<'v> for RepoPath {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "basename" => Some(
                heap.alloc(
                    self.path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                ),
            ),
            "dirname" => Some(
                heap.alloc(RepoPath {
                    path: self
                        .path
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| PathBuf::from("/")),
                    state: Arc::clone(&self.state),
                }),
            ),
            "exists" => Some(Value::new_bool(self.path.exists())),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        ["basename", "dirname", "exists"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(repo_path_methods)
    }
}

#[starlark_module]
fn repo_path_methods(b: &mut MethodsBuilder) {
    /// `path.read()` — read this file as text, record its digest.
    fn read<'v>(#[starlark(this)] this: &RepoPath, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let value = observe_file(&this.path);
        this.state.record(RepoRecordedInput::File {
            path: this.path.clone(),
            value: value.clone(),
        });
        match value {
            FileValue::Digest(_) => {
                let bytes = std::fs::read(&this.path)
                    .map_err(|e| anyhow!("path.read({}): {e}", this.path.display()))?;
                Ok(heap.alloc(String::from_utf8_lossy(&bytes).into_owned()))
            }
            FileValue::Dir => Err(anyhow!(
                "path.read({}): is a directory",
                this.path.display()
            )),
            FileValue::Missing => Err(anyhow!(
                "path.read({}): file not found",
                this.path.display()
            )),
        }
    }

    /// `path.watch()` — observe without reading into Starlark.
    fn watch(#[starlark(this)] this: &RepoPath) -> anyhow::Result<NoneType> {
        let value = observe_file(&this.path);
        this.state.record(RepoRecordedInput::File {
            path: this.path.clone(),
            value,
        });
        Ok(NoneType)
    }

    /// `path.is_dir` — exists as a directory.
    fn is_dir(#[starlark(this)] this: &RepoPath) -> anyhow::Result<bool> {
        Ok(this.path.is_dir())
    }

    /// `path.realpath` — canonicalise, returning a new path.
    fn realpath<'v>(
        #[starlark(this)] this: &RepoPath,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let real = std::fs::canonicalize(&this.path).unwrap_or_else(|_| this.path.clone());
        Ok(heap.alloc(RepoPath {
            path: real,
            state: Arc::clone(&this.state),
        }))
    }

    /// `path.readdir()` — list the directory's entries as `path`
    /// objects (each a full child path), matching Bazel: callers do
    /// `for f in p.readdir(): ctx.symlink(f, f.basename)` (rules_java's
    /// `local_java_repository`). Records a `Dirents` observation so the
    /// marker tracks the entry set.
    fn readdir<'v>(#[starlark(this)] this: &RepoPath, heap: Heap<'v>) -> anyhow::Result<Value<'v>> {
        let value = observe_dirents(&this.path);
        this.state.record(RepoRecordedInput::Dirents {
            path: this.path.clone(),
            value: value.clone(),
        });
        match value {
            DirentsValue::Fingerprint(_) => {}
            DirentsValue::Missing => {
                return Err(anyhow!(
                    "path.readdir({}): not a directory",
                    this.path.display()
                ));
            }
        }
        let mut names: Vec<String> = std::fs::read_dir(&this.path)
            .map_err(|e| anyhow!("path.readdir({}): {e}", this.path.display()))?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        let entries: Vec<Value<'v>> = names
            .into_iter()
            .map(|name| {
                heap.alloc(RepoPath {
                    path: this.path.join(name),
                    state: Arc::clone(&this.state),
                })
            })
            .collect();
        Ok(heap.alloc(entries))
    }

    /// `path.get_child(name)` — join a child path.
    fn get_child<'v>(
        #[starlark(this)] this: &RepoPath,
        #[starlark(require = pos)] name: &str,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        Ok(heap.alloc(RepoPath {
            path: this.path.join(name),
            state: Arc::clone(&this.state),
        }))
    }
}

fn unpack_url_arg<'v>(v: Value<'v>) -> anyhow::Result<Vec<String>> {
    if let Some(s) = v.unpack_str() {
        return Ok(vec![s.to_string()]);
    }
    if let Some(list) = starlark::values::list::ListRef::from_value(v) {
        let mut out = Vec::with_capacity(list.len());
        for item in list.iter() {
            let s = item.unpack_str().ok_or_else(|| {
                anyhow!(
                    "ctx.download_and_extract: url list entry must be a string, got {}",
                    item.get_type()
                )
            })?;
            out.push(s.to_string());
        }
        return Ok(out);
    }
    Err(anyhow!(
        "ctx.download_and_extract: url= must be a string or list of strings, got {}",
        v.get_type()
    ))
}

/// Lower-case hex SHA-256 of the given bytes. Bazel's
/// `ctx.download(...).sha256` returns this shape; vendored rules
/// (rules_buf, rules_python) chain the result.
fn sha256_hex_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn archive_kind_from_explicit(s: &str) -> Option<ArchiveKind> {
    match s.to_ascii_lowercase().as_str() {
        "tar.gz" | "tgz" => Some(ArchiveKind::TarGz),
        "tar.xz" | "txz" => Some(ArchiveKind::TarXz),
        "tar.zst" | "tar.zstd" => Some(ArchiveKind::TarZst),
        "tar" => Some(ArchiveKind::Tar),
        "zip" | "jar" => Some(ArchiveKind::Zip),
        _ => None,
    }
}

/// Register the bzlmod globals (`repository_rule`, `tag_class`,
/// `module_extension`) into a `.bzl`-phase `GlobalsBuilder`.
///
/// `module_extension` and `tag_class` are placeholders for now — they
/// accept the kwargs without erroring, so vendored `.bzl` files that
/// reference them load. Real implementations land alongside extension
/// eval.
pub fn register_for_bzlmod(builder: &mut GlobalsBuilder) {
    bzlmod_globals(builder);
}

#[allow(clippy::too_many_arguments)]
#[starlark_module]
fn bzlmod_globals(b: &mut GlobalsBuilder) {
    fn repository_rule<'v>(
        // Bazel's surface accepts `implementation` positionally too —
        // `rules_nodejs` and friends call `repository_rule(_my_impl, ...)`.
        #[starlark(require = pos)] implementation_pos: Option<Value<'v>>,
        #[starlark(require = named)] implementation: Option<Value<'v>>,
        #[starlark(require = named)] attrs: Option<Value<'v>>,
        #[starlark(require = named, default = false)] local: bool,
        #[starlark(require = named, default = false)] configure: bool,
        #[starlark(require = named)] environ: Option<UnpackList<String>>,
        #[starlark(require = named, default = "")] doc: &str,
        #[starlark(require = named)] remotable: Option<Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<RepositoryRule<'v>> {
        let _ = (doc, remotable);
        let implementation = match (implementation_pos, implementation) {
            (Some(p), None) => p,
            (None, Some(n)) => n,
            (Some(_), Some(_)) => {
                return Err(anyhow!(
                    "repository_rule() got both positional and named implementation; pick one"
                ));
            }
            (None, None) => {
                return Err(anyhow!(
                    "repository_rule(implementation=…) is required and must be a callable"
                ));
            }
        };
        let attrs = attrs.unwrap_or_else(|| {
            let pairs: Vec<(&str, Value<'v>)> = Vec::new();
            heap.alloc(AllocStruct(pairs))
        });
        let environ: Vec<String> = environ.map(|l| l.into_iter().collect()).unwrap_or_default();
        Ok(RepositoryRuleGen {
            implementation,
            attrs,
            local,
            configure,
            environ,
        })
    }

    fn tag_class<'v>(
        #[starlark(require = named)] attrs: Option<Value<'v>>,
        // Bazel-compat: `tag_class(doc=...)` for human-readable docs.
        // Accepted-and-ignored — Loupe doesn't surface doc strings.
        #[starlark(require = named, default = "")] doc: &str,
        heap: Heap<'v>,
    ) -> anyhow::Result<TagClass<'v>> {
        let _ = doc;
        let attrs = attrs.unwrap_or_else(|| {
            let pairs: Vec<(&str, Value<'v>)> = Vec::new();
            heap.alloc(AllocStruct(pairs))
        });
        Ok(TagClassGen { attrs })
    }

    fn module_extension<'v>(
        // `implementation` is accepted both positionally (rules_cc's
        // `compatibility_proxy = module_extension(_compat_proxy_impl)`)
        // and named (`module_extension(implementation = _impl, ...)`)
        // in the wild. The default `require = both` lets either form
        // work; spell it explicitly so the intent stays clear.
        implementation: Value<'v>,
        #[starlark(require = named)] tag_classes: Option<Value<'v>>,
        #[starlark(require = named)] environ: Option<UnpackList<String>>,
        #[starlark(require = named, default = false)] os_dependent: bool,
        #[starlark(require = named, default = false)] arch_dependent: bool,
        #[starlark(require = named, default = "")] doc: &str,
        heap: Heap<'v>,
    ) -> anyhow::Result<ModuleExtension<'v>> {
        let _ = (environ, doc);
        let tag_classes = tag_classes.unwrap_or_else(|| {
            let pairs: Vec<(&str, Value<'v>)> = Vec::new();
            heap.alloc(AllocStruct(pairs))
        });
        Ok(ModuleExtensionGen {
            implementation,
            tag_classes,
            os_dependent,
            arch_dependent,
        })
    }
}

/// One module's tag invocations passed into a module_extension via
/// `module_ctx.modules[i].tags.<class>`. Plain Rust data; constructed
/// from MODULE.bazel parsing or by test helpers.
#[derive(Debug, Clone, Allocative)]
pub struct ModuleTagInvocation {
    pub class: String,
    pub attrs: BTreeMap<String, RepoAttrValue>,
}

/// One module visible to an extension. `is_root` marks the workspace's
/// own module (Bazel's `module(name="demo")`); transitive `bazel_dep`
/// modules show up here with `is_root=false`.
#[derive(Debug, Clone, Allocative)]
pub struct ModuleView {
    pub name: String,
    pub version: String,
    pub is_root: bool,
    /// Canonical name of the module (Bazel's `<name>+<version>` or
    /// `<name>~<version>` form; empty for the root module). Carries
    /// from MODULE.bazel parsing so `attr.label` tag-attr values can
    /// be canonicalised against this module's repo_mapping —
    /// `lock_file = "//:maven_install.json"` in a transitive module
    /// must anchor on that module's repo, not the root workspace.
    pub canonical: String,
    pub tags: Vec<ModuleTagInvocation>,
}

#[derive(Clone, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("<module_ctx>")]
pub struct ModuleCtx {
    /// JSON-decoded view of each module visible to this extension.
    /// Built outside of Starlark; surfaces via `module_ctx.modules`
    /// as a list of structs.
    pub modules: Vec<ModuleView>,
    /// True when called with no tag invocations across all modules
    /// (matches Bazel's `module_ctx.root_module_has_non_dev_dependency`
    /// shape — though v1 doesn't surface that specific accessor).
    pub root_is_only_module: bool,
    /// Names of every `tag_class` the extension declared. Used to
    /// ensure `module.tags.<class>` is always defined (as an empty
    /// list when no invocations matched), matching Bazel's behaviour.
    pub all_tag_classes: Vec<String>,
    /// Effective environment (host snapshot + `--repo_env` overrides).
    /// Surfaced to extension impls via `module_ctx.getenv(NAME)`. Shares
    /// the same source as `RepoCtx`'s `declared_env`, mirroring Bazel's
    /// `StarlarkBaseExternalContext::getEnvironmentVariable`.
    pub declared_env: Arc<BTreeMap<String, String>>,
    /// User's workspace root. Surfaced through main-repo Label
    /// resolution (`module_ctx.path(Label("@@//pkg:name"))`).
    /// Deliberately NOT the default for `module_ctx.path("")` /
    /// `module_ctx.execute(cwd=...)` — Bazel runs extensions in an
    /// isolated working directory under `output_base` to prevent an
    /// impl from accidentally (or maliciously) writing back into the
    /// source tree.
    pub workspace_root: PathBuf,
    /// Per-extension isolated working directory. Default for relative
    /// `module_ctx.path(...)` lookups and `module_ctx.execute(cwd=...)`.
    /// Engine-supplied: `<bzlmod_cache>/.extension_wd/<id>` when a
    /// cache is configured, otherwise a process-lifetime tempdir. The
    /// directory is pre-created before the impl runs.
    pub extension_wd: PathBuf,
    /// `apparent → canonical` map for resolving labels passed to
    /// `module_ctx.path(Label(...))`. Sourced from the main module's
    /// `repo_mapping`; ext-generated repos appear via `use_repo(...)`
    /// synthesis in `module_graph.rs`. Falls back to `<apparent>+`
    /// canonical when the apparent name isn't in the map.
    #[allocative(skip)]
    pub repo_mapping: Arc<loupe_labels::RepoMapping>,
    /// Materialised-repo cache directory (the `--cache` arg). Combined
    /// with the resolved canonical to form an absolute path to the
    /// repo's on-disk content. `None` when no cache was supplied (e.g.
    /// pure-vendor mode) — external-repo labels then error explicitly.
    pub bzlmod_cache: Option<PathBuf>,
    /// Bridge to DICE for lazy materialisation when an extension impl
    /// calls `module_ctx.path(Label("@external_repo//..."))`. Without
    /// this, the path is constructed but the target repo's
    /// `MaterializedRepoKey` never fires — so any `mctx.execute(path)`
    /// or `mctx.read(path)` against the returned handle hits a missing
    /// directory. Tests pass an [`UnresolvedLabelResolver`]; the engine
    /// installs a [`ChannelLabelResolver`] that pumps `LabelReq`s out
    /// to its `select!` loop.
    #[allocative(skip)]
    pub label_resolver: Arc<dyn crate::label_resolver::LabelResolver>,
    /// HTTP client + runtime handle for `module_ctx.download` and
    /// `module_ctx.download_and_extract`. Same client the engine uses
    /// for BCR fetches; the handle lets the sync starlark eval bridge
    /// into an async `download(...)` via `handle.block_on(...)`.
    #[allocative(skip)]
    pub client: reqwest::Client,
    #[allocative(skip)]
    pub handle: tokio::runtime::Handle,
}

impl std::fmt::Debug for ModuleCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `label_resolver` / `client` / `handle` skip Debug.
        f.debug_struct("ModuleCtx")
            .field("modules", &self.modules)
            .field("root_is_only_module", &self.root_is_only_module)
            .field("all_tag_classes", &self.all_tag_classes)
            .field("workspace_root", &self.workspace_root)
            .field("extension_wd", &self.extension_wd)
            .field("bzlmod_cache", &self.bzlmod_cache)
            .finish_non_exhaustive()
    }
}

starlark_simple_value!(ModuleCtx);

#[starlark_value(type = "module_ctx")]
impl<'v> StarlarkValue<'v> for ModuleCtx {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "modules" => Some(build_modules_value(
                heap,
                &self.modules,
                &self.all_tag_classes,
            )),
            "os" => Some(build_os_struct(heap, &self.declared_env)),
            "is_isolated" => Some(Value::new_bool(false)),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        ["modules", "os", "is_isolated"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(module_ctx_methods)
    }
}

/// Resolve a `module_ctx.path` / `module_ctx.read` argument into a
/// filesystem path. Accepts strings (workspace-relative or absolute,
/// `""` / `"."` → workspace root), [`ModulePath`] handles, and
/// `Label` values. External-repo labels resolve via
/// `ctx.repo_mapping` + `ctx.bzlmod_cache`; main-repo labels resolve
/// against the workspace root.
fn module_ctx_resolve_path<'v>(v: Value<'v>, ctx: &ModuleCtx) -> anyhow::Result<PathBuf> {
    if let Some(mp) = v.downcast_ref::<ModulePath>() {
        return Ok(mp.path.clone());
    }
    if let Some(lv) = v.downcast_ref::<loupe_labels::LabelValue>() {
        return resolve_label_path(lv, ctx);
    }
    if let Some(s) = v.unpack_str() {
        if s.starts_with('@') || s.starts_with("//") {
            let parsed = loupe_labels::LabelValue::parse(s);
            return resolve_label_path(&parsed, ctx);
        }
        // Bazel: relative paths in `module_ctx.path(...)` resolve
        // against the extension's working directory (an isolated
        // output_base subdir), NOT the workspace root. Mirroring this
        // keeps an impl from accidentally probing user source via
        // `module_ctx.path("")` or "..".
        if s.is_empty() || s == "." {
            return Ok(ctx.extension_wd.clone());
        }
        let pb = PathBuf::from(s);
        return Ok(if pb.is_absolute() {
            pb
        } else {
            ctx.extension_wd.join(pb)
        });
    }
    Err(anyhow!(
        "expected str, path, or Label (got `{}`)",
        v.get_type()
    ))
}

/// Resolve a Label into a filesystem path. Main-repo labels
/// (`repo == ""`) anchor on `workspace_root`. External labels are
/// canonicalised through the using module's repo_mapping; the
/// resolution is delegated to `ctx.label_resolver` so the target repo
/// gets materialised before the path is returned. Without that step,
/// downstream `mctx.execute(path)` / `mctx.read(path)` hit a missing
/// directory.
fn resolve_label_path(lv: &loupe_labels::LabelValue, ctx: &ModuleCtx) -> anyhow::Result<PathBuf> {
    let repo = lv.repo.as_str();
    let package = lv.package.as_str();
    let name = lv.name.as_str();
    if repo.is_empty() {
        return Ok(append_pkg_name(
            ctx.workspace_root.to_path_buf(),
            package,
            name,
        ));
    }
    // For external labels we route through the resolver so DICE can
    // materialise the target canonical first. The resolver handles
    // both the `@@<canonical>` (already-canonical) and `@<apparent>`
    // (needs mapping) shapes via its internal `looks_canonical`
    // heuristic + the supplied `repo_mapping`.
    ctx.label_resolver
        .resolve(repo, package, name, &ctx.repo_mapping)
        .map(|r| r.path)
        .map_err(|e| anyhow!(e))
}

fn append_pkg_name(mut p: PathBuf, package: &str, name: &str) -> PathBuf {
    if !package.is_empty() {
        p.push(package);
    }
    if !name.is_empty() {
        p.push(name);
    }
    p
}

#[starlark_module]
#[allow(clippy::too_many_arguments)] // generated ctor; Bazel surface
fn module_ctx_methods(b: &mut MethodsBuilder) {
    /// `module_ctx.extension_metadata(root_module_direct_deps=None, ...)` —
    /// Bazel uses this to report repo visibility & strict-deps info
    /// back to the framework. v1 returns an opaque sentinel that
    /// Bazel-style impls can return-or-ignore without error.
    fn extension_metadata<'v>(
        #[starlark(this)] _this: &ModuleCtx,
        #[starlark(require = named)] root_module_direct_deps: Option<Value<'v>>,
        #[starlark(require = named)] root_module_direct_dev_deps: Option<Value<'v>>,
        #[starlark(require = named)] reproducible: Option<Value<'v>>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (
            root_module_direct_deps,
            root_module_direct_dev_deps,
            reproducible,
        );
        let pairs: Vec<(&str, Value<'v>)> = Vec::new();
        Ok(heap.alloc(AllocStruct(pairs)))
    }

    /// `module_ctx.is_dev_dependency(tag)` — v1 returns False
    /// uniformly; dev-deps aren't tracked yet.
    fn is_dev_dependency<'v>(
        #[starlark(this)] _this: &ModuleCtx,
        #[starlark(require = pos)] tag: Value<'v>,
    ) -> anyhow::Result<bool> {
        let _ = tag;
        Ok(false)
    }

    /// `module_ctx.getenv(name, default=None)` — reads the declared env
    /// (host snapshot + `--repo_env` overrides). Mirrors
    /// `repository_ctx.getenv` (Bazel sources both from the same
    /// `StarlarkBaseExternalContext::getEnvironmentVariable`).
    fn getenv<'v>(
        #[starlark(this)] this: &ModuleCtx,
        #[starlark(require = pos)] name: &str,
        #[starlark(require = pos, default = NoneType)] default: Value<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        match this.declared_env.get(name) {
            Some(v) => Ok(heap.alloc(v.as_str())),
            None => Ok(default),
        }
    }

    /// `module_ctx.report_progress(message)` — non-fatal status.
    fn report_progress(
        #[starlark(this)] _this: &ModuleCtx,
        #[starlark(require = pos)] status: &str,
    ) -> anyhow::Result<NoneType> {
        tracing::debug!(status = %status, "module_extension progress");
        Ok(NoneType)
    }

    /// `module_ctx.path(arg)` — construct a [`ModulePath`] handle.
    /// Bazel: relative paths resolve against the workspace root (a
    /// module extension has no per-repo workdir). `""` and `"."` both
    /// return the workspace root; vendored rules read it via
    /// `str(rctx.path(""))` (e.g. aspect_rules_js's
    /// `npm_translate_lock_state.bzl:497`).
    fn path<'v>(
        #[starlark(this)] this: &ModuleCtx,
        #[starlark(require = pos)] arg: Value<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let p = module_ctx_resolve_path(arg, this)?;
        Ok(heap.alloc(ModulePath { path: p }))
    }

    /// `module_ctx.read(path)` — read this file as text. Accepts a
    /// path string (workspace-relative or absolute) or a [`ModulePath`].
    /// No marker side-effects (module extensions don't materialise a
    /// repo; their captures live in `ModuleCtxCaptures`).
    fn read<'v>(
        #[starlark(this)] this: &ModuleCtx,
        #[starlark(require = pos)] arg: Value<'v>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let p = module_ctx_resolve_path(arg, this)?;
        let bytes =
            std::fs::read(&p).map_err(|e| anyhow!("module_ctx.read({}): {e}", p.display()))?;
        Ok(heap.alloc(String::from_utf8_lossy(&bytes).into_owned()))
    }

    /// `module_ctx.watch(path)` — no-op observation. Loupe doesn't
    /// track repo-marker invalidation for module extensions.
    fn watch<'v>(
        #[starlark(this)] _this: &ModuleCtx,
        #[starlark(require = pos)] _arg: Value<'v>,
    ) -> anyhow::Result<NoneType> {
        Ok(NoneType)
    }

    /// `module_ctx.file(path, content="", executable=True, legacy_utf8=False)`
    /// — write a literal file under the extension's working directory.
    /// Mirrors `repository_ctx.file`; rules_js's npm extension writes
    /// its `action_cache` and intermediate JSON files this way before
    /// invoking `mctx.execute([yq, ...])`.
    fn file<'v>(
        #[starlark(this)] this: &ModuleCtx,
        #[starlark(require = pos)] path: Value<'v>,
        #[starlark(default = "")] content: &str,
        #[starlark(require = named, default = true)] executable: bool,
        #[starlark(require = named, default = false)] _legacy_utf8: bool,
    ) -> anyhow::Result<NoneType> {
        let target = module_ctx_resolve_path(path, this)?;
        // `target` from module_ctx_resolve_path is always absolute,
        // so the workdir arg is unused — pass `extension_wd` anyway
        // to keep `do_file`'s signature uniform with the repo_ctx
        // call site.
        do_file(&this.extension_wd, &target, content, executable)?;
        Ok(NoneType)
    }

    /// `module_ctx.download(url, output="", sha256="", executable=False,
    /// integrity="", allow_fail=False, canonical_id="", auth=None)` —
    /// fetch `url` to `output` under the extension's working directory.
    /// Bazel exposes the same surface on `module_ctx` as on
    /// `repository_ctx`; vendored extensions (`rules_go`'s `sdk.bzl`,
    /// rules_js's pnpm fetcher) call it directly. Returns a struct
    /// `{success, sha256, integrity}` mirroring Bazel.
    fn download<'v>(
        #[starlark(this)] this: &ModuleCtx,
        url: Value<'v>,
        #[starlark(default = starlark::values::none::NoneOr::None)]
        output: starlark::values::none::NoneOr<Value<'v>>,
        #[starlark(default = "")] sha256: &str,
        #[starlark(default = false)] executable: bool,
        #[starlark(require = named, default = "")] integrity: &str,
        #[starlark(require = named, default = false)] allow_fail: bool,
        #[starlark(require = named, default = "")] canonical_id: &str,
        #[starlark(require = named)] auth: Option<Value<'v>>,
        #[starlark(require = named)] headers: Option<Value<'v>>,
        #[starlark(require = named, default = true)] block: bool,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (sha256, canonical_id, auth, headers, block);
        let urls = unpack_url_arg(url)?;
        let parsed_integrity = if integrity.is_empty() {
            None
        } else {
            Some(Integrity::parse(integrity)?)
        };
        let output_path = match output {
            starlark::values::none::NoneOr::Other(v) => module_ctx_resolve_path(v, this)?,
            starlark::values::none::NoneOr::None => PathBuf::new(),
        };
        do_download(
            &urls,
            output_path,
            parsed_integrity.as_ref(),
            executable,
            allow_fail,
            &this.extension_wd,
            Some(&this.workspace_root),
            &this.client,
            &this.handle,
            heap,
        )
    }

    /// `module_ctx.execute(argv, timeout=600, environment={}, quiet=True, working_directory="")`
    /// — run a host subprocess and return a struct with `return_code`,
    /// `stdout`, `stderr`. Vendored repo-utils call this for system
    /// probes (`uname -m`, `which curl`, etc.) before laying down
    /// rules. Effective env = declared_env layered with `environment=`.
    /// Argv entries can be strings, `ModulePath`, or main-repo `Label`s
    /// (resolved as workspace-relative paths).
    fn execute<'v>(
        #[starlark(this)] this: &ModuleCtx,
        #[starlark(require = pos)] arguments: Value<'v>,
        #[starlark(require = named, default = 600i32)] timeout: i32,
        #[starlark(require = named)] environment: Option<Value<'v>>,
        #[starlark(require = named, default = true)] quiet: bool,
        #[starlark(require = named, default = "")] working_directory: &str,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = quiet;
        let args = unpack_module_argv(arguments, this)?;
        if args.is_empty() {
            return Err(anyhow!("module_ctx.execute: arguments must be non-empty"));
        }
        let mut env_overrides: BTreeMap<String, Option<String>> = BTreeMap::new();
        if let Some(env_val) = environment {
            if let Some(dict) = starlark::values::dict::DictRef::from_value(env_val) {
                for (k, v) in dict.iter() {
                    let key = k.unpack_str().ok_or_else(|| {
                        anyhow!("module_ctx.execute: environment keys must be strings")
                    })?;
                    if v.is_none() {
                        env_overrides.insert(key.to_string(), None);
                    } else if let Some(val) = v.unpack_str() {
                        env_overrides.insert(key.to_string(), Some(val.to_string()));
                    } else {
                        return Err(anyhow!(
                            "module_ctx.execute: environment values must be strings or None"
                        ));
                    }
                }
            } else {
                return Err(anyhow!(
                    "module_ctx.execute: environment must be a dict, got {}",
                    env_val.get_type()
                ));
            }
        }
        let mut effective: BTreeMap<String, String> = (*this.declared_env).clone();
        for (k, v) in env_overrides {
            match v {
                Some(val) => {
                    effective.insert(k, val);
                }
                None => {
                    effective.remove(&k);
                }
            }
        }
        // Bazel: extensions run under an isolated working directory
        // in the output base — not the workspace root. Default
        // here matches that so a hostile / careless impl can't
        // emit files into the user's source tree by leaving
        // `working_directory=` unset.
        let cwd = if working_directory.is_empty() {
            this.extension_wd.clone()
        } else {
            let raw = PathBuf::from(working_directory);
            if raw.is_absolute() {
                raw
            } else {
                this.extension_wd.join(raw)
            }
        };
        let result = run_subprocess(&args, &effective, &cwd, timeout)?;
        let pairs: Vec<(&'static str, Value<'v>)> = vec![
            ("return_code", heap.alloc(result.return_code)),
            ("stdout", heap.alloc(result.stdout)),
            ("stderr", heap.alloc(result.stderr)),
        ];
        Ok(heap.alloc(AllocStruct(pairs)))
    }

    /// `module_ctx.register_toolchains(*labels)` — register toolchain
    /// labels produced by this extension. Captured into the eval-time
    /// `ModuleCtxCaptures` and surfaced via `ModuleExtensionOutput`;
    /// the engine's toolchain resolver unions these with the
    /// MODULE.bazel-level `register_toolchains` records.
    fn register_toolchains<'v>(
        #[starlark(this)] _this: &ModuleCtx,
        args: &starlark::eval::Arguments<'v, '_>,
        eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<NoneType> {
        let positions = args
            .positions(eval.heap())
            .map_err(|e| anyhow!("module_ctx.register_toolchains: {e}"))?;
        let captures = eval
            .extra
            .ok_or_else(|| {
                anyhow!(
                    "module_ctx.register_toolchains called outside a module_extension impl \
                     (no ModuleCtxCaptures on evaluator)"
                )
            })?
            .downcast_ref::<ModuleCtxCaptures>()
            .ok_or_else(|| anyhow!("module_ctx.register_toolchains: wrong eval.extra type"))?;
        for v in positions {
            let s = v.unpack_str().ok_or_else(|| {
                anyhow!(
                    "module_ctx.register_toolchains: argument must be a string label, got {}",
                    v.get_type()
                )
            })?;
            captures.push_toolchain(s.to_string());
        }
        Ok(NoneType)
    }

    /// `module_ctx.which(program)` — split the declared PATH and
    /// return the first executable named `program`. Returns `None` if
    /// not found. Mirrors `repository_ctx.which`.
    fn which<'v>(
        #[starlark(this)] this: &ModuleCtx,
        #[starlark(require = pos)] program: &str,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let path = this.declared_env.get("PATH").cloned().unwrap_or_default();
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(program);
            if is_executable(&candidate) {
                return Ok(heap.alloc(ModulePath { path: candidate }));
            }
        }
        Ok(Value::new_none())
    }
}

/// Unpack a `module_ctx.execute` argv: list of strings, [`ModulePath`]
/// handles, or main-repo `Label` values (resolved workspace-relative).
fn unpack_module_argv<'v>(v: Value<'v>, ctx: &ModuleCtx) -> anyhow::Result<Vec<String>> {
    let list = starlark::values::list::ListRef::from_value(v).ok_or_else(|| {
        anyhow!(
            "module_ctx.execute: arguments must be a list, got {}",
            v.get_type()
        )
    })?;
    let mut out = Vec::with_capacity(list.len());
    for item in list.iter() {
        if let Some(s) = item.unpack_str() {
            out.push(s.to_string());
        } else if let Some(mp) = item.downcast_ref::<ModulePath>() {
            out.push(mp.path.to_string_lossy().into_owned());
        } else if let Some(lv) = item.downcast_ref::<loupe_labels::LabelValue>() {
            let p = resolve_label_path(lv, ctx)?;
            out.push(p.to_string_lossy().into_owned());
        } else {
            return Err(anyhow!(
                "module_ctx.execute: list entry must be a string, path, or Label, got {}",
                item.get_type()
            ));
        }
    }
    Ok(out)
}

/// Path handle returned by `module_ctx.path(...)`. Mirrors
/// [`RepoPath`] but without the per-repo recorder/state — module
/// extensions don't materialise a repo, so there's nothing to record
/// against. Stringification yields the absolute path.
#[derive(Debug, Clone, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("{}", path.display())]
pub struct ModulePath {
    pub path: PathBuf,
}

starlark_simple_value!(ModulePath);

#[starlark_value(type = "path")]
impl<'v> StarlarkValue<'v> for ModulePath {
    fn get_attr(&self, attribute: &str, heap: Heap<'v>) -> Option<Value<'v>> {
        match attribute {
            "basename" => Some(
                heap.alloc(
                    self.path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                ),
            ),
            "dirname" => Some(
                heap.alloc(ModulePath {
                    path: self
                        .path
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| PathBuf::from("/")),
                }),
            ),
            "exists" => Some(Value::new_bool(self.path.exists())),
            _ => None,
        }
    }

    fn dir_attr(&self) -> Vec<String> {
        ["basename", "dirname", "exists"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(module_path_methods)
    }
}

#[starlark_module]
fn module_path_methods(b: &mut MethodsBuilder) {
    /// `path.get_child(name)` — append a child segment, returning a
    /// new path.
    fn get_child<'v>(
        #[starlark(this)] this: &ModulePath,
        #[starlark(args)] args: starlark::values::tuple::UnpackTuple<&'v str>,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let mut p = this.path.clone();
        for seg in args.items {
            p.push(seg);
        }
        Ok(heap.alloc(ModulePath { path: p }))
    }

    /// `path.readdir()` — list the directory's entries as `path`
    /// objects (each a full child path), matching Bazel (so `.basename`
    /// etc. work on each entry).
    fn readdir<'v>(
        #[starlark(this)] this: &ModulePath,
        heap: Heap<'v>,
    ) -> anyhow::Result<Value<'v>> {
        let entries = std::fs::read_dir(&this.path)
            .map_err(|e| anyhow!("path.readdir({}): {e}", this.path.display()))?;
        let mut names: Vec<String> = Vec::new();
        for e in entries.flatten() {
            names.push(e.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        let vals: Vec<Value<'v>> = names
            .into_iter()
            .map(|name| {
                heap.alloc(ModulePath {
                    path: this.path.join(name),
                })
            })
            .collect();
        Ok(heap.alloc(vals))
    }
}

fn build_modules_value<'v>(
    heap: Heap<'v>,
    modules: &[ModuleView],
    all_tag_classes: &[String],
) -> Value<'v> {
    let items: Vec<Value<'v>> = modules
        .iter()
        .map(|m| build_one_module_value(heap, m, all_tag_classes))
        .collect();
    heap.alloc(items)
}

fn build_one_module_value<'v>(
    heap: Heap<'v>,
    m: &ModuleView,
    all_tag_classes: &[String],
) -> Value<'v> {
    // Group tags by class so `module.tags.<class>` returns a list.
    let mut by_class: BTreeMap<&str, Vec<&ModuleTagInvocation>> = BTreeMap::new();
    for t in &m.tags {
        by_class.entry(t.class.as_str()).or_default().push(t);
    }
    // Always include every tag_class the extension declared, even if
    // this module didn't invoke any — matches Bazel's surface (the
    // extension impl iterates `mod.tags.<class>` without checking).
    for class in all_tag_classes {
        by_class.entry(class.as_str()).or_default();
    }
    let tag_pairs: Vec<(&'v str, Value<'v>)> = by_class
        .into_iter()
        .map(|(class, tags)| {
            let key: &'v str = heap.alloc_str(class).as_str();
            let entries: Vec<Value<'v>> = tags
                .into_iter()
                .map(|t| build_tag_struct(heap, &t.attrs))
                .collect();
            (key, heap.alloc(entries))
        })
        .collect();
    let tags_struct = heap.alloc(AllocStruct(tag_pairs));

    let pairs: Vec<(&'v str, Value<'v>)> = vec![
        ("name", heap.alloc(m.name.as_str())),
        ("version", heap.alloc(m.version.as_str())),
        ("is_root", Value::new_bool(m.is_root)),
        ("tags", tags_struct),
    ];
    heap.alloc(AllocStruct(pairs))
}

/// Extract the keys of `module_extension(tag_classes={...})`. Accepts
/// either a dict or a struct; returns an empty list for anything else.
fn extract_tag_class_names<'v>(v: Value<'v>) -> Vec<String> {
    if let Some(dict) = starlark::values::dict::DictRef::from_value(v) {
        return dict
            .iter()
            .filter_map(|(k, _)| k.unpack_str().map(str::to_string))
            .collect();
    }
    if let Some(strukt) = starlark::values::structs::StructRef::from_value(v) {
        return strukt.iter().map(|(k, _)| k.to_string()).collect();
    }
    Vec::new()
}

fn build_tag_struct<'v>(heap: Heap<'v>, attrs: &BTreeMap<String, RepoAttrValue>) -> Value<'v> {
    let pairs: Vec<(&'v str, Value<'v>)> = attrs
        .iter()
        .map(|(k, v)| {
            let key: &'v str = heap.alloc_str(k).as_str();
            (key, alloc_repo_attr(heap, v))
        })
        .collect();
    heap.alloc(AllocStruct(pairs))
}

fn build_attr_struct<'v>(heap: Heap<'v>, attrs: &BTreeMap<String, RepoAttrValue>) -> Value<'v> {
    let pairs: Vec<(&'v str, Value<'v>)> = attrs
        .iter()
        .map(|(k, v)| {
            let key: &'v str = heap.alloc_str(k).as_str();
            (key, alloc_repo_attr(heap, v))
        })
        .collect();
    heap.alloc(AllocStruct(pairs))
}

fn alloc_repo_attr<'v>(heap: Heap<'v>, v: &RepoAttrValue) -> Value<'v> {
    use starlark::values::dict::AllocDict;
    match v {
        RepoAttrValue::None => Value::new_none(),
        RepoAttrValue::String(s) => heap.alloc(s.as_str()),
        RepoAttrValue::Int(i) => heap.alloc(*i),
        RepoAttrValue::Bool(b) => Value::new_bool(*b),
        RepoAttrValue::StringList(items) => heap.alloc(items.clone()),
        RepoAttrValue::StringListDict(map) => {
            let pairs: Vec<(Value<'v>, Value<'v>)> = map
                .iter()
                .map(|(k, vs)| {
                    let key: Value<'v> = heap.alloc(k.as_str());
                    let val: Value<'v> = heap.alloc(vs.clone());
                    (key, val)
                })
                .collect();
            heap.alloc(AllocDict(pairs))
        }
        RepoAttrValue::StringDict(map) => {
            let pairs: Vec<(&'v str, Value<'v>)> = map
                .iter()
                .map(|(k, v)| {
                    let key: &'v str = heap.alloc_str(k).as_str();
                    (key, heap.alloc(v.as_str()))
                })
                .collect();
            heap.alloc(starlark::values::dict::AllocDict(pairs))
        }
        RepoAttrValue::Label(raw) => heap.alloc(loupe_labels::LabelValue::parse(raw)),
        RepoAttrValue::LabelList(items) => {
            let lvs: Vec<Value<'v>> = items
                .iter()
                .map(|raw| heap.alloc(loupe_labels::LabelValue::parse(raw)))
                .collect();
            heap.alloc(lvs)
        }
        RepoAttrValue::LabelKeyedStringDict(map) => {
            use starlark::values::dict::AllocDict;
            let pairs: Vec<(Value<'v>, Value<'v>)> = map
                .iter()
                .map(|(k, v)| {
                    let key: Value<'v> = heap.alloc(loupe_labels::LabelValue::parse(k));
                    let val: Value<'v> = heap.alloc(v.as_str());
                    (key, val)
                })
                .collect();
            heap.alloc(AllocDict(pairs))
        }
    }
}

/// Synchronous eval entry point: build `RepoCtx`, call `impl(ctx)`,
/// drop the module. `rule_module` is the `FrozenModule` the rule was
/// defined in — its frozen heap is referenced so the rule's
/// `implementation` FrozenValue stays valid through the eval.
///
/// `rule_fv` is a `FrozenValue` pointing at a `FrozenRepositoryRule`
/// in `rule_module`'s frozen heap. Downcasting happens here rather
/// than at spec-construction time so the spec stays simple to build.
#[allow(clippy::too_many_arguments)]
pub fn eval_repository_rule(
    rule_fv: FrozenValue,
    rule_module: &Arc<FrozenModule>,
    canonical: &str,
    attrs: &BTreeMap<String, RepoAttrValue>,
    workdir: &Path,
    workspace_root: Option<PathBuf>,
    client: &reqwest::Client,
    handle: tokio::runtime::Handle,
    declared_env: Arc<BTreeMap<String, String>>,
    repo_mapping: Arc<loupe_labels::RepoMapping>,
    label_resolver: Arc<dyn crate::label_resolver::LabelResolver>,
) -> Result<Vec<RepoRecordedInput>, MaterializeError> {
    let recorder: Arc<Mutex<Vec<RepoRecordedInput>>> = Arc::new(Mutex::new(Vec::new()));
    let state = Arc::new(RepoCtxState {
        canonical: canonical.to_string(),
        workdir: workdir.to_path_buf(),
        workspace_root,
        client: client.clone(),
        handle,
        declared_env,
        repo_mapping,
        recorder: Arc::clone(&recorder),
        label_resolver,
    });
    Module::with_temp_heap(|module| -> Result<(), MaterializeError> {
        module
            .frozen_heap()
            .add_reference(rule_module.frozen_heap());

        let rule = rule_fv
            .downcast_ref::<FrozenRepositoryRule>()
            .ok_or_else(|| {
                MaterializeError::RuleEval(format!(
                    "FrozenValue for `{canonical}` is not a repository_rule (got type `{}`)",
                    rule_fv.to_value().get_type()
                ))
            })?;

        let ctx_value = module.heap().alloc(RepoCtx {
            name: canonical.to_string(),
            attrs: attrs.clone(),
            state: Arc::clone(&state),
        });

        let mut eval = Evaluator::new(&module);
        let impl_value = rule.implementation.to_value();
        eval.eval_function(impl_value, &[ctx_value], &[])
            .map_err(|e| MaterializeError::RuleEval(format!("{e:#}")))?;
        Ok(())
    })?;
    let drained = recorder
        .lock()
        .map(|mut g| std::mem::take(&mut *g))
        .unwrap_or_default();
    Ok(drained)
}

/// Output of running a single module_extension's `impl(module_ctx)`.
/// Each captured `http_archive(...)` (or custom repo_rule call)
/// becomes one entry: the rule's `FrozenValue` + its frozen module
/// anchor + the kwargs the impl passed.
#[derive(Debug, Clone)]
pub struct ModuleExtensionOutput {
    pub repos: Vec<ExtensionRepoCall>,
    /// Raw label strings captured from `module_ctx.register_toolchains(*labels)`
    /// inside the impl. Engine canonicalises against the extension's
    /// effective repo mapping when promoting these into the registered
    /// toolchain set.
    pub registered_toolchains: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ExtensionRepoCall {
    pub name: String,
    pub rule_fv: FrozenValue,
    pub rule_module: Arc<FrozenModule>,
    pub attrs: BTreeMap<String, RepoAttrValue>,
}

/// Synchronous eval entry point for a module_extension. Builds
/// `module_ctx`, calls `impl(module_ctx)`, drains the captured
/// `http_archive(...)`-style calls.
///
/// `extension_fv` must downcast to [`FrozenModuleExtension`].
/// `extension_module` is the `Arc<FrozenModule>` the extension was
/// defined in — held to keep its frozen heap alive.
#[allow(clippy::too_many_arguments)]
pub fn eval_module_extension(
    extension_fv: FrozenValue,
    extension_module: &Arc<FrozenModule>,
    modules: Vec<ModuleView>,
    declared_env: Arc<BTreeMap<String, String>>,
    workspace_root: PathBuf,
    extension_wd: PathBuf,
    repo_mapping: Arc<loupe_labels::RepoMapping>,
    bzlmod_cache: Option<PathBuf>,
    label_resolver: Arc<dyn crate::label_resolver::LabelResolver>,
    client: reqwest::Client,
    handle: tokio::runtime::Handle,
) -> Result<ModuleExtensionOutput, MaterializeError> {
    let captures = ModuleCtxCaptures::new();
    Module::with_temp_heap(|module| -> Result<(), MaterializeError> {
        module
            .frozen_heap()
            .add_reference(extension_module.frozen_heap());
        let ext = extension_fv
            .downcast_ref::<FrozenModuleExtension>()
            .ok_or_else(|| {
                MaterializeError::RuleEval(format!(
                    "FrozenValue is not a module_extension (got type `{}`)",
                    extension_fv.to_value().get_type()
                ))
            })?;
        let root_is_only_module = modules.len() <= 1;
        let all_tag_classes = extract_tag_class_names(ext.tag_classes.to_value());
        let module_ctx = module.heap().alloc(ModuleCtx {
            modules,
            root_is_only_module,
            all_tag_classes,
            declared_env: Arc::clone(&declared_env),
            workspace_root: workspace_root.clone(),
            extension_wd: extension_wd.clone(),
            repo_mapping: Arc::clone(&repo_mapping),
            bzlmod_cache: bzlmod_cache.clone(),
            label_resolver: Arc::clone(&label_resolver),
            client: client.clone(),
            handle: handle.clone(),
        });
        let mut eval = Evaluator::new(&module);
        eval.extra = Some(&captures);
        let impl_value = ext.implementation.to_value();
        eval.eval_function(impl_value, &[module_ctx], &[])
            .map_err(|e| MaterializeError::RuleEval(format!("module_extension impl: {e:#}")))?;
        Ok(())
    })?;

    let drained = captures.take();
    let toolchains = captures.take_toolchains();
    Ok(ModuleExtensionOutput {
        repos: drained
            .into_iter()
            .map(|c| ExtensionRepoCall {
                name: c.name,
                rule_fv: c.rule_fv,
                rule_module: extension_module.clone(),
                attrs: c.attrs,
            })
            .collect(),
        registered_toolchains: toolchains,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_kind_explicit_recognises_common_strings() {
        assert_eq!(
            archive_kind_from_explicit("tar.gz"),
            Some(ArchiveKind::TarGz)
        );
        assert_eq!(archive_kind_from_explicit("TGZ"), Some(ArchiveKind::TarGz));
        assert_eq!(archive_kind_from_explicit("zip"), Some(ArchiveKind::Zip));
        assert_eq!(archive_kind_from_explicit("rar"), None);
    }

    #[test]
    fn register_for_bzlmod_installs_globals() {
        let mut b = GlobalsBuilder::standard();
        register_for_bzlmod(&mut b);
        let g = b.build();
        assert!(g.names().any(|n| n.as_str() == "repository_rule"));
        assert!(g.names().any(|n| n.as_str() == "tag_class"));
        assert!(g.names().any(|n| n.as_str() == "module_extension"));
    }

    fn compile_extension(src: &str, symbol: &str) -> (FrozenValue, Arc<FrozenModule>) {
        use starlark::syntax::{AstModule, Dialect};
        let ast = AstModule::parse("ext.bzl", src.to_string(), &Dialect::Extended).unwrap();
        let globals = GlobalsBuilder::standard().with(register_for_bzlmod).build();
        let frozen = Module::with_temp_heap(|module| -> FrozenModule {
            {
                let mut eval = Evaluator::new(&module);
                eval.eval_module(ast, &globals).unwrap();
            }
            module.freeze().unwrap()
        });
        let frozen = Arc::new(frozen);
        let owned = frozen.get(symbol).unwrap();
        let fv = unsafe { owned.unchecked_frozen_value() };
        (fv, frozen)
    }

    #[test]
    fn module_ctx_getenv_returns_declared_env() {
        let src = r#"
def _impl(module_ctx):
    v = module_ctx.getenv("LOUPE_TEST_VAR")
    if v != "hello":
        fail("expected hello, got " + str(v))
    missing = module_ctx.getenv("LOUPE_TEST_NOPE")
    if missing != None:
        fail("expected None for missing")
    default = module_ctx.getenv("LOUPE_TEST_NOPE", "fallback")
    if default != "fallback":
        fail("expected fallback for default")

ext = module_extension(implementation = _impl)
"#;
        let (fv, frozen) = compile_extension(src, "ext");
        let mut env = BTreeMap::new();
        env.insert("LOUPE_TEST_VAR".to_string(), "hello".to_string());
        let wd = tempfile::tempdir().expect("tempdir");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let out = runtime
            .block_on(async {
                eval_module_extension(
                    fv,
                    &frozen,
                    Vec::new(),
                    Arc::new(env),
                    std::path::PathBuf::from("/tmp"),
                    wd.path().to_path_buf(),
                    Arc::new(loupe_labels::RepoMapping::owned_by(
                        loupe_labels::CanonicalRepo::main(),
                    )),
                    None,
                    Arc::new(crate::label_resolver::UnresolvedLabelResolver),
                    reqwest::Client::new(),
                    tokio::runtime::Handle::current(),
                )
            })
            .expect("module_extension eval");
        assert!(out.repos.is_empty());
    }

    #[test]
    fn path_readdir_returns_path_objects_with_basename() {
        // Regression: `path.readdir()` must yield `path` objects (each with
        // `.basename`), not bare strings — rules_java's
        // `local_java_repository` does `for f in p.readdir():
        // ctx.symlink(f, f.basename)`. Returning strings raised "Object of
        // type `string` has no attribute `basename`".
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("beta.txt"), b"b").unwrap();
        std::fs::write(dir.path().join("alpha.txt"), b"a").unwrap();
        let src = r#"
def _impl(module_ctx):
    d = module_ctx.path(module_ctx.getenv("LOUPE_TEST_DIR"))
    names = [f.basename for f in d.readdir()]
    if names != ["alpha.txt", "beta.txt"]:
        fail("expected sorted basenames, got " + str(names))

ext = module_extension(implementation = _impl)
"#;
        let (fv, frozen) = compile_extension(src, "ext");
        let mut env = BTreeMap::new();
        env.insert(
            "LOUPE_TEST_DIR".to_string(),
            dir.path().to_string_lossy().into_owned(),
        );
        let wd = tempfile::tempdir().expect("tempdir");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        runtime
            .block_on(async {
                eval_module_extension(
                    fv,
                    &frozen,
                    Vec::new(),
                    Arc::new(env),
                    std::path::PathBuf::from("/tmp"),
                    wd.path().to_path_buf(),
                    Arc::new(loupe_labels::RepoMapping::owned_by(
                        loupe_labels::CanonicalRepo::main(),
                    )),
                    None,
                    Arc::new(crate::label_resolver::UnresolvedLabelResolver),
                    reqwest::Client::new(),
                    tokio::runtime::Handle::current(),
                )
            })
            .expect("module_extension eval (readdir basename)");
    }
}
