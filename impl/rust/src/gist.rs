//! GiST access method — the operation-deterministic R-tree (spec/design/gist.md).
//!
//! GX1 ships the **`range_ops`** opclass: a GiST index over a `range` column, accelerating the
//! overlap `&&` and containment `@>` operators. This module is the self-contained core — the
//! in-memory R-tree (build / penalty / median split), the on-disk node codec (the §4.1 byte
//! layout, page types 5/6), and the consistent-descent search. Catalog/format integration
//! (`IndexKind::Gist`, the grammar, `format_version` 20, the planner gather) is wired separately
//! and reuses these primitives.
//!
//! Determinism (gist.md §3): every operation is a pure function of its inputs, so the identical
//! mutation sequence every core replays builds the byte-identical tree. Within a node, entries are
//! ordered canonically (`range_total_cmp`, ties by storage-key / subtree-min-key), so a node's
//! bytes are a pure function of its entry set; pages are assigned in a canonical post-order walk.

use crate::catalog::ColType;
use crate::error::{EngineError, Result};
use crate::format::{encode_range_body, read_range_body};
use crate::range::{range_contains, range_overlaps, range_total_cmp, range_union};
use crate::sqlstate::SqlState;
use crate::value::{RangeVal, Value};
use std::cmp::Ordering;

/// Maximum entries per GiST node (gist.md §4.1). A pinned cross-core constant: inserting an
/// (N+1)-th entry triggers a median `picksplit`. Small enough that a few rows exercise a
/// multi-level tree; every GX1 element bound fits a page well within this fan-out.
pub const GIST_FANOUT: usize = 4;

/// GiST page types (gist.md §4.1, format.md *Page header*).
pub const PAGE_GIST_LEAF: u8 = 5;
pub const PAGE_GIST_INTERIOR: u8 = 6;

/// The query operators `range_ops` serves. GX1 accelerates **`Overlaps`** (`&&`) and **`Contains`**
/// (`@>`); the positional operators (`<<`/`>>`/`&<`/`&>`/`-|-`), `<@`, `=`, and the empty-query
/// edge cases stay full-scan this slice (the GIN-`<@` precedent, gist.md §5/§11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GistStrategy {
    /// `col && Q` — the range overlap operator.
    Overlaps,
    /// `col @> Q` — `col` contains the query range/element.
    Contains,
}

/// A leaf entry: the row's range value (the bound) plus its storage key.
#[derive(Clone, Debug)]
struct LeafEntry {
    bound: RangeVal,
    skey: Vec<u8>,
}

/// An interior entry: the union range covering a child subtree, plus the child node.
#[derive(Clone, Debug)]
struct ChildEntry {
    bound: RangeVal,
    node: Box<GistNode>,
}

/// A GiST tree node — a leaf of row entries or an interior of child entries (each carrying its
/// subtree's covering union as its bound). Unlike the ordered B-tree, an interior holds **one
/// bound per child** (N bounds, N children), not N separators between N+1 children.
#[derive(Clone, Debug)]
enum GistNode {
    Leaf(Vec<LeafEntry>),
    Interior(Vec<ChildEntry>),
}

/// An operation-deterministic GiST R-tree over a single range column.
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

    /// Insert one row's `(range bound, storage key)` into the tree. `elem` is the range's element
    /// (sub)type, used by the value codec and the penalty metric.
    pub fn insert(&mut self, elem: &ColType, bound: RangeVal, skey: Vec<u8>) {
        if let Some(sib) = insert_node(&mut self.root, elem, bound, skey) {
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

    /// Consistent-descent search: every storage key whose row satisfies `query OP col` under
    /// `strat`. The interior descend predicate is conservative (no false negatives); the exact
    /// operator is applied at the leaf. Returns `(storage keys, nodes_visited)` — the second is the
    /// `gist_descent` cost (interior + leaf nodes touched).
    pub fn search(&self, query: &RangeVal, strat: GistStrategy) -> (Vec<Vec<u8>>, usize) {
        let mut out = Vec::new();
        let mut visited = 0usize;
        search_node(&self.root, query, strat, &mut out, &mut visited);
        (out, visited)
    }
}

/// Choose the child to descend on insert: the one whose union, merged with the new entry, has the
/// lexicographically-smallest value-codec bytes; ties keep the lower slot (gist.md §3 `penalty`).
fn choose_child(children: &[ChildEntry], elem: &ColType, bound: &RangeVal) -> usize {
    let mut best = 0usize;
    let mut best_key: Option<Vec<u8>> = None;
    for (i, c) in children.iter().enumerate() {
        let merged = range_union(&c.bound, bound, false).expect("range_merge is total");
        let key = encode_range_body(elem, &merged);
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
    elem: &ColType,
    bound: RangeVal,
    skey: Vec<u8>,
) -> Option<ChildEntry> {
    match node {
        GistNode::Leaf(entries) => {
            entries.push(LeafEntry { bound, skey });
            sort_leaf(entries);
            split_if_overflow(node)
        }
        GistNode::Interior(children) => {
            let i = choose_child(children, elem, &bound);
            let split = insert_node(&mut children[i].node, elem, bound, skey);
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

/// The covering union of a node's entries (the convex-hull merge — never errors). The node must be
/// non-empty (the empty tree's root leaf is never unioned).
fn node_union(node: &GistNode) -> RangeVal {
    let merge_all = |bounds: &mut dyn Iterator<Item = RangeVal>| -> RangeVal {
        let mut u = bounds.next().expect("node_union of an empty node");
        for b in bounds {
            u = range_union(&u, &b, false).expect("range_merge is total");
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
    entries.sort_by(|a, b| range_total_cmp(&a.bound, &b.bound).then_with(|| a.skey.cmp(&b.skey)));
}

fn sort_children(children: &mut [ChildEntry]) {
    children.sort_by(|a, b| {
        range_total_cmp(&a.bound, &b.bound)
            .then_with(|| subtree_min_skey(&a.node).cmp(&subtree_min_skey(&b.node)))
    });
}

/// The conservative interior descend predicate (gist.md §5). For `&&` and `@>`, a matching row must
/// overlap the query, and every row is contained in its subtree's union, so a non-overlapping union
/// can hold no match — `overlaps` prunes safely. (Empty-range rows never match `&&`/`@>` for a
/// non-empty query, so their absence from the union is harmless.)
fn descend(union: &RangeVal, query: &RangeVal, strat: GistStrategy) -> bool {
    match strat {
        GistStrategy::Overlaps | GistStrategy::Contains => range_overlaps(union, query),
    }
}

/// The exact operator, applied at the leaf to keep only true matches.
fn leaf_matches(bound: &RangeVal, query: &RangeVal, strat: GistStrategy) -> bool {
    match strat {
        GistStrategy::Overlaps => range_overlaps(bound, query),
        GistStrategy::Contains => range_contains(bound, query),
    }
}

fn search_node(
    node: &GistNode,
    query: &RangeVal,
    strat: GistStrategy,
    out: &mut Vec<Vec<u8>>,
    visited: &mut usize,
) {
    *visited += 1;
    match node {
        GistNode::Leaf(entries) => {
            for e in entries {
                if leaf_matches(&e.bound, query, strat) {
                    out.push(e.skey.clone());
                }
            }
        }
        GistNode::Interior(children) => {
            for c in children {
                if descend(&c.bound, query, strat) {
                    search_node(&c.node, query, strat, out, visited);
                }
            }
        }
    }
}

// ---- on-disk node codec (gist.md §4.1) -------------------------------------------------------

/// One serialized GiST node page: its page number, type (leaf 5 / interior 6), and the payload
/// bytes that follow the standard 16-byte page header. Page allocation is post-order (children
/// before parent, the root last) so page numbers are a deterministic function of the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GistPage {
    pub page_no: u32,
    pub page_type: u8,
    pub payload: Vec<u8>,
}

/// Serialize the whole tree to its node pages, numbering from `base_page` in canonical post-order.
/// Returns the pages and the root page number.
pub fn serialize_tree(tree: &GistTree, elem: &ColType, base_page: u32) -> (Vec<GistPage>, u32) {
    let mut pages = Vec::new();
    let mut next = base_page;
    let root = serialize_node(&tree.root, elem, &mut pages, &mut next);
    (pages, root)
}

fn serialize_node(
    node: &GistNode,
    elem: &ColType,
    pages: &mut Vec<GistPage>,
    next: &mut u32,
) -> u32 {
    match node {
        GistNode::Leaf(entries) => {
            let mut payload = Vec::new();
            for e in entries {
                let b = encode_range_body(elem, &e.bound);
                payload.extend_from_slice(&(b.len() as u16).to_be_bytes());
                payload.extend_from_slice(&b);
                payload.extend_from_slice(&(e.skey.len() as u16).to_be_bytes());
                payload.extend_from_slice(&e.skey);
            }
            let page_no = *next;
            *next += 1;
            pages.push(GistPage {
                page_no,
                page_type: PAGE_GIST_LEAF,
                payload,
            });
            page_no
        }
        GistNode::Interior(children) => {
            // Children first (post-order), in the node's canonical entry order.
            let child_pages: Vec<u32> = children
                .iter()
                .map(|c| serialize_node(&c.node, elem, pages, next))
                .collect();
            let mut payload = Vec::new();
            for (c, cp) in children.iter().zip(child_pages.iter()) {
                let b = encode_range_body(elem, &c.bound);
                payload.extend_from_slice(&(b.len() as u16).to_be_bytes());
                payload.extend_from_slice(&b);
                payload.extend_from_slice(&cp.to_be_bytes());
            }
            let page_no = *next;
            *next += 1;
            pages.push(GistPage {
                page_no,
                page_type: PAGE_GIST_INTERIOR,
                payload,
            });
            page_no
        }
    }
}

/// Rebuild a tree from its node pages (keyed by page number), starting at `root_page`. `elem` is the
/// range element (sub)type for decoding bounds.
pub fn load_tree(
    elem: &ColType,
    pages: &std::collections::HashMap<u32, GistPage>,
    root_page: u32,
) -> Result<GistTree> {
    let root = load_node(elem, pages, root_page)?;
    let len = count_rows(&root);
    Ok(GistTree { root, len })
}

fn count_rows(node: &GistNode) -> usize {
    match node {
        GistNode::Leaf(entries) => entries.len(),
        GistNode::Interior(children) => children.iter().map(|c| count_rows(&c.node)).sum(),
    }
}

fn load_node(
    elem: &ColType,
    pages: &std::collections::HashMap<u32, GistPage>,
    page_no: u32,
) -> Result<GistNode> {
    let p = pages
        .get(&page_no)
        .ok_or_else(|| corrupt(&format!("gist: missing page {page_no}")))?;
    let buf = &p.payload;
    let mut pos = 0usize;
    match p.page_type {
        PAGE_GIST_LEAF => {
            let mut entries = Vec::new();
            while pos < buf.len() {
                let bound = read_bound(elem, buf, &mut pos)?;
                let slen = rd_u16(buf, &mut pos)? as usize;
                let skey = rd_bytes(buf, &mut pos, slen)?;
                entries.push(LeafEntry { bound, skey });
            }
            Ok(GistNode::Leaf(entries))
        }
        PAGE_GIST_INTERIOR => {
            let mut children = Vec::new();
            while pos < buf.len() {
                let bound = read_bound(elem, buf, &mut pos)?;
                let child_page = rd_u32(buf, &mut pos)?;
                let node = Box::new(load_node(elem, pages, child_page)?);
                children.push(ChildEntry { bound, node });
            }
            Ok(GistNode::Interior(children))
        }
        other => Err(corrupt(&format!("gist: bad page_type {other}"))),
    }
}

/// Read one length-prefixed bound (`bound_len u16 ‖ encode_range_body`) into a `RangeVal`.
fn read_bound(elem: &ColType, buf: &[u8], pos: &mut usize) -> Result<RangeVal> {
    let blen = rd_u16(buf, pos)? as usize;
    if *pos + blen > buf.len() {
        return Err(corrupt("gist: truncated bound"));
    }
    let mut bpos = 0usize;
    let slice = &buf[*pos..*pos + blen];
    let v = read_range_body(elem, slice, &mut bpos)?;
    *pos += blen;
    match v {
        Value::Range(rv) => Ok(rv),
        _ => Err(corrupt("gist: bound is not a range")),
    }
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
    use crate::catalog::ColType;
    use crate::types::ScalarType;
    use std::collections::HashMap;

    fn i32_range_elem() -> ColType {
        ColType::Scalar(ScalarType::Int32)
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
        let elem = i32_range_elem();
        let mut t = GistTree::new();
        for &(lo, hi, id) in rows {
            t.insert(&elem, r(lo, hi), skey(id));
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

    fn pagemap(pages: Vec<GistPage>) -> HashMap<u32, GistPage> {
        pages.into_iter().map(|p| (p.page_no, p)).collect()
    }

    #[test]
    fn empty_tree_searches_to_nothing() {
        let t = GistTree::new();
        let (hits, _) = t.search(&r(1, 5), GistStrategy::Overlaps);
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
            sorted(t.search(&q, GistStrategy::Overlaps).0),
            brute(&rows, &q, GistStrategy::Overlaps)
        );
        // @> : which rows contain [4,6)?  [3,8) does; [1,5) does not (5 < 6).
        assert_eq!(
            sorted(t.search(&q, GistStrategy::Contains).0),
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
                    sorted(t.search(&q, strat).0),
                    brute(&rows, &q, strat),
                    "mismatch q=[{qlo},{qhi}) strat={strat:?}"
                );
            }
        }
    }

    #[test]
    fn empty_range_row_never_matches_overlap_or_contains() {
        let elem = i32_range_elem();
        let mut t = build(&[(1, 5, 1), (10, 20, 2)]);
        t.insert(&elem, RangeVal::empty(), skey(99)); // an empty-range row
        let q = r(0, 100);
        let (hits, _) = t.search(&q, GistStrategy::Overlaps);
        assert!(!sorted(hits).contains(&skey(99)));
    }

    #[test]
    fn serialize_load_round_trips_and_preserves_search() {
        let rows: Vec<(i32, i32, u32)> = (0..30).map(|i| (i * 2, i * 2 + 5, i as u32)).collect();
        let t = build(&rows);
        let elem = i32_range_elem();
        let (pages, root) = serialize_tree(&t, &elem, 7);
        // Re-serializing the loaded tree yields identical pages (deterministic codec).
        let loaded = load_tree(&elem, &pagemap(pages.clone()), root).unwrap();
        let (pages2, root2) = serialize_tree(&loaded, &elem, 7);
        assert_eq!(root, root2);
        assert_eq!(pages, pages2, "serialize is not deterministic across round-trip");
        // The loaded tree answers searches identically to the original / brute force.
        for &(qlo, qhi) in &[(0, 1), (10, 14), (40, 60), (200, 300)] {
            let q = r(qlo, qhi);
            for strat in [GistStrategy::Overlaps, GistStrategy::Contains] {
                assert_eq!(
                    sorted(t.search(&q, strat).0),
                    sorted(loaded.search(&q, strat).0)
                );
                assert_eq!(sorted(loaded.search(&q, strat).0), brute(&rows, &q, strat));
            }
        }
    }

    #[test]
    fn page_types_and_postorder_allocation() {
        let rows: Vec<(i32, i32, u32)> = (0..12).map(|i| (i, i + 2, i as u32)).collect();
        let t = build(&rows);
        let elem = i32_range_elem();
        let (pages, root) = serialize_tree(&t, &elem, 0);
        // Post-order: the root is allocated last (highest page number).
        assert_eq!(root, pages.iter().map(|p| p.page_no).max().unwrap());
        // Page numbers are a contiguous 0..n.
        let mut nums: Vec<u32> = pages.iter().map(|p| p.page_no).collect();
        nums.sort();
        assert_eq!(nums, (0..pages.len() as u32).collect::<Vec<_>>());
        // Only GiST page types appear.
        assert!(pages
            .iter()
            .all(|p| p.page_type == PAGE_GIST_LEAF || p.page_type == PAGE_GIST_INTERIOR));
        assert_eq!(
            pages.iter().find(|p| p.page_no == root).unwrap().page_type,
            PAGE_GIST_INTERIOR
        );
    }
}
