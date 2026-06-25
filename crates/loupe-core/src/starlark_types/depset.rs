//! `depset` — Bazel's lazy nested set.
//!
//! From DESIGN.md non-negotiables:
//! > `depset` is a lazy tree, not a flat collection. Direct items +
//! > transitive children + order. Flatten only on iteration. Dedupe per
//! > the order's traversal. Implement all four orders (default/postorder,
//! > preorder, topological).
//!
//! From DETAILS.md:
//! > `depset` exists because of memory. Early Bazel passed plain lists
//! > through providers; large C++ projects exploded. Depsets share
//! > structure transitively. The lazy tree shape is the entire point —
//! > flattening eagerly negates the win.
//!
//! ## Architecture
//!
//! Two parallel implementations:
//!
//! - **Rust core** — `DepsetTree<T>` + [`flatten`] / `preorder` /
//!   `postorder` / `topological`. Generic over `T: Hash + Eq`, lives
//!   in pure Rust so the algorithm has thorough unit tests without
//!   dragging Starlark in. Used internally by Loupe types that need
//!   a depset shape over a known Rust type.
//! - **Starlark binding** — [`DepsetGen`] (`Depset<'v>` +
//!   `FrozenDepset`) + the `depset(...)` builtin (registered via
//!   [`register_depset`]). Holds `Vec<Value<'v>>` direct + `Vec<Value<'v>>`
//!   transitive children + order; flattens via the Starlark-side
//!   `preorder_collect` / `postorder_collect` mirroring the Rust core
//!   but using `Hashed<Value>` for dedupe so Starlark equality wins.
//!
//! The two share `DepsetOrder`, parsing, and the order-compatibility
//! check. They don't share traversal code because dedupe semantics
//! diverge: Rust uses `Hash + Eq`; Starlark uses Bazel value-equality
//! through `Hashed<Value<'v>>`.

use std::hash::Hash;
use std::sync::Arc;

use allocative::Allocative;
use anyhow::anyhow;
use starlark::any::ProvidesStaticType;
use starlark::collections::Hashed;
use starlark::environment::{GlobalsBuilder, Methods, MethodsBuilder, MethodsStatic};
use starlark::starlark_complex_values;
use starlark::starlark_module;
use starlark::values::list::UnpackList;
use starlark::values::{
    Freeze, FreezeResult, Freezer, FrozenValue, Heap, NoSerialize, StarlarkValue, Trace, Value,
    ValueLifetimeless, ValueLike, starlark_value,
};

/// Traversal order for `depset.to_list()`. Names match Bazel's
/// `depset(order=…)` keyword exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Allocative)]
pub enum DepsetOrder {
    /// Alias for `Postorder`. Bazel's default; what `depset(...)` uses
    /// when no `order` kwarg is supplied.
    Default,
    /// Each parent appears *before* any of its descendants (DFS pre).
    /// On duplicates the *first* occurrence wins.
    Preorder,
    /// Each parent appears *after* its descendants (DFS post). On
    /// duplicates the *last* occurrence wins; we achieve this by
    /// reversing both the traversal and the dedupe pass.
    Postorder,
    /// Each element appears after every element it depends on. Same
    /// underlying children-first traversal as `Postorder`, but dedupe
    /// is first-wins instead of last-wins so shared nodes float up to
    /// where they're first seen — i.e. before any of their dependents.
    Topological,
}

impl DepsetOrder {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "default" => Some(DepsetOrder::Default),
            "preorder" => Some(DepsetOrder::Preorder),
            "postorder" => Some(DepsetOrder::Postorder),
            "topological" => Some(DepsetOrder::Topological),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            DepsetOrder::Default => "default",
            DepsetOrder::Preorder => "preorder",
            DepsetOrder::Postorder => "postorder",
            DepsetOrder::Topological => "topological",
        }
    }

    /// Two depsets can only compose (via `transitive=[…]`) if their
    /// orders are compatible. Mirrors Bazel's `Order.isCompatible`: the
    /// orders are compatible iff they're equal or one of them is the
    /// `default` (`STABLE_ORDER`) — `default` interoperates with every
    /// order, while two distinct explicit orders (e.g. `preorder` +
    /// `topological`) never do.
    pub fn is_compatible_with(self, other: DepsetOrder) -> bool {
        self == other || self == DepsetOrder::Default || other == DepsetOrder::Default
    }
}

/// A node in the depset tree. Cheap to clone (shared via `Arc`) — the
/// whole reason the type exists is to share transitive structure
/// across many depsets without flattening.
#[derive(Debug, Clone, Allocative)]
pub struct DepsetTree<T>
where
    T: Allocative + Clone + 'static,
{
    direct: Arc<[T]>,
    transitive: Arc<[Arc<DepsetTree<T>>]>,
    order: DepsetOrder,
}

impl<T> DepsetTree<T>
where
    T: Allocative + Clone + 'static,
{
    pub fn new(
        direct: Vec<T>,
        transitive: Vec<Arc<DepsetTree<T>>>,
        order: DepsetOrder,
    ) -> Result<Self, DepsetError> {
        for child in &transitive {
            if !order.is_compatible_with(child.order) {
                return Err(DepsetError::IncompatibleOrders {
                    parent: order,
                    child: child.order,
                });
            }
        }
        Ok(DepsetTree {
            direct: direct.into(),
            transitive: transitive.into(),
            order,
        })
    }

    pub fn order(&self) -> DepsetOrder {
        self.order
    }

    pub fn direct(&self) -> &[T] {
        &self.direct
    }

    pub fn transitive(&self) -> &[Arc<DepsetTree<T>>] {
        &self.transitive
    }

    /// Flatten in this depset's order, deduplicated. **Only** call
    /// when you actually need the materialised sequence — DETAILS.md:
    /// "Eager depset flattening on access destroys the whole point of
    /// depsets."
    pub fn to_list(&self) -> Vec<T>
    where
        T: Hash + Eq,
    {
        flatten(self)
    }
}

/// Walk a depset in its declared order with the dedupe rule that
/// matches Bazel's behaviour.
fn flatten<T>(root: &DepsetTree<T>) -> Vec<T>
where
    T: Allocative + Clone + Hash + Eq + 'static,
{
    match root.order {
        DepsetOrder::Preorder => preorder(root),
        // `default` is an alias for `postorder`.
        DepsetOrder::Default | DepsetOrder::Postorder => postorder(root),
        DepsetOrder::Topological => topological(root),
    }
}

fn preorder<T>(root: &DepsetTree<T>) -> Vec<T>
where
    T: Clone + Hash + Eq + Allocative + 'static,
{
    // DFS, parent-first, left-to-right. First occurrence wins.
    // Explicit stack so we don't blow Rust's recursion stack on deep
    // depsets.
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut stack: Vec<&DepsetTree<T>> = vec![root];
    while let Some(node) = stack.pop() {
        for item in node.direct.iter() {
            if seen.insert(item.clone()) {
                out.push(item.clone());
            }
        }
        // Push children in reverse so left-to-right pop order matches
        // declaration order.
        for child in node.transitive.iter().rev() {
            stack.push(child);
        }
    }
    out
}

fn postorder<T>(root: &DepsetTree<T>) -> Vec<T>
where
    T: Clone + Hash + Eq + Allocative + 'static,
{
    // Children-first traversal. For last-wins dedupe, we collect every
    // occurrence in post-order and then dedupe by walking the list in
    // reverse with a first-wins set — equivalent.
    let mut full: Vec<T> = Vec::new();
    visit_postorder(root, &mut full);

    let mut seen = std::collections::HashSet::new();
    let mut rev_unique: Vec<T> = Vec::with_capacity(full.len());
    for item in full.into_iter().rev() {
        if seen.insert(item.clone()) {
            rev_unique.push(item);
        }
    }
    rev_unique.reverse();
    rev_unique
}

fn topological<T>(root: &DepsetTree<T>) -> Vec<T>
where
    T: Clone + Hash + Eq + Allocative + 'static,
{
    // Same children-first traversal as `postorder`, but first-wins
    // dedupe — shared nodes appear at their earliest position, ahead
    // of every place they're depended on. This is what gives the order
    // its "every element after its dependencies" guarantee.
    let mut full: Vec<T> = Vec::new();
    visit_postorder(root, &mut full);

    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<T> = Vec::with_capacity(full.len());
    for item in full {
        if seen.insert(item.clone()) {
            out.push(item);
        }
    }
    out
}

fn visit_postorder<T>(node: &DepsetTree<T>, out: &mut Vec<T>)
where
    T: Clone + Allocative + 'static,
{
    for child in node.transitive.iter() {
        visit_postorder(child, out);
    }
    for item in node.direct.iter() {
        out.push(item.clone());
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DepsetError {
    #[error(
        "incompatible depset orders: parent is `{}` but transitive child is `{}`",
        parent.as_str(),
        child.as_str()
    )]
    IncompatibleOrders {
        parent: DepsetOrder,
        child: DepsetOrder,
    },
}

/// Starlark `depset` value. Holds direct elements + child depsets +
/// order. Iteration / `to_list()` flatten the tree according to the
/// declared order with dedupe.
///
/// Element type isn't enforced (Bazel rejects mixing types like
/// `depset([file, "string"])`; v1 stubs that — caller can still do
/// nonsense and we'll dedupe by Starlark value-equality).
#[derive(Debug, Trace, Allocative, NoSerialize, ProvidesStaticType, derive_more::Display)]
#[repr(C)]
#[display("<depset {} direct, {} transitive>", direct.len(), transitive.len())]
pub struct DepsetGen<V: ValueLifetimeless> {
    pub direct: Vec<V>,
    /// Child depsets. Each `V` is a Starlark `depset` value (either
    /// `Depset<'v>` or `FrozenDepset`).
    pub transitive: Vec<V>,
    #[trace(static)]
    pub order: DepsetOrder,
}

pub type Depset<'v> = DepsetGen<Value<'v>>;
pub type FrozenDepset = DepsetGen<FrozenValue>;

starlark_complex_values!(Depset);

impl<'v> Freeze for Depset<'v> {
    type Frozen = FrozenDepset;
    fn freeze(self, freezer: &Freezer) -> FreezeResult<Self::Frozen> {
        Ok(DepsetGen {
            direct: self.direct.freeze(freezer)?,
            transitive: self.transitive.freeze(freezer)?,
            order: self.order,
        })
    }
}

#[starlark_value(type = "depset")]
impl<'v, V: ValueLike<'v>> StarlarkValue<'v> for DepsetGen<V>
where
    Self: ProvidesStaticType<'v>,
{
    fn iterate_collect(&self, _heap: Heap<'v>) -> starlark::Result<Vec<Value<'v>>> {
        flatten_starlark(self.order, self.direct_values(), self.transitive_values())
    }

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(depset_methods)
    }
}

impl<'v, V: ValueLike<'v>> DepsetGen<V> {
    fn direct_values(&self) -> Vec<Value<'v>> {
        self.direct.iter().map(|v| v.to_value()).collect()
    }

    fn transitive_values(&self) -> Vec<Value<'v>> {
        self.transitive.iter().map(|v| v.to_value()).collect()
    }
}

fn downcast_depset_parts<'v>(
    v: Value<'v>,
) -> anyhow::Result<(Vec<Value<'v>>, Vec<Value<'v>>, DepsetOrder)> {
    if let Some(d) = v.downcast_ref::<Depset<'v>>() {
        Ok((d.direct_values(), d.transitive_values(), d.order))
    } else if let Some(d) = v.downcast_ref::<FrozenDepset>() {
        Ok((d.direct_values(), d.transitive_values(), d.order))
    } else {
        Err(anyhow!(
            "internal: expected depset in transitive list, got {}",
            v.get_type()
        ))
    }
}

/// Flatten a depset `Value` into the materialised list of elements,
/// honoring the depset's own `order=` and deduping by Starlark
/// value-equality (`Hashed<Value>`).
///
/// Public so non-Starlark call sites that read depset-shaped fields
/// (`DefaultInfo.files`, runfiles depsets) can flatten through the
/// same code path the Starlark `to_list()` / iteration uses, rather
/// than rolling a bespoke (and inevitably order-blind) DFS.
///
/// Errors if `v` isn't a `Depset`/`FrozenDepset`. Callers that accept
/// both list and depset shapes should branch before calling this.
pub fn flatten_to_values<'v>(v: Value<'v>) -> starlark::Result<Vec<Value<'v>>> {
    let (direct, transitive, order) =
        downcast_depset_parts(v).map_err(starlark::Error::new_other)?;
    flatten_starlark(order, direct, transitive)
}

/// Flatten a Starlark depset (already split into direct + transitive
/// roots) according to `order`, dedupe by Starlark value equality.
/// Mirrors the `DepsetTree<T>` logic but operates on `Value<'v>` so it
/// works across the Value/FrozenValue boundary.
fn flatten_starlark<'v>(
    order: DepsetOrder,
    direct: Vec<Value<'v>>,
    transitive: Vec<Value<'v>>,
) -> starlark::Result<Vec<Value<'v>>> {
    match order {
        DepsetOrder::Preorder => preorder_collect(direct, transitive),
        DepsetOrder::Default | DepsetOrder::Postorder => {
            postorder_collect(direct, transitive, /* first_wins */ false)
        }
        DepsetOrder::Topological => {
            postorder_collect(direct, transitive, /* first_wins */ true)
        }
    }
}

fn preorder_collect<'v>(
    direct: Vec<Value<'v>>,
    transitive: Vec<Value<'v>>,
) -> starlark::Result<Vec<Value<'v>>> {
    let mut seen: std::collections::HashSet<Hashed<Value<'v>>> = std::collections::HashSet::new();
    let mut out: Vec<Value<'v>> = Vec::new();
    let mut stack: Vec<(Vec<Value<'v>>, Vec<Value<'v>>)> = vec![(direct, transitive)];
    while let Some((d, t)) = stack.pop() {
        for v in d {
            let h = v.get_hashed()?;
            if seen.insert(h) {
                out.push(v);
            }
        }
        // Reverse so left-to-right pop order matches declaration order.
        for child in t.into_iter().rev() {
            let (cd, ct, _) = downcast_depset_parts(child)?;
            stack.push((cd, ct));
        }
    }
    Ok(out)
}

fn postorder_collect<'v>(
    direct: Vec<Value<'v>>,
    transitive: Vec<Value<'v>>,
    first_wins: bool,
) -> starlark::Result<Vec<Value<'v>>> {
    let mut full: Vec<Value<'v>> = Vec::new();
    visit_postorder_starlark(direct, transitive, &mut full)?;

    if first_wins {
        // `topological` — shared nodes float to first occurrence.
        let mut seen: std::collections::HashSet<Hashed<Value<'v>>> =
            std::collections::HashSet::new();
        let mut out: Vec<Value<'v>> = Vec::with_capacity(full.len());
        for v in full {
            let h = v.get_hashed()?;
            if seen.insert(h) {
                out.push(v);
            }
        }
        Ok(out)
    } else {
        // `postorder` / `default` — last-wins. Walk in reverse, then flip.
        let mut seen: std::collections::HashSet<Hashed<Value<'v>>> =
            std::collections::HashSet::new();
        let mut rev_unique: Vec<Value<'v>> = Vec::with_capacity(full.len());
        for v in full.into_iter().rev() {
            let h = v.get_hashed()?;
            if seen.insert(h) {
                rev_unique.push(v);
            }
        }
        rev_unique.reverse();
        Ok(rev_unique)
    }
}

fn visit_postorder_starlark<'v>(
    direct: Vec<Value<'v>>,
    transitive: Vec<Value<'v>>,
    out: &mut Vec<Value<'v>>,
) -> starlark::Result<()> {
    for child in transitive {
        let (cd, ct, _) = downcast_depset_parts(child)?;
        visit_postorder_starlark(cd, ct, out)?;
    }
    out.extend(direct);
    Ok(())
}

#[starlark_module]
fn depset_methods(builder: &mut MethodsBuilder) {
    /// `depset.to_list()` — flatten according to the depset's order
    /// with dedupe. Equivalent to iterating the depset.
    fn to_list<'v>(#[starlark(this)] this: Value<'v>) -> starlark::Result<Vec<Value<'v>>> {
        let (d, t, order) = downcast_depset_parts(this)?;
        flatten_starlark(order, d, t)
    }
}

#[starlark_module]
fn depset_globals(builder: &mut GlobalsBuilder) {
    /// `depset(direct=[], transitive=[], order="default")`.
    ///
    /// Bazel's depset constructor. `direct` is this node's elements;
    /// `transitive` is a list of child depsets. `order` is one of
    /// `"default"`, `"preorder"`, `"postorder"`, `"topological"`.
    /// Children whose order is incompatible with the parent's are
    /// rejected — matching Bazel's runtime check.
    fn depset<'v>(
        // `direct` accepts both positional (`depset([files])`) and
        // named (`depset(direct=[files])`) — both forms are common in
        // Bazel ruleset code.
        direct: Option<UnpackList<Value<'v>>>,
        #[starlark(require = named)] transitive: Option<UnpackList<Value<'v>>>,
        #[starlark(require = named, default = "default")] order: &str,
        heap: Heap<'v>,
    ) -> starlark::Result<Value<'v>> {
        let order = DepsetOrder::parse(order).ok_or_else(|| {
            starlark::Error::new_other(anyhow!(
                "depset(order=...): expected one of `default`, `preorder`, `postorder`, `topological`, got `{order}`"
            ))
        })?;
        let direct: Vec<Value<'v>> = direct.map(|l| l.items).unwrap_or_default();
        let transitive: Vec<Value<'v>> = transitive.map(|l| l.items).unwrap_or_default();
        // Validate transitive children: each must be a depset, and its
        // order must be compatible with the parent's.
        for t in &transitive {
            let (_, _, child_order) = downcast_depset_parts(*t)
                .map_err(|e| starlark::Error::new_other(anyhow!("depset(transitive=...): {e}")))?;
            if !order.is_compatible_with(child_order) {
                return Err(starlark::Error::new_other(anyhow!(
                    "incompatible depset orders: parent is `{}` but transitive child is `{}`",
                    order.as_str(),
                    child_order.as_str()
                )));
            }
        }
        Ok(heap.alloc_complex(Depset {
            direct,
            transitive,
            order,
        }))
    }
}

/// Wires the `depset` builtin into a `Globals` set. Both `register_for_bzl`
/// and `register_for_build` call this so the constructor is reachable
/// from `.bzl` and BUILD files.
pub fn register_depset(builder: &mut GlobalsBuilder) {
    depset_globals(builder);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(items: &[&str], order: DepsetOrder) -> Arc<DepsetTree<String>> {
        Arc::new(
            DepsetTree::new(items.iter().map(|s| s.to_string()).collect(), vec![], order).unwrap(),
        )
    }

    fn node(
        direct: &[&str],
        transitive: Vec<Arc<DepsetTree<String>>>,
        order: DepsetOrder,
    ) -> DepsetTree<String> {
        DepsetTree::new(
            direct.iter().map(|s| s.to_string()).collect(),
            transitive,
            order,
        )
        .unwrap()
    }

    // --- order-name parsing ---------------------------------------------

    #[test]
    fn parses_all_order_names() {
        for (name, expected) in [
            ("default", DepsetOrder::Default),
            ("preorder", DepsetOrder::Preorder),
            ("postorder", DepsetOrder::Postorder),
            ("topological", DepsetOrder::Topological),
        ] {
            assert_eq!(DepsetOrder::parse(name), Some(expected));
            assert_eq!(expected.as_str(), name);
        }
        assert_eq!(DepsetOrder::parse("nope"), None);
    }

    // --- compatibility --------------------------------------------------

    #[test]
    fn explicit_order_composes_only_with_itself_or_default() {
        // Bazel's `Order.isCompatible`: two orders compose iff they're
        // equal or one of them is the `default` (STABLE_ORDER). A
        // `default` parent/child interoperates with everything (notably
        // `default` + `preorder`); two *distinct explicit* orders never do.
        use DepsetOrder::*;
        for explicit in [Preorder, Postorder, Topological] {
            // equal → compatible; either side default → compatible.
            assert!(explicit.is_compatible_with(explicit));
            assert!(explicit.is_compatible_with(Default));
            assert!(Default.is_compatible_with(explicit));
            // distinct explicit orders → incompatible (both directions).
            for other in [Preorder, Postorder, Topological] {
                if other != explicit {
                    assert!(!explicit.is_compatible_with(other));
                    assert!(!other.is_compatible_with(explicit));
                }
            }
        }
    }

    #[test]
    fn default_composes_with_every_order() {
        use DepsetOrder::*;
        for other in [Default, Preorder, Postorder, Topological] {
            assert!(Default.is_compatible_with(other));
            assert!(other.is_compatible_with(Default));
        }
    }

    #[test]
    fn rejects_incompatible_transitive() {
        let preorder_child = leaf(&["x"], DepsetOrder::Preorder);
        let err = DepsetTree::<String>::new(
            vec!["a".into()],
            vec![preorder_child],
            DepsetOrder::Postorder,
        )
        .unwrap_err();
        assert!(matches!(err, DepsetError::IncompatibleOrders { .. }));
    }

    // --- preorder semantics --------------------------------------------

    #[test]
    fn preorder_visits_parent_before_children() {
        // Tree:
        //   root [a, b]
        //   ├── child1 [c, d]
        //   └── child2 [e]
        // Expected preorder: a, b, c, d, e
        let c1 = leaf(&["c", "d"], DepsetOrder::Preorder);
        let c2 = leaf(&["e"], DepsetOrder::Preorder);
        let root = node(&["a", "b"], vec![c1, c2], DepsetOrder::Preorder);
        assert_eq!(root.to_list(), vec!["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn preorder_dedupe_keeps_first_occurrence() {
        // Shared "x" appears twice via different children; first wins.
        // Tree:
        //   root [r]
        //   ├── c1 [a, x]
        //   └── c2 [b, x]
        // Expected: r, a, x, b   (the second x is dropped)
        let c1 = leaf(&["a", "x"], DepsetOrder::Preorder);
        let c2 = leaf(&["b", "x"], DepsetOrder::Preorder);
        let root = node(&["r"], vec![c1, c2], DepsetOrder::Preorder);
        assert_eq!(root.to_list(), vec!["r", "a", "x", "b"]);
    }

    // --- postorder / default semantics ---------------------------------

    #[test]
    fn postorder_visits_children_before_parent() {
        // Same tree as the preorder case.
        // Expected postorder: c, d, e, a, b
        let c1 = leaf(&["c", "d"], DepsetOrder::Postorder);
        let c2 = leaf(&["e"], DepsetOrder::Postorder);
        let root = node(&["a", "b"], vec![c1, c2], DepsetOrder::Postorder);
        assert_eq!(root.to_list(), vec!["c", "d", "e", "a", "b"]);
    }

    #[test]
    fn postorder_dedupe_keeps_last_occurrence() {
        // Bazel's postorder dedupes such that each item ends up at its
        // *last* position in the unfiltered traversal. Key gotcha called
        // out by DETAILS.md.
        //
        // Tree (postorder traversal: c1.direct, c2.direct, root.direct):
        //   root [a, x]
        //   ├── c1 [x, b]
        //   └── c2 [c]
        // Unfiltered order:  x, b, c, a, x
        // Last-wins dedupe:        b, c, a, x
        let c1 = leaf(&["x", "b"], DepsetOrder::Postorder);
        let c2 = leaf(&["c"], DepsetOrder::Postorder);
        let root = node(&["a", "x"], vec![c1, c2], DepsetOrder::Postorder);
        assert_eq!(root.to_list(), vec!["b", "c", "a", "x"]);
    }

    #[test]
    fn default_aliases_postorder() {
        // The same dedupe rules and direction.
        let c1 = leaf(&["x", "b"], DepsetOrder::Default);
        let c2 = leaf(&["c"], DepsetOrder::Default);
        let root = node(&["a", "x"], vec![c1, c2], DepsetOrder::Default);
        assert_eq!(root.to_list(), vec!["b", "c", "a", "x"]);
    }

    // --- topological --------------------------------------------------

    #[test]
    fn topological_emits_dependencies_before_dependents() {
        // Diamond:
        //   root [r]
        //   ├── left  [l] -> shared
        //   └── right [r2] -> shared
        //   shared [s]
        //
        // Topological/postorder both need: s before l, s before r2,
        // and l/r2 before r.
        let shared = leaf(&["s"], DepsetOrder::Topological);
        let left = Arc::new(node(&["l"], vec![shared.clone()], DepsetOrder::Topological));
        let right = Arc::new(node(
            &["r2"],
            vec![shared.clone()],
            DepsetOrder::Topological,
        ));
        let root = node(&["r"], vec![left, right], DepsetOrder::Topological);
        let out = root.to_list();
        // Order isn't fully constrained — only the dependency edges are.
        let pos = |x: &str| out.iter().position(|y| y == x).unwrap();
        assert!(pos("s") < pos("l"));
        assert!(pos("s") < pos("r2"));
        assert!(pos("l") < pos("r"));
        assert!(pos("r2") < pos("r"));
    }

    // --- laziness ------------------------------------------------------

    #[test]
    fn construction_does_not_flatten() {
        // The whole point: building a tree with millions of items in
        // transitive children must not allocate the materialised list
        // until the user asks for one. This test is a smoke check —
        // construction is O(direct + |transitive|), not the closure.
        let mut t: Arc<DepsetTree<String>> =
            Arc::new(node(&["leaf"], vec![], DepsetOrder::Postorder));
        for _ in 0..1000 {
            // Each layer adds a single direct item plus the previous
            // (shared) tree. Eager flattening would be quadratic.
            t = Arc::new(
                DepsetTree::new(
                    vec!["item".to_string()],
                    vec![t.clone()],
                    DepsetOrder::Postorder,
                )
                .unwrap(),
            );
        }
        // The depth is 1001 but the *unique* set is just 2 elements:
        // "leaf" and "item". Verify the flatten dedupes correctly and
        // doesn't blow up.
        let out = t.to_list();
        assert_eq!(out, vec!["leaf", "item"]);
    }

    // --- structural sharing -------------------------------------------

    #[test]
    fn arc_clone_is_cheap() {
        let big = leaf(&["x", "y", "z"], DepsetOrder::Default);
        let p1 = node(&["a"], vec![big.clone()], DepsetOrder::Default);
        let p2 = node(&["b"], vec![big.clone()], DepsetOrder::Default);
        // The shared `big` should appear as the same Arc in both
        // parents. (`Arc::ptr_eq` compares the pointer, not the
        // contents — exactly what we want for "structure sharing".)
        assert!(Arc::ptr_eq(&p1.transitive[0], &p2.transitive[0]));
    }
}
