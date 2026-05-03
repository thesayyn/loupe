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
//! The traversal algorithm is in pure Rust (`DepsetTree<T>` plus
//! [`flatten`]) so it can be unit-tested without dragging Starlark in.
//! The Starlark binding is a thin wrapper (`Depset` + `depset(...)`
//! builtin) that holds `DepsetTree<FrozenValue>`. The wrapper lives in
//! a follow-up commit on this branch — Phase 4's deliverable says
//! "thorough unit tests including the gotchas DETAILS.md calls out",
//! so the algorithmic core is what matters first.

use std::hash::Hash;
use std::sync::Arc;

use allocative::Allocative;

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
    /// orders are compatible. Bazel rejects mixing `preorder` with
    /// anything else; `default`/`postorder`/`topological` interoperate.
    pub fn is_compatible_with(self, other: DepsetOrder) -> bool {
        match (self, other) {
            (DepsetOrder::Preorder, DepsetOrder::Preorder) => true,
            (DepsetOrder::Preorder, _) | (_, DepsetOrder::Preorder) => false,
            _ => true,
        }
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
    fn preorder_only_composes_with_preorder() {
        // Bazel rejects mixing preorder with anything else — check both
        // directions of `is_compatible_with`.
        for other in [
            DepsetOrder::Default,
            DepsetOrder::Postorder,
            DepsetOrder::Topological,
        ] {
            assert!(!DepsetOrder::Preorder.is_compatible_with(other));
            assert!(!other.is_compatible_with(DepsetOrder::Preorder));
        }
        assert!(DepsetOrder::Preorder.is_compatible_with(DepsetOrder::Preorder));
    }

    #[test]
    fn default_postorder_topological_freely_compose() {
        for a in [
            DepsetOrder::Default,
            DepsetOrder::Postorder,
            DepsetOrder::Topological,
        ] {
            for b in [
                DepsetOrder::Default,
                DepsetOrder::Postorder,
                DepsetOrder::Topological,
            ] {
                assert!(a.is_compatible_with(b));
            }
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
