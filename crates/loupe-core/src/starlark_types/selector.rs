//! `select()` and the `SelectorList` value it returns.
//!
//! From DESIGN.md non-negotiables:
//! > `select()` returns a value, not a resolved result. Resolution
//! > happens during analysis, not loading. Selectors must survive across
//! > the loading→analysis boundary with their full branch dict intact
//! > (future multi-config will re-resolve the same selector multiple
//! > times).
//!
//! From DETAILS.md:
//! > `select()` is a value because attrs need to be configurable
//! > per-config. v1 has one config but the value type still defers —
//! > same selector might one day resolve multiple ways.
//!
//! ## Shape
//!
//! In Bazel, `select(...)` returns a `SelectorValue`; concatenating one
//! to a plain list, or to another selector, builds a `SelectorList` —
//! a sequence of (concrete-piece | selector-piece) parts. We model
//! the same: every result is a `SelectorList`, where a single
//! `select(...)` call is a list with exactly one selector piece.
//!
//! Concrete pieces are stored as `AttrValue` (already round-trips
//! through our package shape). Branch values inside a selector are
//! also `AttrValue` so selectors can be persisted, compared, and
//! re-resolved by the analysis phase without touching Starlark.
//!
//! Phase 4 implements:
//! - `select(dict)` builtin
//! - `SelectorList` value with `+` (concat) for both `list + select`
//!   and `select + select`
//! - Real resolution lives in Phase 5.

use std::collections::BTreeMap;

use allocative::Allocative;
use anyhow::anyhow;
use starlark::any::ProvidesStaticType;
use starlark::environment::GlobalsBuilder;
use starlark::starlark_module;
use starlark::starlark_simple_value;
use starlark::values::ValueLike;
use starlark::values::{NoSerialize, StarlarkValue, Value, starlark_value};

use crate::package::AttrValue;

/// One alternative branch in a `select()`.
///
/// Bazel's `//conditions:default` lives in here as a regular key — it
/// gets distinguished by string match at resolution time, not by the
/// type system. Phase 5 will introduce a typed sentinel.
#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub struct Selector {
    pub branches: BTreeMap<String, AttrValue>,
    /// `no_match_error="…"` from `select(…, no_match_error=…)`.
    pub no_match_error: Option<String>,
}

/// What `select()` returns and what `[…] + select(…)` continues to be.
/// A list of pieces — each piece is either a literal `AttrValue` or a
/// `Selector`. Resolution concatenates resolved pieces in order.
#[derive(
    Debug, Clone, PartialEq, Eq, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display,
)]
#[display("<select with {} piece(s)>", pieces.len())]
pub struct SelectorList {
    pub pieces: Vec<SelectorPiece>,
}

#[derive(Debug, Clone, PartialEq, Eq, Allocative)]
pub enum SelectorPiece {
    Concrete(AttrValue),
    Selector(Selector),
}

impl SelectorList {
    pub fn from_selector(s: Selector) -> Self {
        SelectorList {
            pieces: vec![SelectorPiece::Selector(s)],
        }
    }

    pub fn from_concrete(v: AttrValue) -> Self {
        SelectorList {
            pieces: vec![SelectorPiece::Concrete(v)],
        }
    }

    pub fn extend_with(self, other: SelectorList) -> Self {
        let mut pieces = self.pieces;
        pieces.extend(other.pieces);
        SelectorList { pieces }
    }
}

starlark_simple_value!(SelectorList);

#[starlark_value(type = "select")]
impl<'v> StarlarkValue<'v> for SelectorList {
    // Resolution is deferred. Phase 5 owns it.
}

#[starlark_module]
pub fn selector_globals(b: &mut GlobalsBuilder) {
    /// `select({"//conditions:default": [...], ...}, no_match_error="...")`.
    fn select<'v>(
        #[starlark(require = pos)] branches: Value<'v>,
        #[starlark(require = named, default = "")] no_match_error: &str,
    ) -> anyhow::Result<SelectorList> {
        let dict = starlark::values::dict::DictRef::from_value(branches)
            .ok_or_else(|| anyhow!("select() expects a dict; got {}", branches.get_type()))?;
        let mut map: BTreeMap<String, AttrValue> = BTreeMap::new();
        for (k, v) in dict.iter() {
            // Bazel accepts both label strings and `Label(...)` values
            // as `select` keys; rules_js uses the latter for its
            // `@aspect_bazel_lib//lib:enable_runfiles` config setting.
            let key = if let Some(s) = k.unpack_str() {
                s.to_string()
            } else if let Some(lv) = k.downcast_ref::<crate::starlark_types::label::LabelValue>() {
                lv.raw.clone()
            } else {
                return Err(anyhow!(
                    "select() keys must be label strings or Label values, got `{}`",
                    k.get_type()
                ));
            };
            map.insert(key, crate::build_eval::unpack_attr(v));
        }
        let selector = Selector {
            branches: map,
            no_match_error: if no_match_error.is_empty() {
                None
            } else {
                Some(no_match_error.to_string())
            },
        };
        Ok(SelectorList::from_selector(selector))
    }
}

/// Try to coerce a Starlark value into a `SelectorList` — concrete
/// values become a single-piece `Concrete(...)` list. Used by the
/// `+`-style helpers and by future attr-typing code that wants to
/// uniformly treat "this attr might or might not be a selector".
pub fn coerce_to_selector_list<'v>(v: Value<'v>) -> Option<SelectorList> {
    if let Some(s) = SelectorList::from_value(v) {
        return Some(s.clone());
    }
    Some(SelectorList::from_concrete(crate::build_eval::unpack_attr(
        v,
    )))
}

/// Concatenation helper used by `+` between selector lists or
/// `list + select(...)`. Phase 4 doesn't wire `+` into the StarlarkValue
/// `add` op (Bazel does); a follow-up commit on this branch can do it
/// once attr-typing actually consumes selectors.
pub fn concat(a: SelectorList, b: SelectorList) -> SelectorList {
    a.extend_with(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selector_with(branches: &[(&str, AttrValue)]) -> Selector {
        Selector {
            branches: branches
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect(),
            no_match_error: None,
        }
    }

    #[test]
    fn single_select_is_one_piece_list() {
        let s = selector_with(&[
            ("//conditions:default", AttrValue::String("x".into())),
            ("//cpu:k8", AttrValue::String("y".into())),
        ]);
        let list = SelectorList::from_selector(s.clone());
        assert_eq!(list.pieces.len(), 1);
        match &list.pieces[0] {
            SelectorPiece::Selector(got) => assert_eq!(got, &s),
            SelectorPiece::Concrete(_) => panic!("expected selector piece"),
        }
    }

    #[test]
    fn concat_preserves_piece_order() {
        // `[a, b] + select({...}) + [c]` resolves to a 3-piece list.
        let a = SelectorList::from_concrete(AttrValue::List(vec![
            AttrValue::String("a".into()),
            AttrValue::String("b".into()),
        ]));
        let b = SelectorList::from_selector(selector_with(&[(
            "//cpu:k8",
            AttrValue::String("middle".into()),
        )]));
        let c = SelectorList::from_concrete(AttrValue::List(vec![AttrValue::String("c".into())]));
        let combined = concat(concat(a, b), c);
        assert_eq!(combined.pieces.len(), 3);
        assert!(matches!(combined.pieces[0], SelectorPiece::Concrete(_)));
        assert!(matches!(combined.pieces[1], SelectorPiece::Selector(_)));
        assert!(matches!(combined.pieces[2], SelectorPiece::Concrete(_)));
    }

    #[test]
    fn selector_value_round_trips_through_attr_value_branches() {
        // Branch values are `AttrValue`s, so a selector can survive the
        // loading→analysis boundary without holding any Starlark
        // pointers (DESIGN.md invariant).
        let s = selector_with(&[
            ("//conditions:default", AttrValue::List(vec![])),
            (
                "//cpu:k8",
                AttrValue::List(vec![AttrValue::String("optimised".into())]),
            ),
        ]);
        let list = SelectorList::from_selector(s.clone());
        // No Starlark `Value<'v>` anywhere on the wire.
        let _: AttrValue = match &list.pieces[0] {
            SelectorPiece::Selector(s) => s.branches["//cpu:k8"].clone(),
            _ => unreachable!(),
        };
    }

    #[test]
    fn concrete_value_becomes_a_concrete_piece() {
        let s = SelectorList::from_concrete(AttrValue::String("hi".into()));
        assert_eq!(
            s.pieces,
            vec![SelectorPiece::Concrete(AttrValue::String("hi".into()))]
        );
    }
}
