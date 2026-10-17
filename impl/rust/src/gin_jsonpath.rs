//! GIN query trees and the `jsonb_ops` jsonpath term extraction (spec/design/gin.md §2.2).
//!
//! A GIN query is a monotone AND/OR tree over terms ([`GinTree`]): a row is a candidate when the
//! tree is TRUE with each term read as "the row carries this term". The ordinary strategies are one
//! level deep (mode ALL is an AND of terms, mode ANY an OR); the jsonpath strategies `@?` / `@@`
//! extract a deeper tree from the path, PostgreSQL's `extract_jsp_query` (`jsonb_gin.c`) for
//! `jsonb_ops`, term for term. The gather evaluates the tree (`access_path.rs`) and the planner
//! prices it from term statistics (`statistics.rs`); both read the same normalized form.

use crate::json::{JsonNode, gin_jsonb_key_term, gin_jsonb_scalar_term};
use crate::jsonpath::{CmpOp, JsonPath, PathBody, PathExpr, Pred, Primary, Step};

/// A normalized GIN query tree (gin.md §2.2). An `And` / `Or` node holds its distinct leaf terms in
/// ascending byte order and its non-leaf children in extraction order; nested nodes of the same kind
/// are spliced into their parent, and a node with one child is that child. Every node holds at least
/// one leaf or child.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum GinTree {
    Term(Vec<u8>),
    And(Vec<Vec<u8>>, Vec<GinTree>),
    Or(Vec<Vec<u8>>, Vec<GinTree>),
}

impl GinTree {
    /// A strategy's flat term set as a tree: mode ANY an `Or`, mode ALL an `And` (gin.md §2).
    /// `terms` is non-empty.
    pub(crate) fn from_terms(any: bool, terms: Vec<Vec<u8>>) -> GinTree {
        let nodes = terms.into_iter().map(GinTree::Term).collect();
        if any {
            GinTree::normalize_or(nodes)
        } else {
            GinTree::normalize_and(nodes)
        }
    }

    /// Every leaf term mapped through `f` (a multi-column index's column tag, gin.md §4.4). The
    /// mapping preserves byte order and distinctness, so the tree stays normalized.
    pub(crate) fn map_terms(self, f: &impl Fn(Vec<u8>) -> Vec<u8>) -> GinTree {
        match self {
            GinTree::Term(t) => GinTree::Term(f(t)),
            GinTree::And(leaves, rest) => GinTree::And(
                leaves.into_iter().map(f).collect(),
                rest.into_iter().map(|c| c.map_terms(f)).collect(),
            ),
            GinTree::Or(leaves, rest) => GinTree::Or(
                leaves.into_iter().map(f).collect(),
                rest.into_iter().map(|c| c.map_terms(f)).collect(),
            ),
        }
    }

    fn normalize_and(nodes: Vec<GinTree>) -> GinTree {
        GinTree::normalize(true, nodes)
    }

    fn normalize_or(nodes: Vec<GinTree>) -> GinTree {
        GinTree::normalize(false, nodes)
    }

    fn normalize(and: bool, nodes: Vec<GinTree>) -> GinTree {
        let mut leaves = Vec::new();
        let mut rest = Vec::new();
        for node in nodes {
            match (and, node) {
                (_, GinTree::Term(t)) => leaves.push(t),
                (true, GinTree::And(l, r)) | (false, GinTree::Or(l, r)) => {
                    leaves.extend(l);
                    rest.extend(r);
                }
                (_, other) => rest.push(other),
            }
        }
        leaves.sort_unstable();
        leaves.dedup();
        match (leaves.len(), rest.len()) {
            (1, 0) => GinTree::Term(leaves.pop().expect("one leaf")),
            (0, 1) => rest.pop().expect("one child"),
            _ if and => GinTree::And(leaves, rest),
            _ => GinTree::Or(leaves, rest),
        }
    }
}

/// One accessor of the path being walked (PostgreSQL's `JsonPathGinPathItem`): a `.key` carries its
/// KEY term; the wildcards and subscripts carry none.
#[derive(Clone)]
enum Item {
    Key(Vec<u8>),
    AnyKey,
    AnyArray,
    IndexArray,
    Any,
}

/// An extracted node before normalization (PostgreSQL's `JsonPathGinNode`).
enum Node {
    Entry(Vec<u8>),
    And(Vec<Node>),
    Or(Vec<Node>),
}

impl Node {
    fn into_tree(self) -> GinTree {
        match self {
            Node::Entry(t) => GinTree::Term(t),
            Node::And(children) => {
                GinTree::normalize_and(children.into_iter().map(Node::into_tree).collect())
            }
            Node::Or(children) => {
                GinTree::normalize_or(children.into_iter().map(Node::into_tree).collect())
            }
        }
    }
}

/// The `jsonb_ops` query tree of a jsonpath under `@?` (`exists`) or `@@` (gin.md §2.2), or `None`
/// when nothing is extractable — the full-scan fallback (PostgreSQL's `GIN_SEARCH_MODE_ALL`).
pub(crate) fn jsonpath_gin_tree(path: &JsonPath, exists: bool) -> Option<GinTree> {
    let lax = !path.strict;
    let node = match (&path.body, exists) {
        // `@?` is `EXISTS(path)`: a path body extracts its filters; a predicate body is no path.
        (PathBody::Path(expr), true) => extract_path_expr(lax, &[], expr, None),
        (PathBody::Predicate(_), true) => None,
        // `@@` reads a predicate body; a path body is not a boolean expression.
        (PathBody::Predicate(pred), false) => extract_bool(lax, &[], pred, false),
        (PathBody::Path(_), false) => None,
    }?;
    Some(node.into_tree())
}

/// PostgreSQL's `extract_jsp_bool_expr`. Under `not`, an `AND` with one unextractable side keeps
/// the other only when the node is still an `AND` after De Morgan: PostgreSQL keeps it whenever the
/// operator is written `&&`, which under `!` drops rows (`!(!A && B)` is not `A`), so jed returns
/// `None` there instead — a deliberate, sound divergence (gin.md §9).
fn extract_bool(lax: bool, path: &[Item], pred: &Pred, not: bool) -> Option<Node> {
    match pred {
        Pred::And(l, r) | Pred::Or(l, r) => {
            let is_and = matches!(pred, Pred::And(..));
            let and = not ^ is_and;
            let larg = extract_bool(lax, path, l, not);
            let rarg = extract_bool(lax, path, r, not);
            match (larg, rarg) {
                (Some(a), Some(b)) => Some(if and {
                    Node::And(vec![a, b])
                } else {
                    Node::Or(vec![a, b])
                }),
                (a, b) if and => a.or(b),
                _ => None,
            }
        }
        Pred::Not(arg) => extract_bool(lax, path, arg, !not),
        Pred::Exists(expr) if !not => extract_path_expr(lax, path, expr, None),
        Pred::Compare(l, CmpOp::Eq, r) if !not => {
            let (scalar, other) = match (scalar_literal(l), scalar_literal(r)) {
                (Some(s), _) => (s, r),
                (None, Some(s)) => (s, l),
                (None, None) => return None,
            };
            extract_path_expr(lax, path, other, Some(scalar))
        }
        _ => None,
    }
}

/// A comparison operand PostgreSQL's `jspIsScalar` accepts: a step-less null / boolean / number /
/// string literal (a negated number literal folds into one at parse).
fn scalar_literal(e: &PathExpr) -> Option<&JsonNode> {
    match &e.primary {
        Primary::Lit(node) if e.steps.is_empty() => Some(node),
        _ => None,
    }
}

/// PostgreSQL's `extract_jsp_path_expr`: the nodes of `expr` AND-ed, or `None` when there are none.
fn extract_path_expr(
    lax: bool,
    path: &[Item],
    expr: &PathExpr,
    scalar: Option<&JsonNode>,
) -> Option<Node> {
    let mut nodes = extract_path_expr_nodes(lax, path, expr, scalar);
    match nodes.len() {
        0 => None,
        1 => nodes.pop(),
        _ => Some(Node::And(nodes)),
    }
}

/// PostgreSQL's `extract_jsp_path_expr_nodes` + `jsonb_ops__extract_nodes`: walk the accessors,
/// extracting each filter; an accessor the opclass cannot follow (an item method, a variable, a
/// literal, arithmetic) ends the walk with only the filter nodes found so far. A complete walk with a
/// `scalar` adds a KEY entry per `.key` accessor and the scalar's entry.
fn extract_path_expr_nodes(
    lax: bool,
    outer: &[Item],
    expr: &PathExpr,
    scalar: Option<&JsonNode>,
) -> Vec<Node> {
    let mut nodes = Vec::new();
    let mut path: Vec<Item> = match expr.primary {
        Primary::Root => Vec::new(),
        Primary::Current => outer.to_vec(),
        _ => return nodes,
    };
    for step in &expr.steps {
        let item = match step {
            Step::Filter(pred) => {
                if let Some(node) = extract_bool(lax, &path, pred, false) {
                    nodes.push(node);
                }
                continue;
            }
            Step::Member(key) => Item::Key(gin_jsonb_key_term(key)),
            Step::WildcardMember => Item::AnyKey,
            Step::WildcardElement => Item::AnyArray,
            Step::Subscripts(_) => Item::IndexArray,
            Step::Any { .. } => Item::Any,
            Step::Method(_) => return nodes,
        };
        path.push(item);
    }
    let Some(scalar) = scalar else {
        return nodes;
    };
    // Keys from the last accessor back to the root, as PostgreSQL appends them.
    for item in path.iter().rev() {
        if let Item::Key(term) = item {
            nodes.push(Node::Entry(term.clone()));
        }
    }
    let node = match scalar {
        JsonNode::String(_) => {
            // jsonb_ops indexes a string array element as a KEY, so the scalar may be a key, a
            // value, or either (lax mode unwraps arrays; `.**` reaches both).
            let key = if lax {
                None
            } else {
                match path.last() {
                    None => Some(false),
                    Some(Item::AnyArray | Item::IndexArray) => Some(true),
                    Some(Item::Any) => None,
                    Some(_) => Some(false),
                }
            };
            match key {
                Some(as_key) => Node::Entry(gin_jsonb_scalar_term(scalar, as_key)),
                None => Node::Or(vec![
                    Node::Entry(gin_jsonb_scalar_term(scalar, true)),
                    Node::Entry(gin_jsonb_scalar_term(scalar, false)),
                ]),
            }
        }
        _ => Node::Entry(gin_jsonb_scalar_term(scalar, false)),
    };
    nodes.push(node);
    nodes
}
