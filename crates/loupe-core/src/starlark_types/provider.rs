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
use starlark::starlark_simple_value;
use starlark::values::{
    Freeze, FreezeResult, Freezer, FrozenValue, Heap, NoSerialize, StarlarkValue, Trace, Value,
    ValueLifetimeless, ValueLike, starlark_value,
};

use crate::label::Label;

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

/// What `provider()` returns. Frozen-value identity is the in-process
/// type identity; the optional [`ProviderId`] gives a stable identity
/// usable in DICE values.
#[derive(Debug, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("<provider{}>", display_name(name))]
pub struct ProviderType {
    /// Optional human-readable name. Lazily set by `BzlLoadKey` to the
    /// symbol the provider was bound to.
    pub name: String,
    /// `Some(set)` enforces a closed schema; `None` is "any field".
    pub fields: Option<Vec<String>>,
    pub doc: String,
    /// Set lazily by post-`.bzl`-freeze identity stamping. Consumers
    /// that need persistent identity should look here.
    #[allocative(skip)]
    pub id: OnceLock<ProviderId>,
}

fn display_name(name: &str) -> String {
    if name.is_empty() {
        String::new()
    } else {
        format!(" {name}")
    }
}

starlark_simple_value!(ProviderType);

#[starlark_value(type = "provider")]
impl<'v> StarlarkValue<'v> for ProviderType {
    fn invoke(
        &self,
        me: Value<'v>,
        args: &Arguments<'v, '_>,
        eval: &mut Evaluator<'v, '_, '_>,
    ) -> starlark::Result<Value<'v>> {
        let names = args.names_map().map_err(|e| {
            starlark::Error::new_other(anyhow::anyhow!("provider call kwargs: {e}"))
        })?;
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
        // Capture the provider type as a frozen value so instances can
        // refer back to it. `me` is whatever was looked up to invoke
        // this — for a frozen `ProviderType` (which it always is once
        // the `.bzl` is frozen) it's already a frozen pointer.
        let type_ptr: FrozenValue = me.unpack_frozen().ok_or_else(|| {
            starlark::Error::new_other(anyhow::anyhow!(
                "internal: ProviderType::invoke called on non-frozen value"
            ))
        })?;
        let mut fields = SmallMap::new();
        for (k, v) in names.iter() {
            fields.insert(k.as_str().to_string(), *v);
        }
        Ok(eval.heap().alloc(ProviderInstanceGen {
            type_ptr,
            type_name: self.name.clone(),
            fields,
        }))
    }
}

/// Result of `MyProvider(field=value)`. Generic over `V` so Starlark
/// values inside fields participate in heap freezing.
#[derive(Debug, Trace, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[repr(C)]
#[display("<{type_name} provider instance>")]
pub struct ProviderInstanceGen<V: ValueLifetimeless> {
    /// Frozen pointer to the `ProviderType` that constructed this
    /// instance. Survives freeze unchanged (`FrozenValue: 'static`).
    #[trace(static)]
    pub type_ptr: FrozenValue,
    /// Human-readable type name, populated lazily once `BzlLoadKey`
    /// stamps the `ProviderType.name`. Captured at invoke time for
    /// display only.
    #[trace(static)]
    pub type_name: String,
    /// Field values — typed, not stringified. Phase 5 reads
    /// `ctx.attr.x.greeting` straight off this map after analysis.
    /// Keys are plain `String`s so the same `SmallMap` shape works for
    /// both `Value<'v>` and `FrozenValue` field types.
    pub fields: SmallMap<String, V>,
}

pub type ProviderInstance<'v> = ProviderInstanceGen<Value<'v>>;
pub type FrozenProviderInstance = ProviderInstanceGen<FrozenValue>;

starlark_complex_values!(ProviderInstance);

impl<'v> Freeze for ProviderInstance<'v> {
    type Frozen = FrozenProviderInstance;
    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        let mut frozen_fields: SmallMap<String, FrozenValue> =
            SmallMap::with_capacity(self.fields.len());
        for (k, v) in self.fields {
            frozen_fields.insert(k, v.freeze(freezer)?);
        }
        Ok(ProviderInstanceGen {
            type_ptr: self.type_ptr,
            type_name: self.type_name,
            fields: frozen_fields,
        })
    }
}

#[starlark_value(type = "provider_instance")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for ProviderInstanceGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn get_attr(&self, attribute: &str, _heap: Heap<'v>) -> Option<Value<'v>> {
        self.fields.get(attribute).map(|v| v.to_value())
    }

    fn dir_attr(&self) -> Vec<String> {
        self.fields.keys().cloned().collect()
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

#[starlark_module]
pub fn provider_globals(b: &mut GlobalsBuilder) {
    fn provider<'v>(
        #[starlark(require = named, default = "")] doc: &str,
        #[starlark(require = named)] fields: Option<Value<'v>>,
    ) -> anyhow::Result<ProviderType> {
        let fields_vec = match fields {
            Some(v) => unpack_fields(v)?,
            None => None,
        };
        Ok(ProviderType {
            name: String::new(),
            fields: fields_vec,
            doc: doc.to_string(),
            id: OnceLock::new(),
        })
    }
}

/// `provider(fields=...)` accepts `None` (any field), a list of
/// strings, or a dict mapping field name → doc string.
fn unpack_fields<'v>(v: Value<'v>) -> anyhow::Result<Option<Vec<String>>> {
    if v.is_none() {
        return Ok(None);
    }
    if let Some(list) = starlark::values::list::ListRef::from_value(v) {
        let mut out = Vec::with_capacity(list.len());
        for item in list.iter() {
            let s = item
                .unpack_str()
                .ok_or_else(|| anyhow::anyhow!("provider(fields=[...]) entries must be strings"))?;
            out.push(s.to_string());
        }
        return Ok(Some(out));
    }
    if let Some(dict) = starlark::values::dict::DictRef::from_value(v) {
        let mut out = Vec::with_capacity(dict.len());
        for (k, _) in dict.iter() {
            let s = k
                .unpack_str()
                .ok_or_else(|| anyhow::anyhow!("provider(fields={{...}}) keys must be strings"))?;
            out.push(s.to_string());
        }
        return Ok(Some(out));
    }
    Err(anyhow::anyhow!(
        "provider(fields=…) expects None, a list of strings, or a dict; got {}",
        v.get_type()
    ))
}
