//! Persistent (copy-on-write) ordered map — the page-backed **B+tree** (decision **B1**,
//! spec/design/bplus-reshape.md; spec/design/transactions.md §3; spec/fileformat/format.md "The
//! per-table data B+tree").
//!
//! Keyed by the encoded key bytes (`Vec<u8>`, whose `Ord` is lexicographic = the
//! order-preserving key encoding's memcmp contract, spec/design/encoding.md). Every mutation
//! returns a **new** map that shares structure with the old one — the old root is provably
//! unchanged — so a snapshot is an O(1) `Arc` clone and a commit is a pointer swap
//! (transactions.md §2).
//!
//! **This is the on-disk B+tree, node-for-page (v24).** Records live **only in leaves**; an
//! interior node is a record-free routing skeleton — separator keys + child pointers. A separator
//! is a **copy of a boundary key** (a leaf split copies the right half's first key up; an interior
//! split pushes its median separator up) and may go stale after deletes — it keeps routing
//! (left < sep ≤ right holds forever). Fan-out is **size-driven**: a node holds as many entries as
//! fit a page payload `cap` (= `page_size − 16`) and **splits when it would overflow** — so the
//! node boundaries, and therefore the serialized bytes, are a §8 byte contract (format.md). The
//! caller supplies each leaf entry's on-disk **weight** (its record size) so this map can sum leaf
//! payloads without knowing the value codec; interior payloads come from the separators
//! themselves. `cap` and the leaf's column-class `shape` are passed per call (properties of the
//! database's page size and the table's column types, held by [`crate::storage::TableStore`]).
//!
//! Each [`Node`] also carries a set-once on-disk **page id** (`0` = dirty/unpersisted): an
//! incremental commit writes only the dirty nodes a mutation introduced (format.rs / file.rs).
//! Copy-on-write builds every new node dirty; a node persisted once is never rewritten while it
//! stays shared. `AtomicU32` keeps the shared tree `Send + Sync` (P5.3b) under a relaxed set-once
//! store — the node is otherwise immutable.
//!
//! Boring and explicit (CLAUDE.md §10): one `Node` type (a leaf has no children; an interior has
//! no vals/weights), recursive insert with split-on-overflow (leaf copy-up / interior push-up,
//! format.md "Fan-out"), recursive delete with **merge-then-maybe-split** rebalancing (no borrow —
//! merge subsumes it; an interior merge whose result cannot 2-way split is **abandoned**,
//! format.md "Delete").

use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use crate::error::Result;
use crate::format::{LeafShape, PackedLeaf, leaf_overhead};
use crate::storage::Row;
use crate::value::Value;

/// A B+tree node's reference to one child. Under demand paging (P6.4b, spec/design/pager.md §4) a
/// clean leaf need not be resident: an interior node keeps `OnDisk(page_id)` for such a child and
/// the read path faults it through the buffer pool on access. A `Resident` child is an in-memory
/// node — a dirty/uncommitted node, a resident interior skeleton node (interior nodes are *always*
/// resident, §1), or a leaf currently materialized. Because only **leaves** are paged, an `OnDisk`
/// child is always a leaf — which is exactly what lets `node_count` (cost §5) be computed without
/// loading any leaf. Since B3 every host demand-pages — an in-memory database's committed
/// leaves demote to `OnDisk` children too, faulting back through its pinned pool.
#[derive(Clone)]
pub(crate) enum Child {
    Resident(Arc<Node>),
    OnDisk(u32),
}

impl Child {
    /// The resident node behind this child. For the fully-resident paths only — interior children
    /// (always resident, §1) and in-memory databases. The read/mutation path resolves a possibly-
    /// `OnDisk` child through [`child`] (which faults via the pool); panicking on `OnDisk` here would
    /// be a paging bug, never reachable for a fully-resident tree.
    fn resident(&self) -> &Arc<Node> {
        match self {
            Child::Resident(n) => n,
            Child::OnDisk(p) => unreachable!("OnDisk child page {p} accessed without faulting"),
        }
    }
}

/// Source for faulting a clean **leaf** page to a resident node on demand (spec/design/pager.md §4) —
/// the buffer pool, behind the table's column types. Defined here so the B+tree traversal can fault
/// without depending on the storage/format layers (they implement it); a fully-resident in-memory
/// database passes `None` and never faults.
pub(crate) trait LeafSource {
    fn load_leaf(&self, page: u32) -> Result<Arc<Node>>;
}

/// Resolve child `i` to a resident node, faulting an `OnDisk` leaf through `src` (the buffer pool).
/// A `Resident` child returns its `Arc` directly (a cheap bump); an `OnDisk` leaf with no source is a
/// bug — an in-memory tree constructs no `OnDisk` child, and every file-backed traversal supplies one.
fn child(node: &Node, i: usize, src: Option<&dyn LeafSource>) -> Result<Arc<Node>> {
    match &node.children[i] {
        Child::Resident(n) => Ok(n.clone()),
        Child::OnDisk(p) => match src {
            Some(s) => s.load_leaf(*p),
            // An `OnDisk` child only exists in a file-backed store, which always supplies a source —
            // an internal wiring invariant, not a data or user condition, so this is unreachable.
            None => unreachable!("demand-paged leaf {p} reached with no buffer-pool source"),
        },
    }
}

/// One B+tree node. A **leaf** has no children and `keys.len() == vals.len() == weights.len()`
/// (or a `packed` block in place of `vals`). An **interior** node has `children.len() ==
/// keys.len() + 1` and **empty** `vals`/`weights` — its keys are the routing separators, its
/// payload is derived from the separator bytes themselves (v24, record-free). Nodes are shared
/// behind `Arc`; a mutation clones only the root→leaf path and shares every untouched subtree.
pub(crate) struct Node {
    pub(crate) keys: Vec<Vec<u8>>,
    /// The decoded value rows, one per key — populated for a **Decoded leaf** (a writer's
    /// transient materialize-mutate-repack buffer; the post-commit residency flip demotes it once
    /// persisted, so Decoded survives a commit only in a root leaf, a GiST leaf-key store, or a
    /// bare scratch engine), **empty** for a **Packed** leaf (which reconstructs on demand from
    /// `packed`) and for every **interior** node (record-free, v24). Read only through the
    /// [`row_at`](Node::row_at) / [`col_at`](Node::col_at) / [`with_row`](Node::with_row) /
    /// [`decoded_rows`](Node::decoded_rows) seam on leaves, never indexed directly.
    pub(crate) vals: Vec<Row>,
    /// Each leaf record's on-disk size (`format::record_size`) — the size-driven split weight.
    /// Empty for interior nodes.
    pub(crate) weights: Vec<u32>,
    pub(crate) children: Vec<Child>,
    /// The **Packed** (block-backed) resident form of a demand-paged clean leaf (packed-leaf.md §5):
    /// the page block + the PAX directories, from which `vals` are reconstructed on demand. `None` for
    /// a Decoded node — an in-memory/`from_image` leaf, any dirty (mutated) leaf (mutation
    /// materializes Packed→Decoded first, §7), and every interior node. A Packed leaf is always
    /// clean (`page` ≠ `0`), so it is never serialized.
    pub(crate) packed: Option<PackedLeaf>,
    /// On-disk page index, or `0` when dirty (never persisted / changed since). Set once by the
    /// incremental commit that first persists this node (format.rs `serialize_dirty`, P6.1 part B);
    /// page 0 is a meta slot, never a node, so it doubles as the dirty sentinel. A clean node lets an
    /// incremental commit skip its whole (unchanged) subtree.
    pub(crate) page: AtomicU32,
}

impl Node {
    /// A fresh **dirty leaf** (page `0`) — every copy-on-write leaf rebuild goes through here.
    fn new_leaf(keys: Vec<Vec<u8>>, vals: Vec<Row>, weights: Vec<u32>) -> Arc<Node> {
        Arc::new(Node {
            keys,
            vals,
            weights,
            children: Vec::new(),
            packed: None,
            page: AtomicU32::new(0),
        })
    }

    /// A fresh **dirty interior** node (page `0`) — separators + children, no records (v24).
    fn new_interior(keys: Vec<Vec<u8>>, children: Vec<Child>) -> Arc<Node> {
        debug_assert_eq!(children.len(), keys.len() + 1, "interior child count");
        Arc::new(Node {
            keys,
            vals: Vec::new(),
            weights: Vec::new(),
            children,
            packed: None,
            page: AtomicU32::new(0),
        })
    }

    /// A leaf reconstructed from disk at `page` (format.rs `read_tree`), already persisted and
    /// fully decoded (the in-memory eager load).
    pub(crate) fn loaded(
        keys: Vec<Vec<u8>>,
        vals: Vec<Row>,
        weights: Vec<u32>,
        children: Vec<Child>,
        page: u32,
    ) -> Arc<Node> {
        Arc::new(Node {
            keys,
            vals,
            weights,
            children,
            packed: None,
            page: AtomicU32::new(page),
        })
    }

    /// An interior node reconstructed from disk at `page` (format.rs `read_tree` /
    /// `read_skeleton_node`): the record-free separators + children skeleton (v24). Children may be
    /// `Resident` (the fully-resident in-memory load) or `OnDisk` (the demand-paged skeleton load).
    pub(crate) fn loaded_interior(
        keys: Vec<Vec<u8>>,
        children: Vec<Child>,
        page: u32,
    ) -> Arc<Node> {
        debug_assert_eq!(children.len(), keys.len() + 1, "interior child count");
        Arc::new(Node {
            keys,
            vals: Vec::new(),
            weights: Vec::new(),
            children,
            packed: None,
            page: AtomicU32::new(page),
        })
    }

    /// A **Packed** leaf reconstructed from disk at `page` for the demand-paging fault path
    /// (format.rs `decode_leaf_node`, packed-leaf.md §5). Holds `keys` + `weights` (both derivable
    /// from the PAX directories with no value decode, §3) and the `packed` block; `vals` is **empty**
    /// and rows are reconstructed on demand through the accessor seam. Returns the bare `Node` — the
    /// buffer pool wraps it in an `Arc` (paging.rs). A leaf has no children.
    pub(crate) fn leaf_loaded_packed(
        keys: Vec<Vec<u8>>,
        weights: Vec<u32>,
        packed: PackedLeaf,
        page: u32,
    ) -> Node {
        Node {
            keys,
            vals: Vec::new(),
            weights,
            children: Vec::new(),
            packed: Some(packed),
            page: AtomicU32::new(page),
        }
    }

    pub(crate) fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }

    /// This node's serialized payload size (format.md): a leaf is `Σ weights +
    /// leaf_overhead(N, shape)`; an interior node is `8·N + 4 + Σ sep_len` (child pointers +
    /// separator directory + key blob — record-free, v24).
    fn payload(&self, shape: LeafShape) -> usize {
        if self.is_leaf() {
            self.weights.iter().map(|&w| w as usize).sum::<usize>()
                + leaf_overhead(self.keys.len(), shape)
        } else {
            8 * self.keys.len() + 4 + self.keys.iter().map(Vec::len).sum::<usize>()
        }
    }

    /// Binary-search a **leaf**'s keys: `Ok(i)` if `key` sits at index `i`, else `Err(i)` for the
    /// insertion slot. `Vec<u8>::cmp` is lexicographic (memcmp) — the key contract.
    fn search(&self, key: &[u8]) -> std::result::Result<usize, usize> {
        self.keys.binary_search_by(|k| k.as_slice().cmp(key))
    }

    /// The child an **interior** descent takes for `key`: `partition_point(sep ≤ key)` — a key
    /// equal to a separator lies in the **right** subtree (the copy-up separator is the right
    /// half's first key; format.md "Interior node").
    fn child_slot(&self, key: &[u8]) -> usize {
        self.keys.partition_point(|k| k.as_slice() <= key)
    }

    /// Reconstruct value row `i` as an owned [`Row`] — the value-read seam (packed-leaf.md §4), on
    /// a **leaf**. A **Decoded** leaf clones `vals[i]`; a **Packed** leaf reconstructs the whole row
    /// from the retained PAX directories on demand. Fallible so the Packed reconstruction can
    /// surface a corrupt *touched* inline body (`XX001`, packed-leaf.md §8); the Decoded path never
    /// errors.
    pub(crate) fn row_at(&self, i: usize) -> Result<Row> {
        match &self.packed {
            None => Ok(self.vals[i].clone()),
            Some(p) => p.row(i),
        }
    }

    /// Reconstruct **only** column `c` of row `i` — the touched-column path (packed-leaf.md §4/§6,
    /// the `OP_Column`/`slot_getsomeattrs` model PAX's column regions make O(1)). A **Decoded** leaf
    /// clones `vals[i][c]`; a **Packed** leaf decodes the single column span and reads no other column.
    pub(crate) fn col_at(&self, i: usize, c: usize) -> Result<Value> {
        match &self.packed {
            None => Ok(self.vals[i][c].clone()),
            Some(p) => p.value(c, i),
        }
    }

    /// Borrow leaf row `i` for the duration of `f`, avoiding an owned clone on the Decoded hot
    /// path (the scan/visit callbacks that only need a `&Row`) — the "materialize-then-lend"
    /// borrow helper (packed-leaf.md §4). The old two-form masked/unmasked reconstruction seam is
    /// **collapsed** (bplus-reshape.md B4): a Packed leaf's reconstruction is uniformly lazy
    /// (fixed-width columns decode eagerly, variable columns defer as self-resolving `Unfetched`),
    /// so a reconstruction mask no longer exists — the query's touched set survives as the cost
    /// basis + the scan layer's resolve prefetch, and a missed value resolves on touch (the
    /// demand-fault backstop).
    fn with_row<R>(&self, i: usize, f: impl FnOnce(&Row) -> Result<R>) -> Result<R> {
        match &self.packed {
            None => f(&self.vals[i]),
            Some(p) => f(&p.row(i)?),
        }
    }

    /// Every value row of a **leaf**, owned — the mutation-descent materialization
    /// (packed-leaf.md §7). A **Decoded** leaf clones `vals`; a **Packed** leaf reconstructs every
    /// row so the rebuilt node is Decoded (`build_leaf`/`node_insert`/`node_remove`/`merge_at` then
    /// run unchanged).
    pub(crate) fn decoded_rows(&self) -> Result<Vec<Row>> {
        match &self.packed {
            None => Ok(self.vals.clone()),
            Some(p) => (0..self.keys.len()).map(|i| p.row(i)).collect(),
        }
    }
}

/// The result of inserting into a subtree: either the rebuilt subtree, or a node that overflowed
/// and split into `left`, a **separator key** for the parent, and `right`. A leaf split **copies**
/// the right leaf's first key up (no record leaves the leaf level); an interior split **pushes**
/// its median separator up (format.md "Fan-out").
enum Ins {
    Whole(Arc<Node>),
    Split {
        left: Arc<Node>,
        sep: Vec<u8>,
        right: Arc<Node>,
    },
}

/// The kind-shared split decision (format.md "Split point"): given the per-boundary
/// `leftpayload`/`rightpayload` functions over `m` in `[m_lo, m_hi]`, pick
/// `m = right_edge ? m_max : clamp(min(m_balanced, m_max), m_min, m_max)`, or `None` when no `m`
/// in the range keeps both sides fitting (the interior merge-abandon case — unreachable on the
/// insert path, format.md "Why the record cap").
fn split_point(
    m_lo: usize,
    m_hi: usize,
    payload: usize,
    cap: usize,
    right_edge: bool,
    leftpayload: impl Fn(usize) -> usize,
    rightpayload: impl Fn(usize) -> usize,
) -> Option<usize> {
    debug_assert!(m_lo <= m_hi);
    // leftpayload is nondecreasing in m and rightpayload nonincreasing, so both bounds
    // binary-search cleanly; the ranges are tiny (page fan-out), so a linear scan is clearer.
    let mut m_max = None;
    for m in m_lo..=m_hi {
        if leftpayload(m) <= cap {
            m_max = Some(m);
        } else {
            break;
        }
    }
    let m_max = m_max?;
    let mut m_min = None;
    for m in (m_lo..=m_hi).rev() {
        if rightpayload(m) <= cap {
            m_min = Some(m);
        } else {
            break;
        }
    }
    let m_min = m_min?;
    if m_min > m_max {
        return None;
    }
    if right_edge {
        return Some(m_max);
    }
    let mut m_balanced = m_max;
    for m in m_lo..=m_hi {
        if 2 * leftpayload(m) >= payload {
            m_balanced = m;
            break;
        }
    }
    Some(m_balanced.min(m_max).max(m_min))
}

/// Build a leaf from its parts; if its payload overflows `cap`, split it 2-way **copy-up**
/// (format.md "Leaf split"): the left leaf keeps records `[0, m)`, the right leaf `[m, N)`, and
/// the separator handed up is a **copy of `keys[m]`** (the right leaf's first key). `edited` is
/// the index of the just-inserted/replaced record (`None` for the delete path's merge-overflow,
/// which splits balanced). A leaf with a single over-cap record is left whole (defensive — the
/// oversize surfaces as `0A000` when serialized).
fn build_leaf(
    keys: Vec<Vec<u8>>,
    vals: Vec<Row>,
    weights: Vec<u32>,
    cap: usize,
    shape: LeafShape,
    edited: Option<usize>,
) -> Ins {
    let n = keys.len();
    let payload: usize =
        weights.iter().map(|&w| w as usize).sum::<usize>() + leaf_overhead(n, shape);
    if payload <= cap || n < 2 {
        return Ins::Whole(Node::new_leaf(keys, vals, weights));
    }
    let prefix: Vec<usize> = std::iter::once(0)
        .chain(weights.iter().scan(0usize, |acc, &w| {
            *acc += w as usize;
            Some(*acc)
        }))
        .collect();
    let total = prefix[n];
    let leftpayload = |m: usize| prefix[m] + leaf_overhead(m, shape);
    let rightpayload = |m: usize| (total - prefix[m]) + leaf_overhead(n - m, shape);
    let right_edge = edited == Some(n - 1);
    let m = match split_point(
        1,
        n - 1,
        payload,
        cap,
        right_edge,
        leftpayload,
        rightpayload,
    ) {
        Some(m) => m,
        // Unreachable under the RECORD_MAX cap (a two-record leaf always fits — format.md "Why
        // the record cap"); defensively leave the node whole (0A000 at serialize).
        None => return Ins::Whole(Node::new_leaf(keys, vals, weights)),
    };

    let mut keys = keys;
    let mut vals = vals;
    let mut weights = weights;
    let rkeys = keys.split_off(m);
    let rvals = vals.split_off(m);
    let rweights = weights.split_off(m);
    let sep = rkeys[0].clone();
    Ins::Split {
        left: Node::new_leaf(keys, vals, weights),
        sep,
        right: Node::new_leaf(rkeys, rvals, rweights),
    }
}

/// Build an interior node from its parts; if its payload overflows `cap`, split it 2-way
/// **push-up** (format.md "Interior split"): the left node keeps separators `[0, m)` + children
/// `[0, m]`, separator `m` moves up, the right node keeps `[m+1, N)` + children `[m+1, N]`. With
/// `N = 2` (only reachable with near-cap separators) the split is pinned to `m = 1`, producing a
/// legal `N = 0` right interior (the degenerate fan-out contract). Returns `None` when the node
/// overflows and no valid split point exists — the caller (only the interior **merge** path can
/// hit it) abandons the merge.
fn build_interior(
    keys: Vec<Vec<u8>>,
    children: Vec<Child>,
    cap: usize,
    edited: Option<usize>,
) -> Option<Ins> {
    let n = keys.len();
    let payload = 8 * n + 4 + keys.iter().map(Vec::len).sum::<usize>();
    if payload <= cap || n < 2 {
        return Some(Ins::Whole(Node::new_interior(keys, children)));
    }
    let m = if n == 2 {
        // The degenerate pin (format.md "Interior split"): the left keeps sep[0] (fits, by the
        // minimum-fanout invariant), sep[1] moves up, the right is the legal N = 0 interior.
        1
    } else {
        let prefix: Vec<usize> = std::iter::once(0)
            .chain(keys.iter().scan(0usize, |acc, k| {
                *acc += k.len();
                Some(*acc)
            }))
            .collect();
        let total = prefix[n];
        let leftpayload = |m: usize| 8 * m + 4 + prefix[m];
        let rightpayload = |m: usize| 8 * (n - 1 - m) + 4 + (total - prefix[m + 1]);
        let right_edge = edited == Some(n - 1);
        split_point(
            1,
            n - 2,
            payload,
            cap,
            right_edge,
            leftpayload,
            rightpayload,
        )?
    };

    let mut keys = keys;
    let mut children = children;
    let rkeys = keys.split_off(m + 1);
    let sep = keys.pop().expect("split point m ≥ 1 leaves a separator");
    let rchildren = children.split_off(m + 1);
    Some(Ins::Split {
        left: Node::new_interior(keys, children),
        sep,
        right: Node::new_interior(rkeys, rchildren),
    })
}

/// A persistent ordered map from encoded key to [`Row`]. `Clone` is O(1) (an `Arc` bump on the root
/// plus a length copy) and yields an independent snapshot: mutating the clone leaves this map
/// untouched.
///
/// `count` is the exact row count **when known** (`Some`): a map built from empty by insert/remove
/// maintains it for free. A map loaded from a disk skeleton (`from_skeleton`) carries `None` —
/// **unknown** — because the count would cost a full leaf walk to compute and nothing needs it
/// eagerly (open reads only the interior spine now, spec/design/storage.md §6). `is_empty` never
/// consults it: it derives emptiness from the root (an empty map has no root), which is exact and
/// O(1) whether or not the count is known.
#[derive(Clone, Default)]
pub struct PMap {
    root: Option<Arc<Node>>,
    count: Option<usize>,
}

/// A contiguous range of encoded keys — the form a primary-key predicate pushes down to a bounded
/// B+tree scan (spec/design/cost.md §3 "bounded scan / point lookup", encoding.md). `lo`/`hi` are
/// encoded key bytes; `None` is open on that side (−∞ / +∞), and the `_inc` flags say whether the
/// endpoint key itself is included. Because the key encoding is order-preserving (`[u8]::cmp` = value
/// order), a byte range is a value range. A bounded scan visits exactly the nodes whose key span
/// intersects this bound, so its `page_read` cost is proportional to what it touches, not the whole
/// tree — and the unbounded bound (−∞..+∞) degenerates to the full scan, so existing full-scan costs
/// do not move (overlap_node_count then equals node_count).
#[derive(Default)]
pub(crate) struct KeyBound {
    pub(crate) lo: Option<Vec<u8>>,
    pub(crate) lo_inc: bool,
    pub(crate) hi: Option<Vec<u8>>,
    pub(crate) hi_inc: bool,
}

impl KeyBound {
    /// The full-table bound (−∞..+∞): every node overlaps it, reproducing the full scan exactly.
    pub(crate) fn unbounded() -> Self {
        KeyBound::default()
    }

    /// The contiguous window `[first ..= last]` of an **interior** node's child indices whose key
    /// span can overlap the bound. Child `i` spans `[sep[i−1], sep[i])` (v24 — a key equal to a
    /// separator lies right), so child `i` is pruned iff `sep[i] ≤ lo` (entirely at/below lo) or
    /// `sep[i−1]` is at/above hi — `> hi` for an inclusive hi (a child whose low separator equals
    /// `hi` can still hold `hi` itself), `≥ hi` for an exclusive one. The separators are sorted, so
    /// the surviving children are contiguous and both edges binary-search. `range_entries`
    /// (descends) and `overlap_node_count` (counts) window identically, so they visit the SAME node
    /// set — the §8 determinism the `page_read` cost depends on — decided from resident separators
    /// WITHOUT faulting an OnDisk leaf.
    fn child_window(&self, node: &Node) -> (usize, usize) {
        let first = match &self.lo {
            None => 0,
            Some(lo) => node.keys.partition_point(|k| k.as_slice() <= lo.as_slice()),
        };
        let last = match &self.hi {
            None => node.keys.len(),
            Some(hi) if self.hi_inc => node.keys.partition_point(|k| k.as_slice() <= hi.as_slice()),
            Some(hi) => node.keys.partition_point(|k| k.as_slice() < hi.as_slice()),
        };
        (first, last.max(first))
    }

    /// The contiguous half-open window `[first .. last)` of a **leaf**'s record indices whose keys
    /// lie within the bound — the binary-searched equivalent of testing `contains` per key,
    /// honoring the endpoint inclusivity flags.
    fn entry_window(&self, node: &Node) -> (usize, usize) {
        let first = match &self.lo {
            None => 0,
            Some(lo) if self.lo_inc => node.keys.partition_point(|k| k.as_slice() < lo.as_slice()),
            Some(lo) => node.keys.partition_point(|k| k.as_slice() <= lo.as_slice()),
        };
        let last = match &self.hi {
            None => node.keys.len(),
            Some(hi) if self.hi_inc => node.keys.partition_point(|k| k.as_slice() <= hi.as_slice()),
            Some(hi) => node.keys.partition_point(|k| k.as_slice() < hi.as_slice()),
        };
        (first, last.max(first))
    }
}

impl PMap {
    pub fn new() -> Self {
        PMap {
            root: None,
            count: Some(0),
        }
    }

    /// The exact row count, or `None` when unknown (a disk-loaded skeleton — see the struct doc).
    /// Callers that only need a capacity hint use `.unwrap_or(0)`; callers that need an exact count
    /// on a disk-loaded map must scan (nothing in the engine does — the count is near-vestigial).
    pub fn count(&self) -> Option<usize> {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// The root node, for the serializer (format.rs). `None` for an empty map.
    pub(crate) fn root(&self) -> Option<&Arc<Node>> {
        self.root.as_ref()
    }

    /// Reconstruct a map from a disk-loaded skeleton root (format.rs `read_skeleton`). The count is
    /// **unknown** — open no longer walks the leaves to sum it (spec/design/storage.md §6).
    pub(crate) fn from_skeleton(root: Option<Arc<Node>>) -> Self {
        PMap { root, count: None }
    }

    /// Look up the row at `key`, or `None` — a root→leaf descent (interior nodes only route, v24).
    /// Returns an **owned** row: under demand paging (P6.4b) the leaf holding it may live only in
    /// the buffer pool, not the resident tree, so a borrow could not outlive the pool lock — the
    /// read path clones the row out (spec/design/pager.md §4). `src` faults an `OnDisk` leaf on the
    /// descent (`None` for a fully-resident in-memory tree).
    pub(crate) fn get(&self, key: &[u8], src: Option<&dyn LeafSource>) -> Result<Option<Row>> {
        // Hold an owned `Arc` to the current node so a faulted leaf outlives the step that reads it.
        let mut cur = match &self.root {
            None => return Ok(None),
            Some(root) => root.clone(),
        };
        while !cur.is_leaf() {
            cur = child(&cur, cur.child_slot(key), src)?;
        }
        match cur.search(key) {
            Ok(i) => Ok(Some(cur.row_at(i)?)),
            Err(_) => Ok(None),
        }
    }

    /// Insert or overwrite `key` with `val` (whose on-disk record size is `weight`); `cap` is the
    /// page payload capacity and `shape` the leaf's column-class shape. Returns the previous row if
    /// `key` was present (an overwrite), else `None` (a new insert, which grows `len`). An
    /// overwrite can change the weight, so it too may overflow and split.
    pub(crate) fn insert(
        &mut self,
        key: Vec<u8>,
        val: Row,
        weight: u32,
        cap: usize,
        shape: LeafShape,
        src: Option<&dyn LeafSource>,
    ) -> Result<Option<Row>> {
        let mut old = None;
        let new_root = match &self.root {
            None => Node::new_leaf(vec![key], vec![val], vec![weight]),
            Some(root) => match node_insert(root, key, val, weight, &mut old, src, cap, shape)? {
                Ins::Whole(n) => n,
                Ins::Split { left, sep, right } => Node::new_interior(
                    vec![sep],
                    vec![Child::Resident(left), Child::Resident(right)],
                ),
            },
        };
        self.root = Some(new_root);
        if old.is_none() {
            // Maintain the count only when it is known (`Some`). A disk-loaded skeleton stays
            // `None` — we never learned the base to increment from, and nothing needs it.
            self.count = self.count.map(|n| n + 1);
        }
        Ok(old)
    }

    /// Remove `key`. Returns the removed row, or `None` if absent (then `self` is unchanged). `src`
    /// faults `OnDisk` leaves the delete descends into / rebalances against (spec/design/pager.md §4).
    pub(crate) fn remove(
        &mut self,
        key: &[u8],
        cap: usize,
        shape: LeafShape,
        src: Option<&dyn LeafSource>,
    ) -> Result<Option<Row>> {
        let root = match self.root.as_ref() {
            None => return Ok(None),
            Some(r) => r.clone(),
        };
        let (new_root, removed) = node_remove(&root, key, src, cap, shape)?;
        if removed.is_some() {
            // The root may have drained: an empty leaf becomes the empty map; a 0-key interior
            // root hands the root down a level (height shrinks). The root is exempt from the
            // underfull rule, so no rebalance here.
            self.root = if new_root.keys.is_empty() {
                if new_root.is_leaf() {
                    None
                } else {
                    // The lone surviving child becomes the new root — fault it if it is an OnDisk
                    // leaf (a tree of height 2 can collapse to its single bottom child).
                    Some(child(&new_root, 0, src)?)
                }
            } else {
                Some(new_root)
            };
            self.count = self.count.map(|n| n - 1);
        }
        Ok(removed)
    }

    /// Demote every **clean, persisted** resident leaf to its `Child::OnDisk(page)` reference —
    /// the post-commit residency flip (bplus-reshape.md B4): after a commit assigns page ids to the
    /// dirty nodes it wrote, the committed tree sheds its leaf payloads and becomes the skeletal
    /// `interior nodes + OnDisk leaves` shape every load already produces, so reads everywhere go
    /// through the one Packed pool path and `Decoded` survives only inside an uncommitted writer.
    /// A **root** leaf stays resident (the `PMap` root is always a node — the open/load convention);
    /// an unpersisted (page 0) leaf is left alone (defensive — a bare scratch engine that never
    /// persists). Rebuilds only the interior spine above changed children; an unchanged subtree
    /// keeps its `Arc` (and its set-once page id), so the flip is O(interior nodes) and the flipped
    /// tree stays clean for the next incremental commit.
    pub(crate) fn demote_clean_leaves(&mut self) {
        fn demote(node: &Arc<Node>) -> Option<Arc<Node>> {
            if node.is_leaf() {
                return None; // handled by the parent (a root leaf stays resident)
            }
            let mut changed = false;
            let mut children = Vec::with_capacity(node.children.len());
            for c in &node.children {
                let new_child = match c {
                    Child::OnDisk(p) => Child::OnDisk(*p),
                    Child::Resident(n) => {
                        if n.is_leaf() {
                            let page = n.page.load(std::sync::atomic::Ordering::Acquire);
                            if page != 0 {
                                changed = true;
                                Child::OnDisk(page)
                            } else {
                                Child::Resident(n.clone())
                            }
                        } else {
                            match demote(n) {
                                Some(rebuilt) => {
                                    changed = true;
                                    Child::Resident(rebuilt)
                                }
                                None => Child::Resident(n.clone()),
                            }
                        }
                    }
                };
                children.push(new_child);
            }
            if !changed {
                return None;
            }
            // The rebuilt interior keeps its keys AND its page id — its serialized bytes are
            // unchanged (children reference the same pages), so it must stay clean or the next
            // incremental commit would rewrite the whole spine every time.
            let page = node.page.load(std::sync::atomic::Ordering::Acquire);
            Some(Node::loaded_interior(node.keys.clone(), children, page))
        }
        if let Some(root) = &self.root {
            if let Some(rebuilt) = demote(root) {
                self.root = Some(rebuilt);
            }
        }
    }

    /// The number of B+tree nodes (pages) in this tree — the `page_read` count a full scan
    /// charges (spec/design/cost.md §3 "page_read"). A scan walks every node, so this is the
    /// structural node count (interior + leaf); `0` for an empty map. Deterministic and
    /// byte-identical across cores (the node boundaries are a §8 byte contract — format.md).
    pub fn node_count(&self) -> usize {
        fn count(node: &Node) -> usize {
            1 + node
                .children
                .iter()
                .map(|c| match c {
                    // A resident child is counted recursively; an `OnDisk` child is a clean **leaf**
                    // (only leaves page — pager.md §1/§4), so it contributes exactly one node and is
                    // counted *without loading it* — the dividend of the resident interior skeleton
                    // that keeps cost (§5) identical to P6.3.
                    Child::Resident(n) => count(n),
                    Child::OnDisk(_) => 1,
                })
                .sum::<usize>()
        }
        self.root.as_deref().map(count).unwrap_or(0)
    }

    /// Total on-disk record bytes stored in this tree — the sum of every leaf entry's `weight`
    /// (records live only in leaves, v24). The deterministic, cross-core-identical measure of a
    /// temp table's storage footprint (spec/design/temp-tables.md §7; `weight` is
    /// `format::record_size`, the byte-identical on-disk encoding size — §8). The tree is fully
    /// resident for a temp store (temp data never pages), so this never faults; an `OnDisk` child
    /// would contribute 0 (defensive — temp stores have none).
    pub(crate) fn resident_record_bytes(&self) -> u64 {
        fn walk(node: &Node) -> u64 {
            let here: u64 = node.weights.iter().map(|&w| w as u64).sum();
            let kids: u64 = node
                .children
                .iter()
                .map(|c| match c {
                    Child::Resident(n) => walk(n),
                    Child::OnDisk(_) => 0,
                })
                .sum();
            here + kids
        }
        self.root.as_deref().map(walk).unwrap_or(0)
    }

    /// Iterate `(key, row)` pairs in ascending key order, yielding **owned** pairs. Eagerly walks
    /// the tree into a vector (the cost contract charges per row in the executor loop, not here —
    /// cost.md). Owned, not borrowed, because under demand paging (P6.4b) a leaf may be faulted in
    /// from the buffer pool only for the duration of this walk: the row is cloned out and the leaf
    /// node is free to be evicted, so the resident *node* set stays bounded by the pool even though
    /// the executor materializes the rows it scans (streaming the rows themselves is a deferred,
    /// out-of-scope follow-on — spec/design/pager.md §4/§6).
    pub(crate) fn iter(&self, src: Option<&dyn LeafSource>) -> Result<Vec<(Vec<u8>, Row)>> {
        let mut out = Vec::with_capacity(self.count.unwrap_or(0));
        if let Some(root) = &self.root {
            collect(root, src, &mut out)?;
        }
        Ok(out)
    }

    /// `(key, row)` pairs whose key lies within the bound, in ascending key order — a bounded
    /// in-order traversal that prunes a child subtree whose separator span cannot overlap the bound
    /// ([`KeyBound::child_window`]), so only overlapping leaves fault through `src`. The unbounded
    /// bound walks the whole tree (identical to [`iter`]). Owned pairs, like `iter`, so a faulted leaf
    /// can be evicted after the walk.
    pub(crate) fn range_entries(
        &self,
        b: &KeyBound,
        src: Option<&dyn LeafSource>,
    ) -> Result<Vec<(Vec<u8>, Row)>> {
        Ok(self.range_entries_counted(b, src)?.0)
    }

    /// [`range_entries`](PMap::range_entries) plus the number of B+tree nodes the bounded traversal
    /// visits — the `page_read` count [`overlap_node_count`](PMap::overlap_node_count) would return,
    /// observed during the ONE windowed walk instead of a second counting descent (the visited sets
    /// are identical by construction: both window with [`KeyBound::child_window`]).
    pub(crate) fn range_entries_counted(
        &self,
        b: &KeyBound,
        src: Option<&dyn LeafSource>,
    ) -> Result<(Vec<(Vec<u8>, Row)>, usize)> {
        let mut out = Vec::new();
        let mut nodes = 0usize;
        if let Some(root) = &self.root {
            collect_range(root, b, src, &mut out, &mut nodes)?;
        }
        Ok((out, nodes))
    }

    /// Walk the bounded scan gathering ONLY the columns `mask` selects into dense per-column lanes
    /// (`cols[c]` of length `row_count` for each selected `c`, empty otherwise), never building a
    /// full-width [`Row`] — the A2/A3 columnar-gather feed (packed-leaf.md §11 Track A2, the allocation
    /// dividend A1 leaves on the table). It mirrors [`range_entries_counted`]'s traversal EXACTLY (same
    /// node visits ⇒ the same `page_read` count; same in-order record sequence — leaf-only, v24), but
    /// reads each admitted row's selected columns via [`Node::col_at`] — an O(1) PAX column span on a
    /// Packed leaf, `vals[i][c]` on a Decoded leaf — so a wide-table single-column scan never
    /// materializes the untouched columns NOR a full-width row. Each `cols[c]` is in scan order, so it
    /// equals the column-`c` stride of the row feed. Returns `(cols, row_count, nodes)`.
    pub(crate) fn columnar_scan(
        &self,
        b: &KeyBound,
        src: Option<&dyn LeafSource>,
        mask: &[bool],
    ) -> Result<(Vec<Vec<Value>>, usize, usize)> {
        let mut cols: Vec<Vec<Value>> = vec![Vec::new(); mask.len()];
        let mut row_count = 0usize;
        let mut nodes = 0usize;
        if let Some(root) = &self.root {
            columnar_collect(root, b, src, mask, &mut cols, &mut row_count, &mut nodes)?;
        }
        Ok((cols, row_count, nodes))
    }

    /// The fold-during-walk twin of [`columnar_scan`](PMap::columnar_scan) (packed-leaf.md §11): the
    /// same windowed walk (identical visited-node set → identical `page_read`), but calls
    /// `visit(node, i)` per admitted leaf record — which reads only the row's touched columns via
    /// [`Node::col_at`] and folds them straight into an accumulator — instead of gathering a
    /// per-column lane. So a whole-table / single-int-key aggregate is O(1) memory, never O(rows).
    /// Returns `(row_count, node_count)`, identical to `columnar_scan`, so the caller charges the same
    /// `page_read` / `storage_row_read`.
    pub(crate) fn fold_scan(
        &self,
        b: &KeyBound,
        src: Option<&dyn LeafSource>,
        visit: &mut dyn FnMut(&Node, usize) -> Result<()>,
    ) -> Result<(usize, usize)> {
        let mut row_count = 0usize;
        let mut nodes = 0usize;
        if let Some(root) = &self.root {
            fold_walk(root, b, src, visit, &mut row_count, &mut nodes)?;
        }
        Ok((row_count, nodes))
    }

    /// The number of B+tree nodes a bounded scan over `b` visits — the `page_read` it charges
    /// (cost.md §3). Mirrors `range_entries`' traversal exactly (same `child_window` prune, root
    /// always visited), counting an `OnDisk` leaf as one node WITHOUT faulting it (pager.md §5). The
    /// unbounded bound returns `node_count()`, so a full scan's cost is unchanged.
    pub(crate) fn overlap_node_count(&self, b: &KeyBound) -> usize {
        fn count(node: &Node, b: &KeyBound) -> usize {
            if node.is_leaf() {
                return 1;
            }
            let mut total = 1;
            let (first, last) = b.child_window(node);
            for i in first..=last {
                match &node.children[i] {
                    Child::Resident(n) => total += count(n, b),
                    Child::OnDisk(_) => total += 1,
                }
            }
            total
        }
        self.root.as_deref().map(|r| count(r, b)).unwrap_or(0)
    }

    /// Visit the `(key, row)` pairs within the bound, in ascending key order, calling `visit` per
    /// in-bound row. `visit` returns `Ok(false)` to STOP the traversal — and because a leaf is faulted
    /// only when descended into, leaves past the stop point are never faulted (the genuine LIMIT
    /// short-circuit — spec/design/cost.md §3 "LIMIT short-circuit"). Streams one row at a time (no
    /// `Vec`), so a bounded result holds ~one leaf resident.
    pub(crate) fn scan_range(
        &self,
        b: &KeyBound,
        src: Option<&dyn LeafSource>,
        visit: &mut dyn FnMut(&[u8], &Row) -> Result<bool>,
    ) -> Result<()> {
        if let Some(root) = &self.root {
            walk_range_visit(root, b, src, visit)?;
        }
        Ok(())
    }

    /// Like [`scan_range`](PMap::scan_range) but visits the in-bound rows in **descending** key
    /// order — the exact reverse of the forward traversal's row sequence — for a `DESC` reverse
    /// scan (spec/design/cost.md §3 "ORDER BY satisfied by primary-key order"). It windows with the
    /// same `child_window`/`entry_window` prune (so the visited-node set and `page_read` cost are
    /// identical), and stops the moment `visit` returns `Ok(false)` without faulting leaves past the
    /// stop point — so a reverse top-N faults from the high end.
    pub(crate) fn scan_range_rev(
        &self,
        b: &KeyBound,
        src: Option<&dyn LeafSource>,
        visit: &mut dyn FnMut(&[u8], &Row) -> Result<bool>,
    ) -> Result<()> {
        if let Some(root) = &self.root {
            walk_range_visit_rev(root, b, src, visit)?;
        }
        Ok(())
    }

    /// A **pull** cursor over the `(key, row)` pairs within `b`, in ascending (`reverse = false`) or
    /// descending (`reverse = true`) key order — the pull-model equivalent of
    /// [`scan_range`](PMap::scan_range) / [`scan_range_rev`](PMap::scan_range_rev) (the S2 pull
    /// B+tree scan cursor, spec/design/streaming.md §3/§5). It owns the moved `b` and the `Arc<Node>`
    /// frames it descends into, so it borrows nothing and is `'static` — the leaf source is supplied
    /// **per [`next`](RangeCursor::next) call** (rebuilt cheaply by the caller from its own paging
    /// context, storage.rs), which lets a streaming cursor own its snapshot and outlive the handle
    /// (streaming.md §5). The first node on the stack is the root (always resident). See
    /// [`RangeCursor`].
    pub(crate) fn range_cursor(&self, b: KeyBound, reverse: bool) -> RangeCursor {
        let mut stack = Vec::new();
        if let Some(root) = &self.root {
            stack.push(ScanFrame::new(root.clone(), &b));
        }
        RangeCursor {
            stack,
            bound: b,
            reverse,
        }
    }
}

/// One node on a [`RangeCursor`]'s explicit traversal stack: the node and the half-open span
/// `[lo, hi)` of positions still to process. A **leaf**'s positions are its in-bound record
/// indices (its entry window). An **interior** node's positions are its overlapping child indices
/// (its child window) — interior nodes emit nothing (records are leaf-only, v24), so the frame
/// only descends. Reversal consumes `[lo, hi)` from the back, with no separate forward/reverse
/// logic.
struct ScanFrame {
    node: Arc<Node>,
    is_leaf: bool,
    lo: usize,
    hi: usize,
}

impl ScanFrame {
    fn new(node: Arc<Node>, b: &KeyBound) -> ScanFrame {
        let (lo, hi) = if node.is_leaf() {
            b.entry_window(&node)
        } else {
            let (cf, cl) = b.child_window(&node);
            (cf, cl + 1)
        };
        ScanFrame {
            is_leaf: node.is_leaf(),
            node,
            lo,
            hi,
        }
    }
}

/// A **pull** (stateful) cursor over a [`PMap`]'s `(key, row)` pairs within a [`KeyBound`] — the
/// pull-model equivalent of [`PMap::scan_range`] (spec/design/streaming.md §3/§5, the S2 pull
/// B+tree scan cursor). Where `scan_range` *pushes* each row to a `visit` callback and owns the
/// control flow, this cursor lets the **caller** own it: each [`next`](RangeCursor::next) yields the
/// next in-bound pair, advancing an explicit frame stack over the persistent map. That is the
/// VDBE-forward shape (streaming.md §3): a stateful `OP_Next`/`OP_Rewind`-style cursor a future
/// bytecode VM can drive, where a push callback cannot.
///
/// It yields the **exact same sequence** as `scan_range` (`reverse = false`) / `scan_range_rev`
/// (`reverse = true`) — same rows, same order, faulting a clean leaf through `src` only when the
/// traversal descends into it, so a caller that stops pulling early (drops the cursor) faults no
/// leaves past where it stopped (the genuine LIMIT short-circuit, cost.md §3). It clones each
/// `(key, row)` out (owned), like [`PMap::iter`], because under demand paging a faulted leaf may be
/// evicted between `next` calls (pager.md §4).
pub(crate) struct RangeCursor {
    stack: Vec<ScanFrame>,
    bound: KeyBound,
    reverse: bool,
}

impl RangeCursor {
    /// The next in-bound `(key, row)` pair, or `None` when the traversal is exhausted. Each call
    /// advances the frame stack until it emits a leaf row, descends into (and faults `src`) a child,
    /// or pops an exhausted frame. `src` is supplied per call (rebuilt cheaply by the caller) so the
    /// cursor itself borrows nothing — see [`RangeCursor`].
    pub(crate) fn next(&mut self, src: Option<&dyn LeafSource>) -> Result<Option<(Vec<u8>, Row)>> {
        let reverse = self.reverse;
        loop {
            let (emit, descend) = {
                let frame = match self.stack.last_mut() {
                    Some(f) => f,
                    None => return Ok(None),
                };
                if frame.lo >= frame.hi {
                    (None, None)
                } else {
                    let p = if reverse {
                        frame.hi -= 1;
                        frame.hi
                    } else {
                        let x = frame.lo;
                        frame.lo += 1;
                        x
                    };
                    if frame.is_leaf {
                        (
                            Some((frame.node.keys[p].clone(), frame.node.row_at(p)?)),
                            None,
                        )
                    } else {
                        (None, Some(p))
                    }
                }
            };
            match (emit, descend) {
                (Some(pair), _) => return Ok(Some(pair)),
                (None, Some(i)) => {
                    let parent = self.stack.last().expect("top frame present for descend");
                    let ch = child(&parent.node, i, src)?;
                    self.stack.push(ScanFrame::new(ch, &self.bound));
                }
                (None, None) => {
                    self.stack.pop();
                }
            }
        }
    }
}

/// Recursive insert. Descends to the holding leaf (interior nodes route via
/// [`Node::child_slot`]); on overwrite, sets `*old` and rebuilds with the value+weight replaced
/// (which may now overflow). Splits propagate back up: a leaf split copies its boundary key up, an
/// interior receiving a separator may push-split in turn.
#[allow(clippy::too_many_arguments)]
fn node_insert(
    node: &Arc<Node>,
    key: Vec<u8>,
    val: Row,
    weight: u32,
    old: &mut Option<Row>,
    src: Option<&dyn LeafSource>,
    cap: usize,
    shape: LeafShape,
) -> Result<Ins> {
    if node.is_leaf() {
        let (i, edited_keys, mut vals, mut weights) = match node.search(&key) {
            Ok(i) => {
                let mut vals = node.decoded_rows()?;
                *old = Some(std::mem::replace(&mut vals[i], val));
                let mut weights = node.weights.clone();
                weights[i] = weight;
                (i, node.keys.clone(), vals, weights)
            }
            Err(i) => {
                let mut keys = node.keys.clone();
                let mut vals = node.decoded_rows()?;
                let mut weights = node.weights.clone();
                keys.insert(i, key);
                vals.insert(i, val);
                weights.insert(i, weight);
                (i, keys, vals, weights)
            }
        };
        let _ = &mut vals;
        let _ = &mut weights;
        return Ok(build_leaf(edited_keys, vals, weights, cap, shape, Some(i)));
    }
    // Fault the target child (a `Resident` interior, or an `OnDisk` leaf brought in for
    // mutation — it becomes a dirty resident node on the rebuilt path).
    let i = node.child_slot(&key);
    let c = child(node, i, src)?;
    match node_insert(&c, key, val, weight, old, src, cap, shape)? {
        Ins::Whole(c) => {
            // This node's separators are unchanged, so it cannot overflow — rebuild whole.
            let mut children = node.children.clone();
            children[i] = Child::Resident(c);
            Ok(Ins::Whole(Node::new_interior(node.keys.clone(), children)))
        }
        Ins::Split { left, sep, right } => {
            let mut keys = node.keys.clone();
            let mut children = node.children.clone();
            keys.insert(i, sep);
            children[i] = Child::Resident(left);
            children.insert(i + 1, Child::Resident(right));
            let edited = Some(i);
            Ok(build_interior(keys, children, cap, edited)
                .expect("insert-path interior split always has a valid split point"))
        }
    }
}

/// A non-root node is **underfull** when its payload is below half a page (`cap/2`), the threshold
/// at which delete rebalances it (format.md "Delete"). The root is exempt.
fn underfull(node: &Node, cap: usize, shape: LeafShape) -> bool {
    node.payload(shape) < cap / 2
}

/// Recursive delete (copy-on-write). Descends to the holding **leaf** (a separator equal to the
/// key routes right and is never itself deleted — separators may go stale, format.md "Delete").
/// Returns the rebuilt subtree (possibly underfull — the caller rebalances it) and the removed row
/// (or `None` if absent). The touched child is rebalanced via [`rebalance_child`].
fn node_remove(
    node: &Arc<Node>,
    key: &[u8],
    src: Option<&dyn LeafSource>,
    cap: usize,
    shape: LeafShape,
) -> Result<(Arc<Node>, Option<Row>)> {
    if node.is_leaf() {
        return match node.search(key) {
            Ok(i) => {
                let mut keys = node.keys.clone();
                let mut vals = node.decoded_rows()?;
                let mut weights = node.weights.clone();
                keys.remove(i);
                let removed = vals.remove(i);
                weights.remove(i);
                Ok((Node::new_leaf(keys, vals, weights), Some(removed)))
            }
            Err(_) => Ok((node.clone(), None)),
        };
    }
    let i = node.child_slot(key);
    let c = child(node, i, src)?;
    let (new_child, removed) = node_remove(&c, key, src, cap, shape)?;
    if removed.is_none() {
        return Ok((node.clone(), None));
    }
    let mut children = node.children.clone();
    children[i] = Child::Resident(new_child);
    let rebuilt = Node::new_interior(node.keys.clone(), children);
    Ok((rebalance_child(&rebuilt, i, src, cap, shape)?, removed))
}

/// If `node.children[i]` is underfull, merge it with an adjacent sibling (prefer the right one),
/// then split the merged node back if it overflows — the unified rebalance (no borrow). Returns the
/// rebuilt parent (which may itself have lost a key and become underfull — its own parent handles
/// that as the recursion unwinds).
fn rebalance_child(
    node: &Arc<Node>,
    i: usize,
    src: Option<&dyn LeafSource>,
    cap: usize,
    shape: LeafShape,
) -> Result<Arc<Node>> {
    // `children[i]` was just rebuilt resident by `node_remove`, so inspecting it faults nothing.
    if !underfull(node.children[i].resident(), cap, shape) {
        return Ok(node.clone());
    }
    if node.children.len() < 2 {
        // A 0-key interior (one child, the degenerate max-separator shape) has no sibling to merge
        // with — its own parent merges *it* away; the root case collapses in `PMap::remove`.
        return Ok(node.clone());
    }
    let j = if i + 1 < node.children.len() {
        i
    } else {
        i - 1
    };
    merge_at(node, j, src, cap, shape)
}

/// Merge `children[j]` and `children[j+1]` into one node `M` (format.md "Delete"): a **leaf**
/// merge concatenates the two record lists and the parent separator `j` is **removed** (it was a
/// routing copy — nothing comes down); an **interior** merge **pulls the separator down** between
/// the two key lists (the merged children need a routing key between them). If `M` fits, it
/// replaces the pair (the parent loses one key); if it overflows, it is split 2-way by the
/// balanced rule and the halves + the new separator replace the pair (the parent's key count is
/// unchanged). An **interior** `M` that overflows but admits no valid split (near-cap separators)
/// **abandons the merge** — the parent is returned unchanged (format.md "Delete", the deterministic
/// abandon rule).
fn merge_at(
    node: &Arc<Node>,
    j: usize,
    src: Option<&dyn LeafSource>,
    cap: usize,
    shape: LeafShape,
) -> Result<Arc<Node>> {
    // Fault both children — the underfull child (just rebuilt resident) and its sibling, which may
    // still be an `OnDisk` leaf the delete never touched.
    let left = child(node, j, src)?;
    let right = child(node, j + 1, src)?;

    let merged = if left.is_leaf() {
        let mut mkeys = left.keys.clone();
        let mut mvals = left.decoded_rows()?;
        let mut mweights = left.weights.clone();
        mkeys.extend(right.keys.iter().cloned());
        mvals.extend(right.decoded_rows()?);
        mweights.extend(right.weights.iter().copied());
        build_leaf(mkeys, mvals, mweights, cap, shape, None)
    } else {
        let mut mkeys = left.keys.clone();
        mkeys.push(node.keys[j].clone());
        mkeys.extend(right.keys.iter().cloned());
        let mut mchildren = left.children.clone();
        mchildren.extend(right.children.iter().cloned());
        match build_interior(mkeys, mchildren, cap, None) {
            Some(ins) => ins,
            // No valid 2-way split point (near-cap separators): abandon the merge — the two
            // children and the parent separator stay exactly as they were (underfull tolerated).
            None => return Ok(node.clone()),
        }
    };

    let mut keys = node.keys.clone();
    let mut children = node.children.clone();
    match merged {
        Ins::Whole(m) => {
            keys.remove(j);
            children[j] = Child::Resident(m);
            children.remove(j + 1);
            Ok(Node::new_interior(keys, children))
        }
        Ins::Split { left, sep, right } => {
            keys[j] = sep;
            children[j] = Child::Resident(left);
            children[j + 1] = Child::Resident(right);
            Ok(Node::new_interior(keys, children))
        }
    }
}

/// In-order walk — a leaf walk in key order (records are leaf-only, v24). Clones each
/// `(key, row)` out (owned) — see [`PMap::iter`] for why the walk does not borrow. Faults each
/// `OnDisk` leaf through `src`; the faulted `Arc` is dropped as soon as its rows are copied out, so
/// the resident leaf set stays bounded by the pool, not the tree (pager.md §4).
fn collect(node: &Node, src: Option<&dyn LeafSource>, out: &mut Vec<(Vec<u8>, Row)>) -> Result<()> {
    if node.is_leaf() {
        for i in 0..node.keys.len() {
            out.push((node.keys[i].clone(), node.row_at(i)?));
        }
        return Ok(());
    }
    for i in 0..node.children.len() {
        let c = child(node, i, src)?;
        collect(&c, src, out)?;
    }
    Ok(())
}

/// The pruned `collect` for a bounded scan: binary-search the child window (the children whose
/// separator span can overlap the bound — [`KeyBound::child_window`]) and, at each leaf, the
/// in-bound entry window ([`KeyBound::entry_window`]), then walk only those, in order. Mirrors
/// [`PMap::overlap_node_count`]'s traversal so the visited-node set — and the `page_read` cost — is
/// identical. `nodes` counts every node the walk enters — the same total
/// [`PMap::overlap_node_count`] computes, observed for free during the collecting descent.
fn collect_range(
    node: &Node,
    b: &KeyBound,
    src: Option<&dyn LeafSource>,
    out: &mut Vec<(Vec<u8>, Row)>,
    nodes: &mut usize,
) -> Result<()> {
    *nodes += 1;
    if node.is_leaf() {
        let (ef, el) = b.entry_window(node);
        for i in ef..el {
            out.push((node.keys[i].clone(), node.row_at(i)?));
        }
        return Ok(());
    }
    let (cf, cl) = b.child_window(node);
    for i in cf..=cl {
        let ch = child(node, i, src)?;
        collect_range(&ch, b, src, out, nodes)?;
    }
    Ok(())
}

/// Gather the columns `mask` selects of leaf row `i` into the per-column lanes (the A2 columnar
/// feed's per-entry step). Reads each selected column via [`Node::col_at`] — an O(1) PAX span on a
/// Packed leaf — so no full-width row is built. The untouched lanes are left untouched.
fn gather_cols(node: &Node, i: usize, mask: &[bool], cols: &mut [Vec<Value>]) -> Result<()> {
    for (c, &m) in mask.iter().enumerate() {
        if m {
            cols[c].push(node.col_at(i, c)?);
        }
    }
    Ok(())
}

/// The columnar twin of [`collect_range`] (packed-leaf.md §11 Track A2): the same windowed walk (so
/// the visited-node set — and the `page_read` cost — is identical), but gathers each admitted leaf
/// record's selected columns into `cols` lanes via [`gather_cols`] instead of pushing a
/// `(key, row)` pair. `row_count` counts the admitted records.
fn columnar_collect(
    node: &Node,
    b: &KeyBound,
    src: Option<&dyn LeafSource>,
    mask: &[bool],
    cols: &mut [Vec<Value>],
    row_count: &mut usize,
    nodes: &mut usize,
) -> Result<()> {
    *nodes += 1;
    if node.is_leaf() {
        let (ef, el) = b.entry_window(node);
        for i in ef..el {
            gather_cols(node, i, mask, cols)?;
            *row_count += 1;
        }
        return Ok(());
    }
    let (cf, cl) = b.child_window(node);
    for i in cf..=cl {
        let ch = child(node, i, src)?;
        columnar_collect(&ch, b, src, mask, cols, row_count, nodes)?;
    }
    Ok(())
}

/// The fold-during-walk twin of [`columnar_collect`] (packed-leaf.md §11 Track A2): the identical
/// windowed walk (so the visited-node set — and `page_read` — is identical), but calls
/// `visit(node, i)` per admitted leaf record instead of gathering its columns into lanes. `visit`
/// reads the record's touched columns via [`Node::col_at`]. `row_count` counts admitted records;
/// `nodes` counts visited nodes.
fn fold_walk(
    node: &Node,
    b: &KeyBound,
    src: Option<&dyn LeafSource>,
    visit: &mut dyn FnMut(&Node, usize) -> Result<()>,
    row_count: &mut usize,
    nodes: &mut usize,
) -> Result<()> {
    *nodes += 1;
    if node.is_leaf() {
        let (ef, el) = b.entry_window(node);
        for i in ef..el {
            visit(node, i)?;
            *row_count += 1;
        }
        return Ok(());
    }
    let (cf, cl) = b.child_window(node);
    for i in cf..=cl {
        let ch = child(node, i, src)?;
        fold_walk(&ch, b, src, visit, row_count, nodes)?;
    }
    Ok(())
}

/// The early-stoppable, streaming `collect_range`: calls `visit` per in-bound leaf row instead of
/// pushing to a `Vec`, and stops the whole traversal (returning `Ok(false)`) when `visit` does —
/// without faulting any leaf past the stop point. Mirrors `collect_range`'s windowed walk.
fn walk_range_visit(
    node: &Node,
    b: &KeyBound,
    src: Option<&dyn LeafSource>,
    visit: &mut dyn FnMut(&[u8], &Row) -> Result<bool>,
) -> Result<bool> {
    if node.is_leaf() {
        let (ef, el) = b.entry_window(node);
        for i in ef..el {
            if !node.with_row(i, |row| visit(&node.keys[i], row))? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    let (cf, cl) = b.child_window(node);
    for i in cf..=cl {
        let ch = child(node, i, src)?;
        if !walk_range_visit(&ch, b, src, visit)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The reverse-order `walk_range_visit`: visits the in-bound leaf rows in **descending** key order,
/// the exact reverse of the forward traversal's sequence (so an `ORDER BY pk DESC` is satisfied by
/// the scan). Stops the whole traversal (returning `Ok(false)`) when `visit` does, without faulting
/// leaves past the stop point.
fn walk_range_visit_rev(
    node: &Node,
    b: &KeyBound,
    src: Option<&dyn LeafSource>,
    visit: &mut dyn FnMut(&[u8], &Row) -> Result<bool>,
) -> Result<bool> {
    if node.is_leaf() {
        let (ef, el) = b.entry_window(node);
        for i in (ef..el).rev() {
            if !node.with_row(i, |row| visit(&node.keys[i], row))? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    let (cf, cl) = b.child_window(node);
    for i in (cf..=cl).rev() {
        let ch = child(node, i, src)?;
        if !walk_range_visit_rev(&ch, b, src, visit)? {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;

    // A small page cap so a few-thousand-entry map is several levels deep — exercises split,
    // merge-then-split, root growth and collapse (the in-RAM analog of page_size 256).
    const CAP: usize = 240;
    /// `row` has one fixed-width value column — the v24 leaf overhead scales with the class mix
    /// (format.md "Leaf node").
    const SHAPE: LeafShape = LeafShape { fixed: 1, var: 0 };

    fn row(n: i64) -> Row {
        vec![Value::Int(n)]
    }

    fn key(n: u64) -> Vec<u8> {
        n.to_be_bytes().to_vec()
    }

    /// A realistic per-entry weight: 8-byte key + an 8-byte i64 slot = 16 bytes, so a 240-byte
    /// leaf holds ~12 entries before splitting (well under RECORD_MAX).
    const W: u32 = 16;

    /// A deterministic permutation of `0..n` (an LCG-driven shuffle) — no wall-clock / RNG, so the
    /// test is reproducible (CLAUDE.md §10).
    fn shuffled(n: u64) -> Vec<u64> {
        let mut v: Vec<u64> = (0..n).collect();
        let mut state: u64 = 0x9e3779b97f4a7c15;
        for i in (1..v.len()).rev() {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let j = (state >> 33) as usize % (i + 1);
            v.swap(i, j);
        }
        v
    }

    /// The structural invariants the byte contract relies on (format.md "Fan-out"): every node
    /// fits a page; every leaf is non-empty; an interior node has `N+1` children (`N ≥ 0` only in
    /// the degenerate case — these small-key tests never produce it, so `N ≥ 1` is asserted);
    /// records (vals/weights) live only in leaves; all leaves at the same depth; and every key in
    /// a subtree respects its bounding separators (left < sep ≤ right).
    fn check_invariants(pm: &PMap) {
        fn walk(
            node: &Node,
            is_root: bool,
            cap: usize,
            lo: Option<&[u8]>,
            hi: Option<&[u8]>,
        ) -> usize {
            if node.is_leaf() {
                assert!(!node.keys.is_empty() || is_root, "non-root leaf is empty");
                if node.packed.is_none() {
                    assert_eq!(node.keys.len(), node.vals.len());
                }
                assert_eq!(node.keys.len(), node.weights.len());
            } else {
                assert!(
                    !node.keys.is_empty() || is_root,
                    "0-key interior unexpected"
                );
                assert!(node.vals.is_empty(), "interior node carries records");
                assert!(node.weights.is_empty(), "interior node carries weights");
                assert_eq!(
                    node.children.len(),
                    node.keys.len() + 1,
                    "interior child count"
                );
            }
            for w in node.keys.windows(2) {
                assert!(w[0] < w[1], "keys out of order");
            }
            // Subtree keys respect the bounding separators: lo ≤ key < hi (lo inclusive because a
            // separator equals the right subtree's first key at split time).
            for k in &node.keys {
                if let Some(lo) = lo {
                    assert!(k.as_slice() >= lo, "key below its subtree's low separator");
                }
                if let Some(hi) = hi {
                    assert!(
                        k.as_slice() < hi,
                        "key at/above its subtree's high separator"
                    );
                }
            }
            assert!(
                node.payload(SHAPE) <= cap,
                "node payload {} exceeds cap {cap}",
                node.payload(SHAPE)
            );
            if node.is_leaf() {
                return 1;
            }
            let mut depth = None;
            for (i, c) in node.children.iter().enumerate() {
                let clo = if i == 0 {
                    lo
                } else {
                    Some(node.keys[i - 1].as_slice())
                };
                let chi = if i == node.keys.len() {
                    hi
                } else {
                    Some(node.keys[i].as_slice())
                };
                let d = walk(c.resident(), false, cap, clo, chi);
                match depth {
                    None => depth = Some(d),
                    Some(prev) => assert_eq!(prev, d, "leaves at unequal depth"),
                }
            }
            depth.unwrap() + 1
        }
        if let Some(root) = &pm.root {
            walk(root, true, CAP, None, None);
        }
    }

    #[test]
    fn insert_get_remove_against_reference() {
        use std::collections::BTreeMap;
        let mut pm = PMap::new();
        let mut bt: BTreeMap<Vec<u8>, Row> = BTreeMap::new();
        let n = 4000;

        for k in shuffled(n) {
            assert_eq!(
                pm.insert(key(k), row(k as i64), W, CAP, SHAPE, None)
                    .unwrap(),
                bt.insert(key(k), row(k as i64))
            );
        }
        // An in-memory map (built from empty) tracks its count exactly (`Some`).
        assert_eq!(pm.count(), Some(bt.len()));
        check_invariants(&pm);
        for k in 0..n {
            assert_eq!(pm.get(&key(k), None).unwrap().as_ref(), bt.get(&key(k)));
        }
        let got: Vec<_> = pm.iter(None).unwrap();
        let want: Vec<_> = bt.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(got, want);

        // Overwrite returns the old value and does not change the count.
        let before = pm.count();
        assert_eq!(
            pm.insert(key(7), row(777), W, CAP, SHAPE, None).unwrap(),
            bt.insert(key(7), row(777))
        );
        assert_eq!(pm.count(), before);
        assert_eq!(pm.get(&key(7), None).unwrap(), Some(row(777)));

        // Interleave removes with invariant checks so merge-then-split is exercised mid-stream.
        for (step, k) in shuffled(n).into_iter().enumerate() {
            assert_eq!(
                pm.remove(&key(k), CAP, SHAPE, None).unwrap(),
                bt.remove(&key(k))
            );
            if step % 257 == 0 {
                check_invariants(&pm);
            }
        }
        assert!(pm.is_empty());
        assert_eq!(pm.iter(None).unwrap().len(), 0);
        assert_eq!(pm.remove(&key(123), CAP, SHAPE, None).unwrap(), None);
    }

    #[test]
    fn clone_is_an_independent_snapshot() {
        let mut base = PMap::new();
        for k in 0..2000 {
            base.insert(key(k), row(k as i64), W, CAP, SHAPE, None)
                .unwrap();
        }
        let snap = base.clone();

        let mut other = base.clone();
        for k in 0..2000 {
            other
                .insert(key(k), row(-(k as i64)), W, CAP, SHAPE, None)
                .unwrap(); // overwrite every value
        }
        for k in 2000..3000 {
            other
                .insert(key(k), row(k as i64), W, CAP, SHAPE, None)
                .unwrap(); // and grow it
        }
        for k in 0..500 {
            other.remove(&key(k), CAP, SHAPE, None).unwrap(); // and shrink it
        }

        // `snap` still sees the original contents, untouched.
        assert_eq!(snap.count(), Some(2000));
        for k in 0..2000 {
            assert_eq!(snap.get(&key(k), None).unwrap(), Some(row(k as i64)));
        }
        let snap_rows: Vec<_> = snap.iter(None).unwrap();
        assert_eq!(snap_rows.len(), 2000);
        assert_eq!(snap_rows[0], (key(0), row(0)));
        assert_eq!(snap_rows[1999], (key(1999), row(1999)));
        check_invariants(&snap);

        assert_eq!(other.count(), Some(2500));
        assert_eq!(other.get(&key(0), None).unwrap(), None);
        assert_eq!(other.get(&key(1000), None).unwrap(), Some(row(-1000)));
        assert_eq!(other.get(&key(2500), None).unwrap(), Some(row(2500)));
        check_invariants(&other);
    }

    #[test]
    fn empty_and_single() {
        let mut pm = PMap::new();
        assert!(pm.is_empty());
        assert_eq!(pm.get(&key(1), None).unwrap(), None);
        assert_eq!(pm.remove(&key(1), CAP, SHAPE, None).unwrap(), None);
        assert_eq!(
            pm.insert(key(1), row(1), W, CAP, SHAPE, None).unwrap(),
            None
        );
        assert_eq!(pm.get(&key(1), None).unwrap(), Some(row(1)));
        assert_eq!(pm.remove(&key(1), CAP, SHAPE, None).unwrap(), Some(row(1)));
        assert!(pm.is_empty());
        assert!(pm.root.is_none());
    }

    /// Wide records (near RECORD_MAX) force tiny fan-out — the stress case for the split point and
    /// the fit guarantee. With weight 100 (≤ RECORD_MAX(240, 1) = 106), a two-record leaf fits but
    /// a third overflows.
    #[test]
    fn wide_values_keep_nodes_valid() {
        use std::collections::BTreeMap;
        let mut pm = PMap::new();
        let mut bt: BTreeMap<Vec<u8>, Row> = BTreeMap::new();
        for k in shuffled(300) {
            pm.insert(key(k), row(k as i64), 100, CAP, SHAPE, None)
                .unwrap();
            bt.insert(key(k), row(k as i64));
            check_invariants(&pm);
        }
        for k in shuffled(300) {
            pm.remove(&key(k), CAP, SHAPE, None).unwrap();
            bt.remove(&key(k));
            check_invariants(&pm);
        }
        assert!(pm.is_empty());
    }

    /// Near-cap KEYS (the max-size-separator case, format.md "Interior node"): separators are key
    /// copies, so two of them overflow an interior node, forcing the pinned degenerate `N = 2 →
    /// m = 1` split and legal 0-key interiors. The map must stay correct through inserts, scans,
    /// and removes (a looser invariant check — 0-key interiors are legal here).
    #[test]
    fn near_cap_keys_degenerate_interior() {
        use std::collections::BTreeMap;
        // Index-tree shape: zero value columns, record = key alone. RECORD_MAX(0) = (240-12)/2
        // = 114; keys of 110 bytes keep records under the cap while two separators (2·110 + 20)
        // overflow an interior.
        let shape = LeafShape { fixed: 0, var: 0 };
        let big_key = |n: u64| {
            let mut k = vec![0xAB; 110];
            k[..8].copy_from_slice(&n.to_be_bytes());
            k
        };
        let mut pm = PMap::new();
        let mut bt: BTreeMap<Vec<u8>, Row> = BTreeMap::new();
        for k in shuffled(60) {
            pm.insert(big_key(k), Vec::new(), 110, CAP, shape, None)
                .unwrap();
            bt.insert(big_key(k), Vec::new());
        }
        assert_eq!(pm.count(), Some(bt.len()));
        // Structure: fits + routing correctness (0-key interiors allowed).
        fn walk(node: &Node, cap: usize, shape: LeafShape) {
            assert!(node.payload(shape) <= cap, "node overflows its page");
            if !node.is_leaf() {
                assert_eq!(node.children.len(), node.keys.len() + 1);
                for c in &node.children {
                    walk(c.resident(), cap, shape);
                }
            }
        }
        walk(pm.root().unwrap(), CAP, shape);
        let got: Vec<_> = pm.iter(None).unwrap();
        let want: Vec<_> = bt.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(got, want);
        for k in 0..60 {
            assert!(pm.get(&big_key(k), None).unwrap().is_some());
        }
        for k in shuffled(60) {
            assert_eq!(
                pm.remove(&big_key(k), CAP, shape, None).unwrap(),
                Some(Vec::new())
            );
        }
        assert!(pm.is_empty());
    }

    /// The bounded scan yields exactly the in-bound rows, in order, and the counted nodes match
    /// `overlap_node_count`; the pull cursor and the reverse walk agree with it.
    #[test]
    fn bounded_scans_and_cursor_agree() {
        let mut pm = PMap::new();
        for k in shuffled(2000) {
            pm.insert(key(k), row(k as i64), W, CAP, SHAPE, None)
                .unwrap();
        }
        let b = KeyBound {
            lo: Some(key(500)),
            lo_inc: true,
            hi: Some(key(1500)),
            hi_inc: false,
        };
        let (entries, nodes) = pm.range_entries_counted(&b, None).unwrap();
        assert_eq!(entries.len(), 1000);
        assert_eq!(entries[0].0, key(500));
        assert_eq!(entries[999].0, key(1499));
        assert_eq!(nodes, pm.overlap_node_count(&b));

        // Push walk agrees.
        let mut push = Vec::new();
        pm.scan_range(&b, None, &mut |k, r| {
            push.push((k.to_vec(), r.clone()));
            Ok(true)
        })
        .unwrap();
        assert_eq!(push, entries);

        // Reverse push walk is the exact reverse.
        let mut rev = Vec::new();
        pm.scan_range_rev(&b, None, &mut |k, r| {
            rev.push((k.to_vec(), r.clone()));
            Ok(true)
        })
        .unwrap();
        let mut expect = entries.clone();
        expect.reverse();
        assert_eq!(rev, expect);

        // Pull cursor agrees, both directions.
        let mut fwd = Vec::new();
        let mut cur = pm.range_cursor(
            KeyBound {
                lo: Some(key(500)),
                lo_inc: true,
                hi: Some(key(1500)),
                hi_inc: false,
            },
            false,
        );
        while let Some(pair) = cur.next(None).unwrap() {
            fwd.push(pair);
        }
        assert_eq!(fwd, entries);

        let mut bwd = Vec::new();
        let mut cur = pm.range_cursor(
            KeyBound {
                lo: Some(key(500)),
                lo_inc: true,
                hi: Some(key(1500)),
                hi_inc: false,
            },
            true,
        );
        while let Some(pair) = cur.next(None).unwrap() {
            bwd.push(pair);
        }
        assert_eq!(bwd, expect);

        // Exclusive lo / inclusive hi.
        let b2 = KeyBound {
            lo: Some(key(500)),
            lo_inc: false,
            hi: Some(key(1500)),
            hi_inc: true,
        };
        let got = pm.range_entries(&b2, None).unwrap();
        assert_eq!(got[0].0, key(501));
        assert_eq!(got.last().unwrap().0, key(1500));
        assert_eq!(got.len(), 1000);
    }
}
