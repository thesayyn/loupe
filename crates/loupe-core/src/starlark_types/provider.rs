//! `provider()` and provider-instance values.
//!
//! Phase 3 defines the surface; analysis (Phase 5) is where instances
//! get constructed in anger via rule impls. The non-negotiable
//! invariant from DESIGN.md — *provider types are singletons by call
//! site* — is satisfied by allocating each `provider()` call as a
//! distinct frozen value in the `.bzl`'s heap; identity is then
//! `FrozenValue` pointer equality, and consumers loading from the same
//! `.bzl` see the same `FrozenValue` because `BzlLoadKey` is cached
//! by DICE.

use std::collections::BTreeMap;

use allocative::Allocative;
use starlark::any::ProvidesStaticType;
use starlark::collections::SmallMap;
use starlark::environment::GlobalsBuilder;
use starlark::eval::{Arguments, Evaluator};
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::{NoSerialize, StarlarkValue, Value, starlark_value};

/// What `provider()` returns. Frozen-value identity *is* the provider's
/// type identity — see DESIGN.md.
#[derive(Debug, Clone, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("<provider{}>", display_name(name))]
pub struct ProviderType {
    /// Optional human-readable name. Bazel's `provider()` accepts no
    /// `name` kwarg, so this is empty unless Phase 5 retro-populates
    /// it from the `.bzl` binding name.
    pub name: String,
    /// `Some(set)` enforces a closed schema; `None` is "any field".
    pub fields: Option<Vec<String>>,
    pub doc: String,
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
        _me: Value<'v>,
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
        let mut fields = SmallMap::new();
        for (k, v) in names.iter() {
            fields.insert(k.as_str().to_string(), *v);
        }
        Ok(eval.heap().alloc(ProviderInstance {
            type_name: self.name.clone(),
            fields: fields
                .into_iter()
                .map(|(k, v)| (k, v.to_string()))
                .collect(),
        }))
    }
}

/// What `MyProvider(field=value)` returns. Phase 3 stringifies field
/// values for display only — Phase 5 will introduce typed extraction
/// once the configured-target graph is being built.
#[derive(Debug, Clone, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[display("<{type_name} provider instance>")]
pub struct ProviderInstance {
    pub type_name: String,
    pub fields: BTreeMap<String, String>,
}

starlark_simple_value!(ProviderInstance);

#[starlark_value(type = "provider_instance")]
impl<'v> StarlarkValue<'v> for ProviderInstance {}

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
