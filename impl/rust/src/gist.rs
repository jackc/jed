//! GiST access method — the operation-deterministic R-tree (spec/design/gist.md).
//!
//! A GiST index covers **one or more columns** (gist.md §4/§7), each with its own opclass — the
//! type-specific part is the *only* part that differs. The opclasses this feature ships:
//!   * **`range_ops`** (GX1) — over a `range` column, accelerating overlap `&&` and containment
//!     `@>`. Its component bound is the row's exact range (leaf) / the covering union (interior),
//!     stored as the decodable `encode_range_body` value codec.
//!   * **scalar `=`** (GX2, the in-core `btree_gist` equivalent) — over a fixed-width keyable
//!     scalar column, accelerating `=`. Its component bound is `[min, max]` over the
//!     **order-preserving key encoding** (gist.md §6): the executor encodes a value to its key
//!     bytes, and the tree only ever *compares* those bytes — no value decode, no per-type
//!     comparator, no collation (text/bytea/decimal/interval are a deferred follow-on, gist.md §11).
//!
//! A **multi-column** GiST index (GX3, the backing structure of an `EXCLUDE` constraint, gist.md §7)
//! carries one component bound per indexed column — its tree bound is the *tuple* of per-column
//! bounds, compared lexicographically, unioned componentwise, and descended/rechecked by the
//! conjunction (descend iff EVERY column's component is consistent). A single-column index is the
//! degenerate one-component case, and its on-disk bytes are unchanged by this generalization (a
//! one-element tuple encodes to exactly the single component's bytes — the GX1/GX2 goldens hold).
//!
//! This module is the self-contained core — the in-memory R-tree (build / penalty / median split),
//! the on-disk node codec (the §4.1 byte layout, page types 5/6), and the consistent-descent
//! search. Catalog/format integration (`IndexKind::Gist`, the grammar, `format_version` 20/21, the
//! planner gather, the exclusion probe) is wired separately and reuses these primitives.
//!
//! Determinism (gist.md §3): every operation is a pure function of its inputs, so the identical
//! mutation sequence every core replays builds the byte-identical tree. Within a node, entries are
//! ordered canonically (`bound_total_cmp`, ties by storage-key / subtree-min-key), so a node's
//! bytes are a pure function of its entry set; pages are assigned in a canonical post-order walk.

use crate::catalog::ColType;
use crate::error::{EngineError, Result};
use crate::format::{encode_range_body, read_range_body};
use crate::range::{range_contains, range_overlaps, range_total_cmp, range_union};
use crate::sqlstate::SqlState;
use crate::types::{ScalarType, Type};
use crate::value::{RangeVal, Value};
use std::cmp::Ordering;

/// Maximum entries per GiST node (gist.md §4.1). A pinned cross-core constant: inserting an
/// (N+1)-th entry triggers a median `picksplit`. Small enough that a few rows exercise a
/// multi-level tree; every GX1/GX2 element bound fits a page well within this fan-out.
pub const GIST_FANOUT: usize = 4;

/// GiST page types (gist.md §4.1, format.md *Page header*).
pub const PAGE_GIST_LEAF: u8 = 5;
pub const PAGE_GIST_INTERIOR: u8 = 6;

/// The query operators the GiST opclasses serve. `range_ops` accelerates **`Overlaps`** (`&&`) and
/// **`Contains`** (`@>`) — the positional operators (`<<`/`>>`/`&<`/`&>`/`-|-`), `<@`, and the
/// empty-query edge cases stay full-scan (gist.md §5/§11). The scalar `=` opclass accelerates
/// **`Equal`** (`=`). For a multi-column probe (an `EXCLUDE` conjunction, gist.md §7) one strategy
/// is supplied per column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GistStrategy {
    /// `col && Q` — the range overlap operator.
    Overlaps,
    /// `col @> Q` — `col` contains the query range/element.
    Contains,
    /// `col = q` — scalar equality (the scalar `=` opclass, gist.md §6).
    Equal,
}

/// The operator class of one indexed column — the only type-specific part of a GiST index
/// (gist.md §2). `Range` is `range_ops` over a range column whose element subtype is `ScalarType`;
/// `Scalar` is the `=` opclass over a fixed-width keyable scalar column (whose bound is opaque key
/// bytes the executor produces, so the variant carries no type — the tree never encodes a value,
/// only compares bytes). A multi-column index threads one opclass per column.
#[derive(Clone, Copy, Debug)]
pub enum GistOpclass {
    /// `range_ops` — the component bound is the range value codec over this element subtype.
    Range(ScalarType),
    /// scalar `=` — the component bound is `[min, max]` over the order-preserving key encoding.
    Scalar,
}

/// One column's bounding key. `range_ops` carries the exact range (leaf) / covering union
/// (interior); the scalar `=` opclass carries `[min, max]` over the order-preserving KEY encoding
/// (byte-comparable, so ordering / union / descent are raw byte operations — gist.md §6). A leaf's
/// scalar component is the degenerate `[v, v]`. A tree bound is a `Vec<GistBoundKey>` — one
/// component per indexed column (length 1 for the GX1/GX2 single-column indexes).
#[derive(Clone, Debug, PartialEq, Eq)]
enum GistBoundKey {
    Range(RangeVal),
    Scalar { min: Vec<u8>, max: Vec<u8> },
}

/// One column's search operand: a range constant for `&&`/`@>`, or a scalar equality constant's
/// order-preserving KEY bytes for `=` (the executor encodes it; the tree only compares). A
/// multi-column probe supplies one per column.
pub enum GistQuery {
    Range(RangeVal),
    Scalar(Vec<u8>),
}

/// One column's leaf-bound input, the executor → tree hand-off when building a row's leaf key
/// (gist.md §4.1). The executor knows the value; the tree owns the byte layout. A `Range` carries
/// its element subtype (for `encode_range_body`); a `Scalar` carries the value's already-encoded
/// order-preserving KEY bytes (gist.rs never encodes a value).
pub enum GistLeafComp<'a> {
    Range(ScalarType, &'a RangeVal),
    Scalar(&'a [u8]),
}

/// The opclass for a GiST index column of type `ty` (gist.md §5/§6): `range_ops` for a range
/// column, the scalar `=` opclass otherwise. The CREATE INDEX / EXCLUDE gate guarantees a supported
/// column type, so a non-range column here is a fixed-width keyable scalar.
pub fn opclass_for(ty: &Type) -> GistOpclass {
    match ty.range_element() {
        Some(elem) => GistOpclass::Range(elem.scalar()),
        None => GistOpclass::Scalar,
    }
}

impl GistOpclass {
    /// Serialize one component bound to its self-delimiting bytes (no outer length prefix — the
    /// node codec adds the `bound_len` framing over the whole tuple, gist.md §4.1).
    fn encode_comp(&self, b: &GistBoundKey) -> Vec<u8> {
        match (self, b) {
            (GistOpclass::Range(s), GistBoundKey::Range(rv)) => {
                encode_range_body(&ColType::Scalar(*s), rv)
            }
            (GistOpclass::Scalar, GistBoundKey::Scalar { min, max }) => {
                // `[min, max]`, each a length-prefixed key blob — self-delimiting and width-agnostic
                // (so the deferred variable-width keyables slot in unchanged, gist.md §11).
                let mut out = Vec::with_capacity(4 + min.len() + max.len());
                out.extend_from_slice(&(min.len() as u16).to_be_bytes());
                out.extend_from_slice(min);
                out.extend_from_slice(&(max.len() as u16).to_be_bytes());
                out.extend_from_slice(max);
                out
            }
            _ => panic!("BUG: gist opclass / bound-key kind mismatch"),
        }
    }

    /// Read one self-delimiting component bound starting at `pos`, advancing it past the bound.
    fn read_comp(&self, buf: &[u8], pos: &mut usize) -> Result<GistBoundKey> {
        match self {
            GistOpclass::Range(s) => {
                let elem = ColType::Scalar(*s);
                match read_range_body(&elem, buf, pos)? {
                    Value::Range(rv) => Ok(GistBoundKey::Range(rv)),
                    _ => Err(corrupt("gist: bound is not a range")),
                }
            }
            GistOpclass::Scalar => {
                let mlen = rd_u16(buf, pos)? as usize;
                let min = rd_bytes(buf, pos, mlen)?;
                let xlen = rd_u16(buf, pos)? as usize;
                let max = rd_bytes(buf, pos, xlen)?;
                Ok(GistBoundKey::Scalar { min, max })
            }
        }
    }
}

/// Serialize a whole tuple bound (one component per opclass) — the components concatenated in
/// column order. For a single-column index this is exactly the one component's bytes (the GX1/GX2
/// goldens are unchanged by the multi-column generalization).
fn encode_bound(ops: &[GistOpclass], bound: &[GistBoundKey]) -> Vec<u8> {
    debug_assert_eq!(ops.len(), bound.len());
    let mut out = Vec::new();
    for (op, b) in ops.iter().zip(bound) {
        out.extend_from_slice(&op.encode_comp(b));
    }
    out
}

/// Read a whole tuple bound (one component per opclass) starting at `pos`.
fn read_bound(ops: &[GistOpclass], buf: &[u8], pos: &mut usize) -> Result<Vec<GistBoundKey>> {
    let mut comps = Vec::with_capacity(ops.len());
    for op in ops {
        comps.push(op.read_comp(buf, pos)?);
    }
    Ok(comps)
}

/// A leaf entry: the row's tuple bound plus its storage key.
#[derive(Clone, Debug)]
struct LeafEntry {
    bound: Vec<GistBoundKey>,
    skey: Vec<u8>,
}

/// An interior entry: the tuple bound covering a child subtree, plus the child node. Unlike the
/// ordered B-tree, an interior holds **one bound per child** (N bounds, N children), not N
/// separators between N+1 children.
#[derive(Clone, Debug)]
struct ChildEntry {
    bound: Vec<GistBoundKey>,
    node: Box<GistNode>,
}

/// A GiST tree node — a leaf of row entries or an interior of child entries (each carrying its
/// subtree's covering union as its bound).
#[derive(Clone, Debug)]
enum GistNode {
    Leaf(Vec<LeafEntry>),
    Interior(Vec<ChildEntry>),
}

/// An operation-deterministic GiST R-tree over one or more columns (each a range or scalar opclass).
#[derive(Clone, Debug)]
pub struct GistTree {
    root: GistNode,
    len: usize,
}

impl Default for GistTree {
    fn default() -> Self {
        Self::new()
    }
}

impl GistTree {
    /// An empty tree (an empty leaf root).
    pub fn new() -> Self {
        GistTree {
            root: GistNode::Leaf(Vec::new()),
            len: 0,
        }
    }

    /// The number of indexed rows.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Insert one row's `(tuple bound, storage key)` into the tree under `ops`.
    fn insert(&mut self, ops: &[GistOpclass], bound: Vec<GistBoundKey>, skey: Vec<u8>) {
        if let Some(sib) = insert_node(&mut self.root, ops, bound, skey) {
            // The root split: grow a new interior root over the old root (left) + the sibling.
            let left = std::mem::replace(&mut self.root, GistNode::Leaf(Vec::new()));
            let left_bound = node_union(&left);
            let mut children = vec![
                ChildEntry {
                    bound: left_bound,
                    node: Box::new(left),
                },
                sib,
            ];
            sort_children(&mut children);
            self.root = GistNode::Interior(children);
        }
        self.len += 1;
    }

    /// Consistent-descent search: every storage key whose row satisfies the per-column query under
    /// the matching per-column strategy (a conjunction — descend iff EVERY component is consistent;
    /// recheck the exact operators at the leaf). `query` and `strats` are one entry per indexed
    /// column. Returns `(storage keys, nodes_visited, interior_visited)` — `nodes_visited`
    /// (interior + leaf) is the `page_read` charge, `interior_visited` the `gist_descent` charge
    /// (spec/design/gist.md §9).
    pub fn search(
        &self,
        query: &[GistQuery],
        strats: &[GistStrategy],
    ) -> (Vec<Vec<u8>>, usize, usize) {
        let mut out = Vec::new();
        let mut nodes = 0usize;
        let mut interior = 0usize;
        search_node(
            &self.root,
            query,
            strats,
            &mut out,
            &mut nodes,
            &mut interior,
        );
        (out, nodes, interior)
    }
}

/// The canonical total order over one column's component bound (gist.md §3): `range_total_cmp` for
/// ranges; the `[min, max]` key bytes lexicographically for scalars (the order-preserving key
/// encoding makes raw byte order reproduce value order).
fn comp_total_cmp(a: &GistBoundKey, b: &GistBoundKey) -> Ordering {
    match (a, b) {
        (GistBoundKey::Range(x), GistBoundKey::Range(y)) => range_total_cmp(x, y),
        (GistBoundKey::Scalar { min: a0, max: a1 }, GistBoundKey::Scalar { min: b0, max: b1 }) => {
            a0.cmp(b0).then_with(|| a1.cmp(b1))
        }
        _ => panic!("BUG: gist bound-key kind mismatch"),
    }
}

/// The canonical total order over a tuple bound: lexicographic over its components.
fn bound_total_cmp(a: &[GistBoundKey], b: &[GistBoundKey]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let c = comp_total_cmp(x, y);
        if c != Ordering::Equal {
            return c;
        }
    }
    Ordering::Equal
}

/// The covering union of two component bounds — the convex-hull `range_merge` for ranges; the
/// componentwise `[min(min), max(max)]` (byte-wise, the order-preserving key order) for scalars.
fn comp_union(a: &GistBoundKey, b: &GistBoundKey) -> GistBoundKey {
    match (a, b) {
        (GistBoundKey::Range(x), GistBoundKey::Range(y)) => {
            GistBoundKey::Range(range_union(x, y, false).expect("range_merge is total"))
        }
        (GistBoundKey::Scalar { min: a0, max: a1 }, GistBoundKey::Scalar { min: b0, max: b1 }) => {
            GistBoundKey::Scalar {
                min: a0.min(b0).clone(),
                max: a1.max(b1).clone(),
            }
        }
        _ => panic!("BUG: gist bound-key kind mismatch"),
    }
}

/// The componentwise covering union of two tuple bounds.
fn bound_union(a: &[GistBoundKey], b: &[GistBoundKey]) -> Vec<GistBoundKey> {
    a.iter().zip(b).map(|(x, y)| comp_union(x, y)).collect()
}

/// Choose the child to descend on insert: the one whose union, merged with the new entry, has the
/// lexicographically-smallest serialized bound bytes; ties keep the lower slot (gist.md §3
/// `penalty`).
fn choose_child(children: &[ChildEntry], ops: &[GistOpclass], bound: &[GistBoundKey]) -> usize {
    let mut best = 0usize;
    let mut best_key: Option<Vec<u8>> = None;
    for (i, c) in children.iter().enumerate() {
        let merged = bound_union(&c.bound, bound);
        let key = encode_bound(ops, &merged);
        let better = match &best_key {
            None => true,
            Some(bk) => &key < bk,
        };
        if better {
            best = i;
            best_key = Some(key);
        }
    }
    best
}

/// Insert into `node`, returning a new right sibling `ChildEntry` if the node split.
fn insert_node(
    node: &mut GistNode,
    ops: &[GistOpclass],
    bound: Vec<GistBoundKey>,
    skey: Vec<u8>,
) -> Option<ChildEntry> {
    match node {
        GistNode::Leaf(entries) => {
            entries.push(LeafEntry { bound, skey });
            sort_leaf(entries);
            split_if_overflow(node)
        }
        GistNode::Interior(children) => {
            let i = choose_child(children, ops, &bound);
            let split = insert_node(&mut children[i].node, ops, bound, skey);
            // The chosen child's union may have shrunk (after a split below) or grown; recompute it.
            children[i].bound = node_union(&children[i].node);
            if let Some(sib) = split {
                children.push(sib);
            }
            sort_children(children);
            split_if_overflow(node)
        }
    }
}

/// If `node` exceeds the fan-out, split it at the median (entries are already in canonical order)
/// and return the new right sibling; otherwise `None`.
fn split_if_overflow(node: &mut GistNode) -> Option<ChildEntry> {
    let over = match node {
        GistNode::Leaf(e) => e.len() > GIST_FANOUT,
        GistNode::Interior(c) => c.len() > GIST_FANOUT,
    };
    if !over {
        return None;
    }
    let right = match node {
        GistNode::Leaf(entries) => {
            let mid = entries.len().div_ceil(2);
            GistNode::Leaf(entries.split_off(mid))
        }
        GistNode::Interior(children) => {
            let mid = children.len().div_ceil(2);
            GistNode::Interior(children.split_off(mid))
        }
    };
    let bound = node_union(&right);
    Some(ChildEntry {
        bound,
        node: Box::new(right),
    })
}

/// The covering tuple union of a node's entries. The node must be non-empty (the empty tree's root
/// leaf is never unioned).
fn node_union(node: &GistNode) -> Vec<GistBoundKey> {
    let merge_all = |bounds: &mut dyn Iterator<Item = Vec<GistBoundKey>>| -> Vec<GistBoundKey> {
        let mut u = bounds.next().expect("node_union of an empty node");
        for b in bounds {
            u = bound_union(&u, &b);
        }
        u
    };
    match node {
        GistNode::Leaf(entries) => merge_all(&mut entries.iter().map(|e| e.bound.clone())),
        GistNode::Interior(children) => merge_all(&mut children.iter().map(|c| c.bound.clone())),
    }
}

/// The smallest storage key anywhere in the subtree — a deterministic, sibling-unique tiebreak for
/// canonical interior ordering (a row lives in exactly one leaf, so no two siblings share it).
fn subtree_min_skey(node: &GistNode) -> Vec<u8> {
    match node {
        GistNode::Leaf(entries) => entries
            .iter()
            .map(|e| e.skey.clone())
            .min()
            .expect("non-empty leaf"),
        GistNode::Interior(children) => children
            .iter()
            .map(|c| subtree_min_skey(&c.node))
            .min()
            .expect("non-empty interior"),
    }
}

fn sort_leaf(entries: &mut [LeafEntry]) {
    entries.sort_by(|a, b| bound_total_cmp(&a.bound, &b.bound).then_with(|| a.skey.cmp(&b.skey)));
}

fn sort_children(children: &mut [ChildEntry]) {
    children.sort_by(|a, b| {
        bound_total_cmp(&a.bound, &b.bound)
            .then_with(|| subtree_min_skey(&a.node).cmp(&subtree_min_skey(&b.node)))
    });
}

/// The conservative interior descend predicate for one column (gist.md §5). For `&&`/`@>`, a
/// matching row must overlap the query, and every row is contained in its subtree's union, so a
/// non-overlapping union can hold no match — `overlaps` prunes safely. For `=`, a matching value
/// must lie within the subtree's `[min, max]`, so a query key outside it prunes safely.
fn descend_comp(union: &GistBoundKey, query: &GistQuery, strat: GistStrategy) -> bool {
    match (union, query, strat) {
        (GistBoundKey::Range(u), GistQuery::Range(q), GistStrategy::Overlaps)
        | (GistBoundKey::Range(u), GistQuery::Range(q), GistStrategy::Contains) => {
            range_overlaps(u, q)
        }
        (GistBoundKey::Scalar { min, max }, GistQuery::Scalar(q), GistStrategy::Equal) => {
            min.as_slice() <= q.as_slice() && q.as_slice() <= max.as_slice()
        }
        _ => panic!("BUG: gist strategy / bound-key / query kind mismatch"),
    }
}

/// Descend into a child iff EVERY column's component is consistent with its query (a conjunction —
/// the exclusion-probe and single-column descent are the one- and many-column cases of this).
fn descend(union: &[GistBoundKey], query: &[GistQuery], strats: &[GistStrategy]) -> bool {
    union
        .iter()
        .zip(query)
        .zip(strats)
        .all(|((u, q), &s)| descend_comp(u, q, s))
}

/// The exact operator for one column, applied at the leaf to keep only true matches.
fn leaf_match_comp(bound: &GistBoundKey, query: &GistQuery, strat: GistStrategy) -> bool {
    match (bound, query, strat) {
        (GistBoundKey::Range(b), GistQuery::Range(q), GistStrategy::Overlaps) => {
            range_overlaps(b, q)
        }
        (GistBoundKey::Range(b), GistQuery::Range(q), GistStrategy::Contains) => {
            range_contains(b, q)
        }
        // A leaf's scalar component is the degenerate `[v, v]`, so equality is `min == query key`.
        (GistBoundKey::Scalar { min, .. }, GistQuery::Scalar(q), GistStrategy::Equal) => {
            min.as_slice() == q.as_slice()
        }
        _ => panic!("BUG: gist strategy / bound-key / query kind mismatch"),
    }
}

/// A leaf row matches iff EVERY column's exact operator is TRUE (the full conjunction). For a
/// single-column index this is the lone operator; for an `EXCLUDE` probe it is the whole
/// `(expr_i op_i)` conjunction, so a leaf hit IS a conflicting row (gist.md §7).
fn leaf_matches(bound: &[GistBoundKey], query: &[GistQuery], strats: &[GistStrategy]) -> bool {
    bound
        .iter()
        .zip(query)
        .zip(strats)
        .all(|((b, q), &s)| leaf_match_comp(b, q, s))
}

fn search_node(
    node: &GistNode,
    query: &[GistQuery],
    strats: &[GistStrategy],
    out: &mut Vec<Vec<u8>>,
    nodes: &mut usize,
    interior: &mut usize,
) {
    *nodes += 1;
    match node {
        GistNode::Leaf(entries) => {
            for e in entries {
                if leaf_matches(&e.bound, query, strats) {
                    out.push(e.skey.clone());
                }
            }
        }
        GistNode::Interior(children) => {
            *interior += 1;
            for c in children {
                if descend(&c.bound, query, strats) {
                    search_node(&c.node, query, strats, out, nodes, interior);
                }
            }
        }
    }
}

// ---- on-disk node codec (gist.md §4.1) -------------------------------------------------------

/// One serialized GiST node page: its page number, type (leaf 5 / interior 6), the entry count
/// (the page header's `item_count`), and the payload bytes that follow the standard 16-byte page
/// header. Page allocation is post-order (children before parent, the root last) so page numbers
/// are a deterministic function of the tree. `item_count` is load-bearing: a file page is padded to
/// `page_size`, so the loader parses exactly `item_count` entries rather than to the buffer end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GistPage {
    pub page_no: u32,
    pub page_type: u8,
    pub item_count: u32,
    pub payload: Vec<u8>,
}

/// Serialize the whole tree to its node pages in canonical post-order (children before parent, the
/// root last). `alloc` hands out the next page number — a contiguous counter for the from-scratch
/// image, or the free-list allocator for an incremental commit — so GiST pages interleave with the
/// rest of the file's pages. Returns the pages (each with its allocated number) and the root page.
pub fn serialize_tree<A>(
    tree: &GistTree,
    ops: &[GistOpclass],
    alloc: &mut A,
) -> (Vec<GistPage>, u32)
where
    A: FnMut() -> u32,
{
    let mut pages = Vec::new();
    let root = serialize_node(&tree.root, ops, &mut pages, alloc);
    (pages, root)
}

fn serialize_node<A>(
    node: &GistNode,
    ops: &[GistOpclass],
    pages: &mut Vec<GistPage>,
    alloc: &mut A,
) -> u32
where
    A: FnMut() -> u32,
{
    match node {
        GistNode::Leaf(entries) => {
            let mut payload = Vec::new();
            for e in entries {
                let b = encode_bound(ops, &e.bound);
                payload.extend_from_slice(&(b.len() as u16).to_be_bytes());
                payload.extend_from_slice(&b);
                payload.extend_from_slice(&(e.skey.len() as u16).to_be_bytes());
                payload.extend_from_slice(&e.skey);
            }
            let page_no = alloc();
            pages.push(GistPage {
                page_no,
                page_type: PAGE_GIST_LEAF,
                item_count: entries.len() as u32,
                payload,
            });
            page_no
        }
        GistNode::Interior(children) => {
            // Children first (post-order), in the node's canonical entry order.
            let child_pages: Vec<u32> = children
                .iter()
                .map(|c| serialize_node(&c.node, ops, pages, alloc))
                .collect();
            let mut payload = Vec::new();
            for (c, cp) in children.iter().zip(child_pages.iter()) {
                let b = encode_bound(ops, &c.bound);
                payload.extend_from_slice(&(b.len() as u16).to_be_bytes());
                payload.extend_from_slice(&b);
                payload.extend_from_slice(&cp.to_be_bytes());
            }
            let page_no = alloc();
            pages.push(GistPage {
                page_no,
                page_type: PAGE_GIST_INTERIOR,
                item_count: children.len() as u32,
                payload,
            });
            page_no
        }
    }
}

/// Rebuild a tree from its node pages, starting at `root_page`. `fetch` returns the [`GistPage`] for
/// a page number — the format layer reads it on demand from the pager (header `page_type` +
/// `item_count`, payload after the 16-byte header), so the tree need not be materialized as a map.
/// `ops` is the per-column opclass list for decoding bounds.
pub fn load_tree<F>(ops: &[GistOpclass], root_page: u32, fetch: &F) -> Result<GistTree>
where
    F: Fn(u32) -> Result<GistPage>,
{
    let root = load_node(ops, root_page, fetch)?;
    let len = count_rows(&root);
    Ok(GistTree { root, len })
}

fn count_rows(node: &GistNode) -> usize {
    match node {
        GistNode::Leaf(entries) => entries.len(),
        GistNode::Interior(children) => children.iter().map(|c| count_rows(&c.node)).sum(),
    }
}

fn load_node<F>(ops: &[GistOpclass], page_no: u32, fetch: &F) -> Result<GistNode>
where
    F: Fn(u32) -> Result<GistPage>,
{
    let p = fetch(page_no)?;
    let buf = &p.payload;
    let mut pos = 0usize;
    match p.page_type {
        PAGE_GIST_LEAF => {
            let mut entries = Vec::with_capacity(p.item_count as usize);
            for _ in 0..p.item_count {
                let bound = read_framed_bound(ops, buf, &mut pos)?;
                let slen = rd_u16(buf, &mut pos)? as usize;
                let skey = rd_bytes(buf, &mut pos, slen)?;
                entries.push(LeafEntry { bound, skey });
            }
            Ok(GistNode::Leaf(entries))
        }
        PAGE_GIST_INTERIOR => {
            let mut children = Vec::with_capacity(p.item_count as usize);
            for _ in 0..p.item_count {
                let bound = read_framed_bound(ops, buf, &mut pos)?;
                let child_page = rd_u32(buf, &mut pos)?;
                let node = Box::new(load_node(ops, child_page, fetch)?);
                children.push(ChildEntry { bound, node });
            }
            Ok(GistNode::Interior(children))
        }
        other => Err(corrupt(&format!("gist: bad page_type {other}"))),
    }
}

// ---- the leaf-key codec + canonical-order build (the executor/serializer API) -----------------

/// Build a row's leaf-store key from one component per indexed column (the GIN `term ‖ skey`
/// pattern): each component's self-delimiting bytes in column order, then the storage key (which
/// makes the entry unique). This is what `index_entry_keys` produces for an `IndexKind::Gist` index,
/// so all existing insert/update/delete maintenance is reused. For a single-column index the bytes
/// equal the one component's encoding (the GX1/GX2 leaf-store form is unchanged).
pub fn leaf_key_multi(comps: &[GistLeafComp], skey: &[u8]) -> Vec<u8> {
    let mut k = Vec::new();
    for c in comps {
        match c {
            GistLeafComp::Range(elem, rv) => {
                let op = GistOpclass::Range(*elem);
                k.extend_from_slice(&op.encode_comp(&GistBoundKey::Range((*rv).clone())));
            }
            GistLeafComp::Scalar(key) => {
                let op = GistOpclass::Scalar;
                k.extend_from_slice(&op.encode_comp(&GistBoundKey::Scalar {
                    min: key.to_vec(),
                    max: key.to_vec(),
                }));
            }
        }
    }
    k.extend_from_slice(skey);
    k
}

/// Build a single-column `range_ops` leaf-store key (the GX1 convenience over `leaf_key_multi`).
pub fn range_leaf_key(elem: ScalarType, rv: &RangeVal, skey: &[u8]) -> Vec<u8> {
    leaf_key_multi(&[GistLeafComp::Range(elem, rv)], skey)
}

/// Build a single-column scalar `=` leaf-store key (the GX2 convenience): the value's
/// order-preserving KEY bytes as the degenerate `[v, v]` bound, then its storage key. `value_key`
/// is `encode_key_value` of the row's scalar value — the executor computes it (gist.rs never
/// encodes a value, only compares bytes).
pub fn scalar_leaf_key(value_key: &[u8], skey: &[u8]) -> Vec<u8> {
    leaf_key_multi(&[GistLeafComp::Scalar(value_key)], skey)
}

/// Split a leaf-store key back into `(tuple bound, storage_key)` — the inverse of the
/// `leaf_key_multi` layout (each component is self-delimiting, so the remainder is the storage key).
fn decode_leaf_key(ops: &[GistOpclass], key: &[u8]) -> Result<(Vec<GistBoundKey>, Vec<u8>)> {
    let mut pos = 0usize;
    let bound = read_bound(ops, key, &mut pos)?;
    Ok((bound, key[pos..].to_vec()))
}

/// Build the persisted R-tree from the index store's leaf keys. The keys are decoded and inserted
/// in **canonical order** (`bound_total_cmp`, ties by storage key), so the tree is a pure function
/// of the leaf *set* — content-deterministic, independent of the original mutation order (gist.md
/// §3; stronger than the operation-determinism the design floor requires, and what makes the
/// commit-time rebuild and the golden round-trip reproducible).
pub fn build_from_leaf_keys<'a, I>(ops: &[GistOpclass], keys: I) -> Result<GistTree>
where
    I: IntoIterator<Item = &'a [u8]>,
{
    let mut entries: Vec<(Vec<GistBoundKey>, Vec<u8>)> = Vec::new();
    for k in keys {
        entries.push(decode_leaf_key(ops, k)?);
    }
    entries.sort_by(|a, b| bound_total_cmp(&a.0, &b.0).then_with(|| a.1.cmp(&b.1)));
    let mut tree = GistTree::new();
    for (bound, skey) in entries {
        tree.insert(ops, bound, skey);
    }
    Ok(tree)
}

/// Flatten a tree back to its leaf keys (`leaf_key_multi` per row) — used on load to rebuild the
/// index store from the persisted R-tree (the in-memory store is the leaf-key PMap; the R-tree is
/// the on-disk form, gist.md §4.1). Order is irrelevant (the store re-sorts).
pub fn leaf_keys(tree: &GistTree, ops: &[GistOpclass]) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(tree.len);
    collect_leaf_keys(&tree.root, ops, &mut out);
    out
}

fn collect_leaf_keys(node: &GistNode, ops: &[GistOpclass], out: &mut Vec<Vec<u8>>) {
    match node {
        GistNode::Leaf(entries) => {
            for e in entries {
                let mut k = encode_bound(ops, &e.bound);
                k.extend_from_slice(&e.skey);
                out.push(k);
            }
        }
        GistNode::Interior(children) => {
            for c in children {
                collect_leaf_keys(&c.node, ops, out);
            }
        }
    }
}

/// Read one length-prefixed node bound (`bound_len u16 ‖ tuple bound`) — the §4.1 node framing,
/// which length-delimits the (already self-delimiting) tuple bound so a future non-self-delimiting
/// opclass still parses.
fn read_framed_bound(
    ops: &[GistOpclass],
    buf: &[u8],
    pos: &mut usize,
) -> Result<Vec<GistBoundKey>> {
    let blen = rd_u16(buf, pos)? as usize;
    if *pos + blen > buf.len() {
        return Err(corrupt("gist: truncated bound"));
    }
    let slice = &buf[*pos..*pos + blen];
    let mut bpos = 0usize;
    let bound = read_bound(ops, slice, &mut bpos)?;
    *pos += blen;
    Ok(bound)
}

fn rd_u16(buf: &[u8], pos: &mut usize) -> Result<u16> {
    if *pos + 2 > buf.len() {
        return Err(corrupt("gist: truncated u16"));
    }
    let v = u16::from_be_bytes([buf[*pos], buf[*pos + 1]]);
    *pos += 2;
    Ok(v)
}

fn rd_u32(buf: &[u8], pos: &mut usize) -> Result<u32> {
    if *pos + 4 > buf.len() {
        return Err(corrupt("gist: truncated u32"));
    }
    let v = u32::from_be_bytes([buf[*pos], buf[*pos + 1], buf[*pos + 2], buf[*pos + 3]]);
    *pos += 4;
    Ok(v)
}

fn rd_bytes(buf: &[u8], pos: &mut usize, n: usize) -> Result<Vec<u8>> {
    if *pos + n > buf.len() {
        return Err(corrupt("gist: truncated bytes"));
    }
    let v = buf[*pos..*pos + n].to_vec();
    *pos += n;
    Ok(v)
}

fn corrupt(msg: &str) -> EngineError {
    EngineError::new(SqlState::DataCorrupted, msg.to_string())
}

// Silence the unused-import lint for `Ordering` if the comparator forms change.
const _: fn() -> Ordering = || Ordering::Equal;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ScalarType;
    use std::collections::HashMap;

    fn i32_range_ops() -> Vec<GistOpclass> {
        vec![GistOpclass::Range(ScalarType::Int32)]
    }

    /// A canonical discrete `[lo, hi)` i32 range value.
    fn r(lo: i32, hi: i32) -> RangeVal {
        RangeVal {
            empty: false,
            lower: Some(Box::new(Value::Int(lo as i64))),
            upper: Some(Box::new(Value::Int(hi as i64))),
            lower_inc: true,
            upper_inc: false,
        }
    }

    /// A storage key for row id `n` (4-byte big-endian — stands in for the real key encoding).
    fn skey(n: u32) -> Vec<u8> {
        n.to_be_bytes().to_vec()
    }

    fn build(rows: &[(i32, i32, u32)]) -> GistTree {
        let ops = i32_range_ops();
        let mut t = GistTree::new();
        for &(lo, hi, id) in rows {
            t.insert(&ops, vec![GistBoundKey::Range(r(lo, hi))], skey(id));
        }
        t
    }

    /// Brute-force reference: the exact set of storage keys matching the operator.
    fn brute(rows: &[(i32, i32, u32)], q: &RangeVal, strat: GistStrategy) -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = rows
            .iter()
            .filter(|&&(lo, hi, _)| match strat {
                GistStrategy::Overlaps => range_overlaps(&r(lo, hi), q),
                GistStrategy::Contains => range_contains(&r(lo, hi), q),
                GistStrategy::Equal => unreachable!(),
            })
            .map(|&(_, _, id)| skey(id))
            .collect();
        out.sort();
        out
    }

    fn sorted(mut v: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        v.sort();
        v
    }

    /// A single-column range query + strategy.
    fn rsearch(t: &GistTree, lo: i32, hi: i32, strat: GistStrategy) -> Vec<Vec<u8>> {
        t.search(&[GistQuery::Range(r(lo, hi))], &[strat]).0
    }

    /// A contiguous page allocator from `base` (mirrors the from-scratch image's counter).
    fn contig(base: u32) -> impl FnMut() -> u32 {
        let mut n = base;
        move || {
            let i = n;
            n += 1;
            i
        }
    }

    /// A `fetch` closure over an in-memory page map (mirrors the format layer reading pages on
    /// demand). Pages are padded to a page size to exercise the parse-exactly-`item_count` path.
    fn fetcher(pages: Vec<GistPage>) -> impl Fn(u32) -> Result<GistPage> {
        let map: HashMap<u32, GistPage> = pages
            .into_iter()
            .map(|mut p| {
                p.payload.resize(256, 0); // pad like a real page body
                (p.page_no, p)
            })
            .collect();
        move |page_no| {
            map.get(&page_no)
                .cloned()
                .ok_or_else(|| corrupt(&format!("gist: missing page {page_no}")))
        }
    }

    #[test]
    fn empty_tree_searches_to_nothing() {
        let t = GistTree::new();
        let hits = rsearch(&t, 1, 5, GistStrategy::Overlaps);
        assert!(hits.is_empty());
        assert!(t.is_empty());
    }

    #[test]
    fn single_level_overlap_and_contains() {
        // Few enough rows to stay a single leaf (<= FANOUT).
        let rows = [(1, 5, 1), (10, 20, 2), (3, 8, 3)];
        let t = build(&rows);
        let q = r(4, 6);
        assert_eq!(
            sorted(rsearch(&t, 4, 6, GistStrategy::Overlaps)),
            brute(&rows, &q, GistStrategy::Overlaps)
        );
        // @> : which rows contain [4,6)?  [3,8) does; [1,5) does not (5 < 6).
        assert_eq!(
            sorted(rsearch(&t, 4, 6, GistStrategy::Contains)),
            brute(&rows, &q, GistStrategy::Contains)
        );
    }

    #[test]
    fn multi_level_tree_matches_brute_force() {
        // Enough rows to force several splits (FANOUT = 4), exercising interior descent.
        let rows: Vec<(i32, i32, u32)> = (0..40).map(|i| (i, i + 3, i as u32)).collect();
        let t = build(&rows);
        // It must have grown past one node.
        assert!(matches!(t.root, GistNode::Interior(_)));
        for &(qlo, qhi) in &[(0, 1), (5, 9), (20, 25), (37, 50), (100, 200)] {
            let q = r(qlo, qhi);
            for strat in [GistStrategy::Overlaps, GistStrategy::Contains] {
                assert_eq!(
                    sorted(rsearch(&t, qlo, qhi, strat)),
                    brute(&rows, &q, strat),
                    "mismatch q=[{qlo},{qhi}) strat={strat:?}"
                );
            }
        }
    }

    #[test]
    fn empty_range_row_never_matches_overlap_or_contains() {
        let ops = i32_range_ops();
        let mut t = build(&[(1, 5, 1), (10, 20, 2)]);
        t.insert(&ops, vec![GistBoundKey::Range(RangeVal::empty())], skey(99)); // an empty-range row
        let hits = rsearch(&t, 0, 100, GistStrategy::Overlaps);
        assert!(!sorted(hits).contains(&skey(99)));
    }

    #[test]
    fn serialize_load_round_trips_and_preserves_search() {
        let rows: Vec<(i32, i32, u32)> = (0..30).map(|i| (i * 2, i * 2 + 5, i as u32)).collect();
        let t = build(&rows);
        let ops = i32_range_ops();
        let (pages, root) = serialize_tree(&t, &ops, &mut contig(7));
        // Re-serializing the loaded tree yields identical pages (deterministic codec) — even though
        // load reads padded pages and parses exactly `item_count` entries.
        let loaded = load_tree(&ops, root, &fetcher(pages.clone())).unwrap();
        let (pages2, root2) = serialize_tree(&loaded, &ops, &mut contig(7));
        assert_eq!(root, root2);
        assert_eq!(
            pages, pages2,
            "serialize is not deterministic across round-trip"
        );
        assert_eq!(loaded.len(), t.len());
        // The loaded tree answers searches identically to the original / brute force.
        for &(qlo, qhi) in &[(0, 1), (10, 14), (40, 60), (200, 300)] {
            let q = r(qlo, qhi);
            for strat in [GistStrategy::Overlaps, GistStrategy::Contains] {
                assert_eq!(
                    sorted(rsearch(&t, qlo, qhi, strat)),
                    sorted(rsearch(&loaded, qlo, qhi, strat))
                );
                assert_eq!(
                    sorted(rsearch(&loaded, qlo, qhi, strat)),
                    brute(&rows, &q, strat)
                );
            }
        }
    }

    #[test]
    fn page_types_and_postorder_allocation() {
        let rows: Vec<(i32, i32, u32)> = (0..12).map(|i| (i, i + 2, i as u32)).collect();
        let t = build(&rows);
        let ops = i32_range_ops();
        let (pages, root) = serialize_tree(&t, &ops, &mut contig(0));
        // Post-order: the root is allocated last (highest page number).
        assert_eq!(root, pages.iter().map(|p| p.page_no).max().unwrap());
        // Page numbers are a contiguous 0..n.
        let mut nums: Vec<u32> = pages.iter().map(|p| p.page_no).collect();
        nums.sort();
        assert_eq!(nums, (0..pages.len() as u32).collect::<Vec<_>>());
        // Only GiST page types appear.
        assert!(
            pages
                .iter()
                .all(|p| p.page_type == PAGE_GIST_LEAF || p.page_type == PAGE_GIST_INTERIOR)
        );
        assert_eq!(
            pages.iter().find(|p| p.page_no == root).unwrap().page_type,
            PAGE_GIST_INTERIOR
        );
    }

    #[test]
    fn leaf_key_round_trips() {
        let ops = i32_range_ops();
        for (b, id) in [(r(1, 5), 1u32), (r(-3, 100), 7), (RangeVal::empty(), 9)] {
            let k = range_leaf_key(ScalarType::Int32, &b, &skey(id));
            let (b2, sk2) = decode_leaf_key(&ops, &k).unwrap();
            assert_eq!(b2, vec![GistBoundKey::Range(b)]);
            assert_eq!(sk2, skey(id));
        }
    }

    #[test]
    fn build_from_leaf_keys_is_order_independent_and_correct() {
        // The persisted tree is built from the leaf SET in canonical order, so two different
        // insertion orders of the same rows produce byte-identical trees (content-determinism).
        let ops = i32_range_ops();
        let rows: Vec<(i32, i32, u32)> = (0..25)
            .map(|i| (i * 3 % 17, i * 3 % 17 + 4, i as u32))
            .collect();
        let keys_fwd: Vec<Vec<u8>> = rows
            .iter()
            .map(|&(lo, hi, id)| range_leaf_key(ScalarType::Int32, &r(lo, hi), &skey(id)))
            .collect();
        let mut keys_rev = keys_fwd.clone();
        keys_rev.reverse();

        let t1 = build_from_leaf_keys(&ops, keys_fwd.iter().map(|k| k.as_slice())).unwrap();
        let t2 = build_from_leaf_keys(&ops, keys_rev.iter().map(|k| k.as_slice())).unwrap();
        let (p1, r1) = serialize_tree(&t1, &ops, &mut contig(0));
        let (p2, r2) = serialize_tree(&t2, &ops, &mut contig(0));
        assert_eq!((r1, &p1), (r2, &p2), "build is not order-independent");

        // And it answers searches exactly like brute force.
        for &(qlo, qhi) in &[(0, 2), (5, 9), (14, 20), (100, 200)] {
            let q = r(qlo, qhi);
            for strat in [GistStrategy::Overlaps, GistStrategy::Contains] {
                assert_eq!(
                    sorted(rsearch(&t1, qlo, qhi, strat)),
                    brute(&rows, &q, strat)
                );
            }
        }
    }

    // ---- scalar `=` opclass (GX2) -------------------------------------------------------------

    /// An i32 value's order-preserving key bytes (sign-flip big-endian) — what the executor's
    /// `encode_key_value` produces, reproduced here so the scalar tests are self-contained.
    fn i32_key(v: i32) -> Vec<u8> {
        ((v as i64 as u64) ^ (1u64 << 63)).to_be_bytes()[4..].to_vec()
    }

    fn scalar_ops() -> Vec<GistOpclass> {
        vec![GistOpclass::Scalar]
    }

    fn scalar_build(rows: &[(i32, u32)]) -> GistTree {
        let keys: Vec<Vec<u8>> = rows
            .iter()
            .map(|&(v, id)| scalar_leaf_key(&i32_key(v), &skey(id)))
            .collect();
        build_from_leaf_keys(&scalar_ops(), keys.iter().map(|k| k.as_slice())).unwrap()
    }

    fn scalar_brute(rows: &[(i32, u32)], q: i32) -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = rows
            .iter()
            .filter(|&&(v, _)| v == q)
            .map(|&(_, id)| skey(id))
            .collect();
        out.sort();
        out
    }

    fn scalar_search(t: &GistTree, q: i32) -> Vec<Vec<u8>> {
        t.search(&[GistQuery::Scalar(i32_key(q))], &[GistStrategy::Equal])
            .0
    }

    #[test]
    fn scalar_equal_matches_brute_force_across_splits() {
        // Duplicates (same value, distinct rows) + enough rows to force interior nodes.
        let rows: Vec<(i32, u32)> = (0..40).map(|i| (i % 9, i as u32)).collect();
        let t = scalar_build(&rows);
        assert!(matches!(t.root, GistNode::Interior(_)));
        for q in [-3, 0, 4, 8, 9, 100] {
            assert_eq!(
                sorted(scalar_search(&t, q)),
                scalar_brute(&rows, q),
                "q={q}"
            );
        }
    }

    #[test]
    fn scalar_round_trips_and_is_order_independent() {
        let rows: Vec<(i32, u32)> = (0..30).map(|i| ((i * 7) % 13 - 4, i as u32)).collect();
        let keys_fwd: Vec<Vec<u8>> = rows
            .iter()
            .map(|&(v, id)| scalar_leaf_key(&i32_key(v), &skey(id)))
            .collect();
        let mut keys_rev = keys_fwd.clone();
        keys_rev.reverse();
        let ops = scalar_ops();
        let t1 = build_from_leaf_keys(&ops, keys_fwd.iter().map(|k| k.as_slice())).unwrap();
        let t2 = build_from_leaf_keys(&ops, keys_rev.iter().map(|k| k.as_slice())).unwrap();
        let (p1, r1) = serialize_tree(&t1, &ops, &mut contig(0));
        let (p2, r2) = serialize_tree(&t2, &ops, &mut contig(0));
        assert_eq!(
            (r1, &p1),
            (r2, &p2),
            "scalar build is not order-independent"
        );

        // Re-load from pages and confirm searches still match brute force.
        let loaded = load_tree(&ops, r1, &fetcher(p1)).unwrap();
        for q in [-4, -1, 0, 3, 8, 50] {
            assert_eq!(
                sorted(scalar_search(&loaded, q)),
                scalar_brute(&rows, q),
                "q={q}"
            );
        }
    }

    #[test]
    fn scalar_leaf_key_round_trips() {
        let ops = scalar_ops();
        for (v, id) in [(1i32, 1u32), (-7, 7), (12345, 9)] {
            let k = scalar_leaf_key(&i32_key(v), &skey(id));
            let (b, sk) = decode_leaf_key(&ops, &k).unwrap();
            assert_eq!(
                b,
                vec![GistBoundKey::Scalar {
                    min: i32_key(v),
                    max: i32_key(v)
                }]
            );
            assert_eq!(sk, skey(id));
        }
    }

    // ---- multi-column GiST (GX3) — the EXCLUDE backing structure -------------------------------

    /// A two-column `(scalar =, range &&)` index — the canonical `EXCLUDE (a WITH =, r WITH &&)`
    /// shape (gist.md §7). Each row is `(scalar a, range [lo,hi), id)`.
    fn multi_ops() -> Vec<GistOpclass> {
        vec![GistOpclass::Scalar, GistOpclass::Range(ScalarType::Int32)]
    }

    fn multi_leaf(a: i32, lo: i32, hi: i32, id: u32) -> Vec<u8> {
        leaf_key_multi(
            &[
                GistLeafComp::Scalar(&i32_key(a)),
                GistLeafComp::Range(ScalarType::Int32, &r(lo, hi)),
            ],
            &skey(id),
        )
    }

    fn multi_build(rows: &[(i32, i32, i32, u32)]) -> GistTree {
        let keys: Vec<Vec<u8>> = rows
            .iter()
            .map(|&(a, lo, hi, id)| multi_leaf(a, lo, hi, id))
            .collect();
        build_from_leaf_keys(&multi_ops(), keys.iter().map(|k| k.as_slice())).unwrap()
    }

    /// The conjunction probe: which rows have `a = qa AND [lo,hi) && [qlo,qhi)`?
    fn multi_search(t: &GistTree, qa: i32, qlo: i32, qhi: i32) -> Vec<Vec<u8>> {
        t.search(
            &[
                GistQuery::Scalar(i32_key(qa)),
                GistQuery::Range(r(qlo, qhi)),
            ],
            &[GistStrategy::Equal, GistStrategy::Overlaps],
        )
        .0
    }

    fn multi_brute(rows: &[(i32, i32, i32, u32)], qa: i32, qlo: i32, qhi: i32) -> Vec<Vec<u8>> {
        let q = r(qlo, qhi);
        let mut out: Vec<Vec<u8>> = rows
            .iter()
            .filter(|&&(a, lo, hi, _)| a == qa && range_overlaps(&r(lo, hi), &q))
            .map(|&(_, _, _, id)| skey(id))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn multi_column_conjunction_matches_brute_force() {
        // Several `a` buckets, overlapping ranges, enough rows to split.
        let rows: Vec<(i32, i32, i32, u32)> = (0..40)
            .map(|i| (i % 5, i % 11, i % 11 + 3, i as u32))
            .collect();
        let t = multi_build(&rows);
        assert!(matches!(t.root, GistNode::Interior(_)));
        for &(qa, qlo, qhi) in &[(0, 0, 2), (2, 5, 9), (3, 8, 12), (4, 0, 50), (9, 0, 100)] {
            assert_eq!(
                sorted(multi_search(&t, qa, qlo, qhi)),
                multi_brute(&rows, qa, qlo, qhi),
                "q=({qa},[{qlo},{qhi}))"
            );
        }
    }

    #[test]
    fn multi_column_round_trips_and_is_order_independent() {
        let rows: Vec<(i32, i32, i32, u32)> = (0..30)
            .map(|i| ((i * 7) % 6, i % 9, i % 9 + 2, i as u32))
            .collect();
        let keys_fwd: Vec<Vec<u8>> = rows
            .iter()
            .map(|&(a, lo, hi, id)| multi_leaf(a, lo, hi, id))
            .collect();
        let mut keys_rev = keys_fwd.clone();
        keys_rev.reverse();
        let ops = multi_ops();
        let t1 = build_from_leaf_keys(&ops, keys_fwd.iter().map(|k| k.as_slice())).unwrap();
        let t2 = build_from_leaf_keys(&ops, keys_rev.iter().map(|k| k.as_slice())).unwrap();
        let (p1, r1) = serialize_tree(&t1, &ops, &mut contig(0));
        let (p2, r2) = serialize_tree(&t2, &ops, &mut contig(0));
        assert_eq!((r1, &p1), (r2, &p2), "multi build is not order-independent");
        // Re-load and confirm the conjunction probe still matches brute force.
        let loaded = load_tree(&ops, r1, &fetcher(p1)).unwrap();
        for &(qa, qlo, qhi) in &[(0, 0, 2), (3, 5, 9), (5, 0, 100)] {
            assert_eq!(
                sorted(multi_search(&loaded, qa, qlo, qhi)),
                multi_brute(&rows, qa, qlo, qhi)
            );
        }
        // `leaf_keys` round-trips the leaf set (used on load to repopulate the index store).
        assert_eq!(leaf_keys(&loaded, &ops).len(), rows.len());
    }
}
