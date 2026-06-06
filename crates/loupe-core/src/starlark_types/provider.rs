//! `provider()`, the provider-type value, and the provider-instance
//! values rule impls return.
//!
//! ## Identity
//!
//! From DESIGN.md:
//! > Provider types are singletons by call site.
//!
//! Phase 3 satisfied this with frozen-value pointer equality: each
//! `provider()` call allocates a distinct frozen value in the `.bzl`'s
//! heap; consumers that load from the same `.bzl` see the same
//! `FrozenValue` because `BzlLoadKey` is cached by DICE.
//!
//! Phase 5 keeps in-process pointer identity but additionally carries
//! a stable `(defining_bzl, exported_symbol)` `ProviderId` so analysis
//! can record providers in DICE values without dragging Starlark heap
//! pointers across recompute boundaries. (Cross-run cache persistence
//! later: a future `ProviderId`-keyed serde will replace the pointer
//! eq.) The ID is filled in lazily — `provider()` itself doesn't know
//! which symbol it'll be bound to in the `.bzl`, so Phase 5 walks the
//! freshly-frozen module's exports and stamps each `ProviderType`
//! before returning from `BzlLoadKey`.
//!
//! ## Instances
//!
//! `MyProvider(field=...)` returns a [`ProviderInstance`]. Phase 5
//! preserves the field values as Starlark `Value`s during analysis and
//! freezes them as part of the analysis-output module — the resulting
//! `FrozenProviderInstance` has typed fields again, so `ctx.attr.x`
//! reads of provider fields work without a stringification round-trip.

use std::sync::OnceLock;

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_complex_values;
use starlark::starlark_module;
use starlark::values::{
    Freeze, FreezeResult, Freezer, FrozenValue, Heap, NoSerialize, StarlarkValue, Trace, Value,
    ValueLifetimeless, ValueLike, starlark_value,
};

use loupe_labels::Label;

/// Stable identity for a provider type.
///
/// Set lazily after the defining `.bzl` is frozen — at `provider()`
/// call time we don't yet know which symbol the type will be assigned
/// to, so `BzlLoadKey` walks the frozen exports and stamps each
/// `ProviderType` with a `ProviderId`.
///
/// `defining_bzl` is `None` for *native* providers (Bazel built-ins
/// like `DefaultInfo`, `LicenseInfo`, etc.) and `Some(label)` for
/// user-defined providers from `.bzl` files. Display matches Bazel's
/// `cquery providers(target)` output: native = bare `"DefaultInfo"`;
/// user = `"<bzl>%<symbol>"` (e.g. `"//:defs.bzl%HelloInfo"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Allocative)]
pub struct ProviderId {
    /// `None` for native providers; `Some` for user-defined ones.
    pub defining_bzl: Option<Label>,
    /// Top-level symbol name the provider is bound to, e.g.
    /// `"HelloInfo"` or `"DefaultInfo"`.
    pub symbol: String,
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.defining_bzl {
            None => f.write_str(&self.symbol),
            Some(bzl) => write!(f, "{bzl}%{}", self.symbol),
        }
    }
}

/// What `provider()` returns. Generic over `V` so module-local
/// function values passed to `computed_fields=` get frozen alongside
/// the rest of the `.bzl` (same shape as `RuleClassGen<V>`).
#[derive(Debug, Trace, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[repr(C)]
#[display("<provider{}>", display_name(name))]
pub struct ProviderTypeGen<V: ValueLifetimeless> {
    /// Optional human-readable name. Lazily set by `BzlLoadKey` to the
    /// symbol the provider was bound to.
    #[trace(static)]
    pub name: String,
    /// `Some(set)` enforces a closed schema; `None` is "any field".
    #[trace(static)]
    pub fields: Option<Vec<String>>,
    #[trace(static)]
    pub doc: String,
    /// `computed_fields = {name: fn}` from `provider(...)`. On instance
    /// attribute access for `name`, Loupe returns a thunk that calls
    /// `fn(instance_proxy)` and forwards the user's args to the
    /// resulting value. Empty when the provider wasn't declared with
    /// computed_fields. Generic over `V` so functions passed at
    /// `.bzl` eval time freeze into `FrozenValue` with their
    /// defining module.
    pub computed_fields: SmallMap<String, V>,
    /// `init = fn` from `provider(...)`. When set, `MyInfo(**kwargs)`
    /// runs `fn(**kwargs)` first; the function returns a dict whose
    /// entries become the instance's fields. Vendored upstream uses
    /// this to coerce defaults — e.g. `runfiles`'s init turns a
    /// missing `root_symlinks` into `depset()`. `None` when the
    /// provider was declared without `init=`.
    pub init: Option<V>,
    /// Set lazily by post-`.bzl`-freeze identity stamping. Consumers
    /// that need persistent identity should look here.
    #[allocative(skip)]
    #[trace(static)]
    pub id: OnceLock<ProviderId>,
}

pub type ProviderType<'v> = ProviderTypeGen<Value<'v>>;
pub type FrozenProviderType = ProviderTypeGen<FrozenValue>;

fn display_name(name: &str) -> String {
    if name.is_empty() {
        String::new()
    } else {
        format!(" {name}")
    }
}

starlark_complex_values!(ProviderType);

impl<'v> Freeze for ProviderType<'v> {
    type Frozen = FrozenProviderType;
    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        let mut frozen_cf: SmallMap<String, FrozenValue> =
            SmallMap::with_capacity(self.computed_fields.len());
        for (k, v) in self.computed_fields {
            frozen_cf.insert(k, v.freeze(freezer)?);
        }
        Ok(ProviderTypeGen {
            name: self.name,
            fields: self.fields,
            doc: self.doc,
            computed_fields: frozen_cf,
            init: match self.init {
                Some(v) => Some(v.freeze(freezer)?),
                None => None,
            },
            id: self.id,
        })
    }
}

#[starlark_value(type = "provider")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ProviderTypeGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn invoke(
        &self,
        me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let names = args.names_map().map_err(|e| {
            starlark::Error::new_other(anyhow::anyhow!("provider call kwargs: {e}"))
        })?;
        // `provider(init = fn)` — if set, forward the user's
        // positional + named args verbatim to `fn(...)`; the returned
        // dict's entries become the instance's field values. Both
        // shapes matter: the vendored `runfiles` init takes `**kwargs`,
        // while `_template_variable_info_init(variables)` takes a
        // single positional dict.
        let fields_map: SmallMap<String, Value<'v>> = if let Some(init_fn) = self.init.as_ref() {
            let positional: Vec<Value<'v>> = args
                .positions(eval.heap())
                .map_err(|e| starlark::Error::new_other(anyhow::anyhow!("{e}")))?
                .collect();
            let named: Vec<(&str, Value<'v>)> =
                names.iter().map(|(k, v)| (k.as_str(), *v)).collect();
            let returned = eval
                .eval_function(init_fn.to_value(), &positional, &named)
                .map_err(|e| starlark::Error::new_other(anyhow::anyhow!("provider init: {e}")))?;
            let dict = starlark::values::dict::DictRef::from_value(returned).ok_or_else(|| {
                starlark::Error::new_other(anyhow::anyhow!(
                    "provider init must return a dict, got {}",
                    returned.get_type()
                ))
            })?;
            let mut out = SmallMap::with_capacity(dict.len());
            for (k, v) in dict.iter() {
                let key = k.unpack_str().ok_or_else(|| {
                    starlark::Error::new_other(anyhow::anyhow!(
                        "provider init returned a dict with non-string key: {k}"
                    ))
                })?;
                out.insert(key.to_string(), v);
            }
            out
        } else {
            if let Some(allowed) = &self.fields {
                for k in names.keys() {
                    if !allowed.iter().any(|a| a == k.as_str()) {
                        return Err(starlark::Error::new_other(anyhow::anyhow!(
                            "provider {} has no field `{}`",
                            if self.name.is_empty() {
                                "(anonymous)"
                            } else {
                                &self.name
                            },
                            k.as_str()
                        )));
                    }
                }
            }
            let mut fields = SmallMap::new();
            for (k, v) in names.iter() {
                fields.insert(k.as_str().to_string(), *v);
            }
            fields
        };
        // Capture the provider type pointer for the instance. `me` is
        // whatever was looked up to invoke this — either a frozen
        // `ProviderType` (the common case, after the defining `.bzl`
        // is frozen) or a live one (when a `.bzl` invokes its own
        // provider at module-eval time). `ProviderInstanceGen.type_ptr`
        // is generic over heap kind so both cases land on the same
        // shape; freeze later converts the live form to FrozenValue.
        let type_ptr: Value<'v> = me;
        // Allocate the instance with user fields only and an empty
        // `computed` cell — `cf_fn`s below will see this same Value
        // as `r`, so any `return r` they do gives the user back this
        // very instance.
        let instance_value = eval.heap().alloc(ProviderInstanceGen {
            type_ptr,
            type_name: self.name.clone(),
            fields: fields_map,
            computed: std::sync::OnceLock::new(),
        });
        // `provider(computed_fields = {name: fn})` —
        // eager evaluation. Now that `instance_value` exists, call
        // each `cf_fn(instance_value)` and stash the result in
        // `computed`. Because both `cf_fn`'s captured closures and
        // subsequent `get_attr` lookups read through the same
        // `ProviderInstanceGen`, the "return r" idiom in
        // `_runfiles_merge_all` yields a value whose `.merge` /
        // `.merge_all` still resolve.
        if !self.computed_fields.is_empty() {
            let inst_ref = ProviderInstance::from_value(instance_value)
                .and_then(|either| match either {
                    either::Either::Left(r) => Some(r),
                    either::Either::Right(_) => None,
                })
                .ok_or_else(|| {
                    starlark::Error::new_other(anyhow::anyhow!(
                        "internal: just-allocated provider instance failed to downcast"
                    ))
                })?;
            let mut results: SmallMap<String, Value<'v>> =
                SmallMap::with_capacity(self.computed_fields.len());
            for (cf_name, cf_fn) in self.computed_fields.iter() {
                let computed = eval
                    .eval_function(cf_fn.to_value(), &[instance_value], &[])
                    .map_err(|e| {
                        starlark::Error::new_other(anyhow::anyhow!(
                            "provider computed_field `{cf_name}`: {e}"
                        ))
                    })?;
                results.insert(cf_name.clone(), computed);
            }
            // `OnceLock::set` returns Err only if already set — which
            // can't happen here because we just allocated.
            let _ = inst_ref.computed.set(results);
        }
        Ok(instance_value)
    }
}

/// Result of `MyProvider(field=value)`. Generic over `V` so Starlark
/// values inside fields participate in heap freezing.
///
/// `fields` is immutable post-alloc and carries the user-supplied
/// (or `init`-derived) attributes. `computed` is filled in *after*
/// allocation with the results of each `computed_fields` function —
/// where the closure captured by a
/// computed_field can do `return r` and the caller sees the
/// fully-populated instance (`inst.merge` etc. still resolve). The
/// fill-in step in `ProviderType::invoke` runs each `cf_fn(self)`
/// against the just-allocated instance's Value, then calls
/// `computed.set(results)` on the same heap-allocated cell — so
/// every closure captured during the call sees the same final
/// instance.
#[derive(Debug, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[repr(C)]
#[display("<{type_name} provider instance>")]
pub struct ProviderInstanceGen<V: ValueLifetimeless> {
    /// Pointer to the `ProviderType` that constructed this instance.
    /// Generic over the heap kind so providers can be invoked during
    /// their own module's eval (when the type lives on the live heap)
    /// — `ProviderInstance<'v>` carries `Value<'v>`, the frozen form
    /// carries `FrozenValue`. The freeze impl converts.
    pub type_ptr: V,
    /// Human-readable type name, populated lazily once `BzlLoadKey`
    /// stamps the `ProviderType.name`. Captured at invoke time for
    /// display only.
    pub type_name: String,
    /// User-supplied / `init`-returned field values. Final at alloc
    /// time. Keys are plain `String`s so the same `SmallMap` shape
    /// works for both `Value<'v>` and `FrozenValue`.
    pub fields: SmallMap<String, V>,
    /// `provider(computed_fields={...})` results, populated after
    /// allocation so the closures returned by `cf_fn(self)` capture
    /// the instance they're being installed on. Empty for providers
    /// without `computed_fields=`.
    #[allocative(skip)]
    pub computed: std::sync::OnceLock<SmallMap<String, V>>,
}

pub type ProviderInstance<'v> = ProviderInstanceGen<Value<'v>>;
pub type FrozenProviderInstance = ProviderInstanceGen<FrozenValue>;

// Manual `Trace` impls because `OnceLock<SmallMap<String, V>>` isn't
// covered by the derive's automatic walk. For the live form
// (`ProviderInstance<'v>`), `fields` plus `computed` (if set) need to
// be traced so the GC sees the contained Values. The frozen form
// holds only `FrozenValue`s (`'static`) and needs no traversal.
unsafe impl<'v> Trace<'v> for ProviderInstance<'v> {
    fn trace(&mut self, tracer: &starlark::values::Tracer<'v>) {
        self.fields.trace(tracer);
        // SAFETY: `get_mut` is safe — we have `&mut self`. The
        // returned `&mut SmallMap` is traced just like `fields`.
        if let Some(computed) = self.computed.get_mut() {
            computed.trace(tracer);
        }
    }
}
unsafe impl<'v> Trace<'v> for FrozenProviderInstance {
    fn trace(&mut self, _tracer: &starlark::values::Tracer<'v>) {
        // `FrozenValue` is `'static`; nothing to walk.
    }
}

starlark_complex_values!(ProviderInstance);

impl<'v> Freeze for ProviderInstance<'v> {
    type Frozen = FrozenProviderInstance;
    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        let mut frozen_fields: SmallMap<String, FrozenValue> =
            SmallMap::with_capacity(self.fields.len());
        for (k, v) in self.fields {
            frozen_fields.insert(k, v.freeze(freezer)?);
        }
        let frozen_computed: std::sync::OnceLock<SmallMap<String, FrozenValue>> =
            std::sync::OnceLock::new();
        if let Some(map) = self.computed.into_inner() {
            let mut frozen_map: SmallMap<String, FrozenValue> = SmallMap::with_capacity(map.len());
            for (k, v) in map {
                frozen_map.insert(k, v.freeze(freezer)?);
            }
            let _ = frozen_computed.set(frozen_map);
        }
        Ok(ProviderInstanceGen {
            type_ptr: self.type_ptr.freeze(freezer)?,
            type_name: self.type_name,
            fields: frozen_fields,
            computed: frozen_computed,
        })
    }
}

#[starlark_value(type = "provider_instance")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ProviderInstanceGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_attr(&self, attribute: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        // User-supplied / `init`-derived fields take precedence.
        if let Some(v) = self.fields.get(attribute) {
            return Some(v.to_value());
        }
        // Fall back to `provider(computed_fields=…)` results filled
        // in post-allocation by `ProviderType::invoke`. None for
        // providers without `computed_fields=`.
        if let Some(computed) = self.computed.get()
            && let Some(v) = computed.get(attribute)
        {
            return Some(v.to_value());
        }
        None
    }

    fn dir_attr(&self) -> Vec<String> {
        let mut out: Vec<String> = self.fields.keys().cloned().collect();
        if let Some(computed) = self.computed.get() {
            out.extend(computed.keys().cloned());
        }
        out
    }

    /// Provider instances hash and compare by *identity*, matching
    /// Bazel's `StarlarkInfo`. A provider's fields routinely include
    /// depsets and dicts (rules_js's `NpmPackageStoreInfo`), which
    /// aren't structurally hashable — so structural hashing is
    /// impossible. Identity is what's wanted anyway: a `depset` of
    /// provider instances dedups when the *same* instance is reached
    /// via multiple transitive paths (e.g.
    /// `npm_package_store_infos.to_list()`), while distinct instances
    /// stay distinct. Frozen instances live at a stable heap address,
    /// so the pointer is a sound identity key.
    fn write_hash(
        &self,
        hasher: &mut starlark::collections::StarlarkHasher,
    ) -> starlark::Result<()> {
        std::hash::Hash::hash(&(self as *const Self as usize), hasher);
        Ok(())
    }

    fn equals(&self, other: Value<'v>) -> starlark::Result<bool> {
        Ok(other
            .downcast_ref::<Self>()
            .map(|o| std::ptr::eq(self, o))
            .unwrap_or(false))
    }
}

/// Borrow the `(name, value)` pairs of either form. Useful for
/// analysis code that processes providers irrespective of whether the
/// calling Starlark heap was already frozen.
pub fn provider_instance_fields<'a, 'v, V: ValueLike<'v>>(
    inst: &'a ProviderInstanceGen<V>,
) -> Vec<(&'a str, Value<'v>)> {
    inst.fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.to_value()))
        .collect()
}

/// Register the native `FragmentInfo` provider type as a global.
///
/// Bazel fragments (cpp/apple/objc/…) are modelled as a dep carrying a
/// `FragmentInfo`; rules read `ctx.fragments`/`ctx.configuration` off
/// it. It's a generic any-fields provider with native identity
/// (`{defining_bzl: None, symbol: "FragmentInfo"}`) — defined here in
/// Rust rather than as a `.bzl` `provider()`. The embedded
/// `bazel_tools/fragments/fragment_info.bzl` re-exports this global so
/// the vendored fragment `.bzl` `load(...)`s resolve to it, and
/// `ctx`'s native fragment reads use the same identity.
pub fn fragment_info_globals(builder: &mut GlobalsBuilder) {
    let id = OnceLock::new();
    let _ = id.set(ProviderId {
        defining_bzl: None,
        symbol: "FragmentInfo".to_string(),
    });
    let pt: FrozenProviderType = ProviderTypeGen {
        name: "FragmentInfo".to_string(),
        fields: None,
        doc: String::new(),
        computed_fields: SmallMap::new(),
        init: None,
        id,
    };
    builder.set("FragmentInfo", pt);
}

/// Allocate a (live) `FragmentInfo` provider-type value on `heap`,
/// carrying the same native identity (`{None, "FragmentInfo"}`) the
/// global [`fragment_info_globals`] registers. The native ctx uses this
/// as the `target[FragmentInfo]` lookup key — `Target::at` matches by
/// `ProviderId`, so it resolves the `FragmentInfo` instances the
/// vendored fragment `.bzl` constructed via the global, without a
/// `.bzl` fetch.
pub fn alloc_fragment_info_type<'v>(heap: Heap<'v>) -> Value<'v> {
    let id = OnceLock::new();
    let _ = id.set(ProviderId {
        defining_bzl: None,
        symbol: "FragmentInfo".to_string(),
    });
    heap.alloc_complex(ProviderType {
        name: "FragmentInfo".to_string(),
        fields: None,
        doc: String::new(),
        computed_fields: SmallMap::new(),
        init: None,
        id,
    })
}

#[allow(clippy::too_many_arguments)]
#[starlark_module]
pub fn provider_globals(b: &mut GlobalsBuilder) {
    fn provider<'v>(
        // Bazel accepts `provider("doc string", fields=…)` with doc as
        // either the first positional arg or `doc=`.
        #[starlark(require = pos, default = "")] doc_pos: &str,
        #[starlark(require = named, default = "")] doc: &str,
        // Bazel's `fields` is a `Sequence` of names or a name→doc dict,
        // so a list, a tuple, or a dict are all valid. The typed param
        // lets starlark-rust do the unpacking and reject other shapes.
        #[starlark(require = named)] fields: Option<
            either::Either<
                starlark::values::list_or_tuple::UnpackListOrTuple<String>,
                starlark::values::dict::UnpackDictEntries<String, Value<'v>>,
            >,
        >,
        #[starlark(require = named)] init: Option<Value<'v>>,
        // Vendored upstream providers carry extra annotations:
        //   - `dict_like = True` (OutputGroupInfo) — provider can be
        //     accessed dict-style (`p["key"]`).
        //   - `computed_fields = {name: fn}` (runfiles) — derived
        //     fields computed from instance state.
        //   - `type_name = "..."` — display name override.
        //   - `needs = [...]` — capability hints.
        // v1 accepts and ignores; semantics aren't modelled.
        #[starlark(require = named)] dict_like: Option<Value<'v>>,
        #[starlark(require = named)] computed_fields: Option<Value<'v>>,
        #[starlark(require = named)] type_name: Option<Value<'v>>,
        #[starlark(require = named)] needs: Option<Value<'v>>,
        eval: &mut starlark::eval::Evaluator<'v, '_, '_>,
    ) -> anyhow::Result<Value<'v>> {
        let _ = (dict_like, type_name, needs);
        let fields_vec = fields.map(|f| match f {
            either::Either::Left(seq) => seq.items,
            either::Either::Right(dict) => dict.entries.into_iter().map(|(k, _)| k).collect(),
        });
        let computed_fields_map = match computed_fields {
            Some(v) => unpack_computed_fields(v)?,
            None => SmallMap::new(),
        };
        let doc_text = if !doc.is_empty() { doc } else { doc_pos };
        let pt: ProviderType<'v> = ProviderTypeGen {
            name: String::new(),
            fields: fields_vec.clone(),
            doc: doc_text.to_string(),
            computed_fields: computed_fields_map.clone(),
            init: init.filter(|v| !v.is_none()),
            id: OnceLock::new(),
        };
        let pt_value = eval.heap().alloc_complex(pt);
        // Bazel's `provider(init=fn)` returns a 2-tuple
        // `(provider_callable, raw_constructor)` so user code can
        // unpack as `MyInfo, _new_myinfo = provider(init=…)`. The
        // raw_constructor is the unchecked path that bypasses `init`.
        // Loupe doesn't model `init` (STUB), but vendored Bazel
        // builtins (e.g. `objc_info.bzl`) use this unpacking shape —
        // returning a tuple keeps them parsing/freezing. The second
        // element is a second `ProviderType` with the same shape;
        // it's never invoked at module-eval time, so functional
        // equivalence isn't required.
        if init.is_some() {
            // The "raw" constructor bypasses `init` — it's the second
            // tuple element. Same shape as `pt`, but no `init` set.
            let raw: ProviderType<'v> = ProviderTypeGen {
                name: String::new(),
                fields: fields_vec,
                doc: doc_text.to_string(),
                computed_fields: computed_fields_map,
                init: None,
                id: OnceLock::new(),
            };
            let raw_value = eval.heap().alloc_complex(raw);
            Ok(eval.heap().alloc((pt_value, raw_value)))
        } else {
            Ok(pt_value)
        }
    }
}

/// `provider(computed_fields = {"name": fn})` — parse the dict into a
/// `SmallMap<String, Value<'v>>`. Function values stay as `Value<'v>`
/// while the `.bzl` is mutable; they freeze into `FrozenValue` with
/// the rest of the module via `ProviderType<'v>`'s `Freeze` impl.
fn unpack_computed_fields<'v>(v: Value<'v>) -> anyhow::Result<SmallMap<String, Value<'v>>> {
    let mut out = SmallMap::new();
    let Some(dict) = starlark::values::dict::DictRef::from_value(v) else {
        return Err(anyhow::anyhow!(
            "provider(computed_fields=…) expects a dict, got {}",
            v.get_type()
        ));
    };
    for (k, val) in dict.iter() {
        let Some(name) = k.unpack_str() else {
            return Err(anyhow::anyhow!(
                "provider(computed_fields={{…}}) keys must be strings"
            ));
        };
        out.insert(name.to_string(), val);
    }
    Ok(out)
}
