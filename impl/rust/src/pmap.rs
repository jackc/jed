//! Persistent (copy-on-write) ordered map — the page-backed B-tree (decision **B1**,
//! spec/design/transactions.md §3; spec/fileformat/format.md "The per-table data B-tree").
//!
//! Keyed by the encoded key bytes (`Vec<u8>`, whose `Ord` is lexicographic = the
//! order-preserving key encoding's memcmp contract, spec/design/encoding.md). Every mutation
//! returns a **new** map that shares structure with the old one — the old root is provably
//! unchanged — so a snapshot is an O(1) `Arc` clone and a commit is a pointer swap
//! (transactions.md §2).
//!
//! **This is the on-disk B-tree, node-for-page (Phase 6, P6.1).** Its fan-out is **size-driven**:
//! a node holds as many entries as fit a page payload `cap` (= `page_size − 12`) and **splits when
//! it would overflow** — so the node boundaries, and therefore the serialized bytes, are a §8 byte
//! contract (format.md). The caller supplies each entry's on-disk **weight** (its record size) so
//! this map can sum payloads without knowing the value codec; `cap` is passed per call (it is a
//! property of the database's page size, held by the [`crate::storage::TableStore`]).
//!
//! Each [`Node`] also carries a set-once on-disk **page id** (`0` = dirty/unpersisted): an
//! incremental commit writes only the dirty nodes a mutation introduced (format.rs / file.rs).
//! Copy-on-write builds every new node dirty; a node persisted once is never rewritten while it
//! stays shared. `AtomicU32` keeps the shared tree `Send + Sync` (P5.3b) under a relaxed set-once
//! store — the node is otherwise immutable.
//!
//! Boring and explicit (CLAUDE.md §10): one `Node` type (a leaf has no children), recursive insert
//! with split-on-overflow, recursive delete via in-order-predecessor replacement and
//! **merge-then-maybe-split** rebalancing (no borrow — merge subsumes it; format.md "Delete").

use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use crate::error::Result;
use crate::storage::Row;

/// A B-tree node's reference to one child. Under demand paging (P6.4b, spec/design/pager.md §4) a
/// clean leaf need not be resident: an interior node keeps `OnDisk(page_id)` for such a child and
/// the read path faults it through the buffer pool on access. A `Resident` child is an in-memory
/// node — a dirty/uncommitted node, a resident interior skeleton node (interior nodes are *always*
/// resident, §1), or a leaf currently materialized. Because only **leaves** are paged, an `OnDisk`
/// child is always a leaf — which is exactly what lets `node_count` (cost §5) be computed without
/// loading any leaf. An in-memory database constructs no `OnDisk` child (it is fully resident).
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
/// the buffer pool, behind the table's column types. Defined here so the B-tree traversal can fault
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

/// One B-tree node. `children` is empty for a leaf; otherwise `children.len() == keys.len() + 1`.
/// `keys.len() == vals.len() == weights.len()` always. `weights[i]` is entry `i`'s on-disk record
/// size (format.md), used only for the size-driven split/merge decisions. Nodes are shared behind
/// `Arc`; a mutation clones only the root→leaf path and shares every untouched subtree.
pub(crate) struct Node {
    pub(crate) keys: Vec<Vec<u8>>,
    pub(crate) vals: Vec<Row>,
    pub(crate) weights: Vec<u32>,
    pub(crate) children: Vec<Child>,
    /// On-disk page index, or `0` when dirty (never persisted / changed since). Set once by the
    /// incremental commit that first persists this node (format.rs `serialize_dirty`, P6.1 part B);
    /// page 0 is a meta slot, never a node, so it doubles as the dirty sentinel. A clean node lets an
    /// incremental commit skip its whole (unchanged) subtree.
    pub(crate) page: AtomicU32,
}

impl Node {
    /// A fresh **dirty** node (page `0`) — every copy-on-write rebuild goes through here.
    fn new(
        keys: Vec<Vec<u8>>,
        vals: Vec<Row>,
        weights: Vec<u32>,
        children: Vec<Child>,
    ) -> Arc<Node> {
        Arc::new(Node {
            keys,
            vals,
            weights,
            children,
            page: AtomicU32::new(0),
        })
    }

    /// A node reconstructed from disk at `page` (format.rs `read_tree`), already persisted. Its
    /// children may be `Resident` (the fully-resident in-memory load) or `OnDisk` (the demand-paged
    /// skeleton load, B2) — the constructor is agnostic.
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
            page: AtomicU32::new(page),
        })
    }

    /// A **leaf** node value reconstructed from disk at `page` for the demand-paging fault path
    /// (format.rs `decode_leaf_node`). Returns the bare `Node` — the buffer pool wraps it in an `Arc`
    /// (paging.rs). A leaf has no children.
    pub(crate) fn leaf_loaded(
        keys: Vec<Vec<u8>>,
        vals: Vec<Row>,
        weights: Vec<u32>,
        page: u32,
    ) -> Node {
        Node {
            keys,
            vals,
            weights,
            children: Vec::new(),
            page: AtomicU32::new(page),
        }
    }

    pub(crate) fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }

    /// This node's serialized payload size (format.md): `Σ weights` plus, for an interior node,
    /// `4·(N+1)` for its child pointers.
    fn payload(&self) -> usize {
        let entries: usize = self.weights.iter().map(|&w| w as usize).sum();
        entries
            + if self.is_leaf() {
                0
            } else {
                4 * self.children.len()
            }
    }

    /// Binary-search this node's keys: `Ok(i)` if `key` sits at index `i`, else `Err(i)` for the
    /// child/insertion slot. `Vec<u8>::cmp` is lexicographic (memcmp) — the key contract.
    fn search(&self, key: &[u8]) -> std::result::Result<usize, usize> {
        self.keys.binary_search_by(|k| k.as_slice().cmp(key))
    }
}

/// The result of inserting into a subtree: either the rebuilt subtree, or a node that overflowed
/// and split into `left`, a median `(key, val, weight)` to promote, and `right`.
enum Ins {
    Whole(Arc<Node>),
    Split {
        left: Arc<Node>,
        key: Vec<u8>,
        val: Row,
        weight: u32,
        right: Arc<Node>,
    },
}

/// A persistent ordered map from encoded key to [`Row`]. `Clone` is O(1) (an `Arc` bump on the root
/// plus a length copy) and yields an independent snapshot: mutating the clone leaves this map
/// untouched.
#[derive(Clone, Default)]
pub struct PMap {
    root: Option<Arc<Node>>,
    len: usize,
}

impl PMap {
    pub fn new() -> Self {
        PMap { root: None, len: 0 }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The root node, for the serializer (format.rs). `None` for an empty map.
    pub(crate) fn root(&self) -> Option<&Arc<Node>> {
        self.root.as_ref()
    }

    /// Reconstruct a map from a loaded root (format.rs `from_image`).
    pub(crate) fn from_loaded(root: Option<Arc<Node>>, len: usize) -> Self {
        PMap { root, len }
    }

    /// Look up the row at `key`, or `None`. Returns an **owned** row: under demand paging (P6.4b)
    /// the leaf holding it may live only in the buffer pool, not the resident tree, so a borrow
    /// could not outlive the pool lock — the read path clones the row out (spec/design/pager.md §4).
    /// `src` faults an `OnDisk` leaf on the descent (`None` for a fully-resident in-memory tree).
    pub(crate) fn get(&self, key: &[u8], src: Option<&dyn LeafSource>) -> Result<Option<Row>> {
        // Hold an owned `Arc` to the current node so a faulted leaf outlives the step that reads it.
        let mut cur = match &self.root {
            None => return Ok(None),
            Some(root) => root.clone(),
        };
        loop {
            match cur.search(key) {
                Ok(i) => return Ok(Some(cur.vals[i].clone())),
                Err(i) => {
                    if cur.is_leaf() {
                        return Ok(None);
                    }
                    cur = child(&cur, i, src)?;
                }
            }
        }
    }

    /// Insert or overwrite `key` with `val` (whose on-disk record size is `weight`); `cap` is the
    /// page payload capacity. Returns the previous row if `key` was present (an overwrite), else
    /// `None` (a new insert, which grows `len`). An overwrite can change the weight, so it too may
    /// overflow and split.
    pub(crate) fn insert(
        &mut self,
        key: Vec<u8>,
        val: Row,
        weight: u32,
        cap: usize,
        src: Option<&dyn LeafSource>,
    ) -> Result<Option<Row>> {
        let mut old = None;
        let new_root = match &self.root {
            None => Node::new(vec![key], vec![val], vec![weight], Vec::new()),
            Some(root) => match node_insert(root, key, val, weight, &mut old, src, cap)? {
                Ins::Whole(n) => n,
                Ins::Split {
                    left,
                    key,
                    val,
                    weight,
                    right,
                } => Node::new(
                    vec![key],
                    vec![val],
                    vec![weight],
                    vec![Child::Resident(left), Child::Resident(right)],
                ),
            },
        };
        self.root = Some(new_root);
        if old.is_none() {
            self.len += 1;
        }
        Ok(old)
    }

    /// Remove `key`. Returns the removed row, or `None` if absent (then `self` is unchanged). `src`
    /// faults `OnDisk` leaves the delete descends into / rebalances against (spec/design/pager.md §4).
    pub(crate) fn remove(
        &mut self,
        key: &[u8],
        cap: usize,
        src: Option<&dyn LeafSource>,
    ) -> Result<Option<Row>> {
        let root = match self.root.as_ref() {
            None => return Ok(None),
            Some(r) => r.clone(),
        };
        let (new_root, removed) = node_remove(&root, key, src, cap)?;
        if removed.is_some() {
            // The root may have drained to zero keys: an empty leaf becomes the empty map; an empty
            // internal node (one child) hands the root down a level (height shrinks). The root is
            // exempt from the underfull rule, so no rebalance here.
            self.root = if new_root.keys.is_empty() {
                if new_root.is_leaf() {
                    None
                } else {
                    // The lone surviving child becomes the new root — fault it if it is an OnDisk leaf
                    // (a tree of height 2 can collapse to its single bottom child).
                    Some(child(&new_root, 0, src)?)
                }
            } else {
                Some(new_root)
            };
            self.len -= 1;
        }
        Ok(removed)
    }

    /// The number of B-tree nodes (pages) in this tree — the `page_read` count a full scan
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

    /// Iterate `(key, row)` pairs in ascending key order, yielding **owned** pairs. Eagerly walks
    /// the tree into a vector (the cost contract charges per row in the executor loop, not here —
    /// cost.md). Owned, not borrowed, because under demand paging (P6.4b) a leaf may be faulted in
    /// from the buffer pool only for the duration of this walk: the row is cloned out and the leaf
    /// node is free to be evicted, so the resident *node* set stays bounded by the pool even though
    /// the executor materializes the rows it scans (streaming the rows themselves is a deferred,
    /// out-of-scope follow-on — spec/design/pager.md §4/§6).
    pub(crate) fn iter(&self, src: Option<&dyn LeafSource>) -> Result<Vec<(Vec<u8>, Row)>> {
        let mut out = Vec::with_capacity(self.len);
        if let Some(root) = &self.root {
            collect(root, src, &mut out)?;
        }
        Ok(out)
    }
}

/// Build a node from its parts; if its payload overflows `cap`, split it 2-way and promote one
/// median. The split point `m = min(largest m in [1,N-1] with leftpayload(m) ≤ cap, N-2)` always
/// yields two non-empty, fitting halves under the `RECORD_MAX = (cap-12)/2` cap (format.md "Why the
/// record cap"). `children` empty ⇒ leaf.
fn build(
    keys: Vec<Vec<u8>>,
    vals: Vec<Row>,
    weights: Vec<u32>,
    children: Vec<Child>,
    cap: usize,
) -> Ins {
    let interior = !children.is_empty();
    let payload: usize = weights.iter().map(|&w| w as usize).sum::<usize>()
        + if interior { 4 * children.len() } else { 0 };
    // Under `RECORD_MAX = (cap-12)/2` a node with ≤ 2 keys never overflows (format.md), so a node
    // that overflows here always has ≥ 3 keys and splits cleanly. The `< 3` guard is purely
    // defensive against an oversized record (one larger than `RECORD_MAX`): it leaves the node
    // whole rather than splitting an unsplittable one — the oversize is then surfaced as `0A000`
    // when the node is serialized (format.rs), matching the v1 behaviour.
    if payload <= cap || keys.len() < 3 {
        return Ins::Whole(Node::new(keys, vals, weights, children));
    }

    let n = keys.len();
    // largest m in [1, n-1] with leftpayload(m) ≤ cap
    let mut best = 1usize;
    let mut prefix = 0usize;
    for m in 1..n {
        prefix += weights[m - 1] as usize;
        let lp = if interior { 4 * (m + 1) } else { 0 } + prefix;
        if lp <= cap {
            best = m;
        }
    }
    let m = best.min(n - 2).max(1);

    let mut keys = keys;
    let mut vals = vals;
    let mut weights = weights;
    let mut children = children;
    let rkeys = keys.split_off(m + 1);
    let mkey = keys.pop().unwrap();
    let rvals = vals.split_off(m + 1);
    let mval = vals.pop().unwrap();
    let rweights = weights.split_off(m + 1);
    let mweight = weights.pop().unwrap();
    let (lchildren, rchildren) = if interior {
        let rc = children.split_off(m + 1);
        (children, rc)
    } else {
        (Vec::new(), Vec::new())
    };

    Ins::Split {
        left: Node::new(keys, vals, weights, lchildren),
        key: mkey,
        val: mval,
        weight: mweight,
        right: Node::new(rkeys, rvals, rweights, rchildren),
    }
}

/// Recursive insert. On overwrite, sets `*old` and rebuilds with the value+weight replaced (which
/// may now overflow). On a new key, inserts into the target leaf and splits overflowing nodes back
/// up the path.
fn node_insert(
    node: &Arc<Node>,
    key: Vec<u8>,
    val: Row,
    weight: u32,
    old: &mut Option<Row>,
    src: Option<&dyn LeafSource>,
    cap: usize,
) -> Result<Ins> {
    match node.search(&key) {
        Ok(i) => {
            let mut vals = node.vals.clone();
            *old = Some(std::mem::replace(&mut vals[i], val));
            let mut weights = node.weights.clone();
            weights[i] = weight;
            Ok(build(
                node.keys.clone(),
                vals,
                weights,
                node.children.clone(),
                cap,
            ))
        }
        Err(i) => {
            if node.is_leaf() {
                let mut keys = node.keys.clone();
                let mut vals = node.vals.clone();
                let mut weights = node.weights.clone();
                keys.insert(i, key);
                vals.insert(i, val);
                weights.insert(i, weight);
                Ok(build(keys, vals, weights, Vec::new(), cap))
            } else {
                // Fault the target child (a `Resident` interior, or an `OnDisk` leaf brought in for
                // mutation — it becomes a dirty resident node on the rebuilt path).
                let c = child(node, i, src)?;
                match node_insert(&c, key, val, weight, old, src, cap)? {
                    Ins::Whole(c) => {
                        // This node's separators are unchanged, so it cannot overflow — rebuild whole.
                        let mut children = node.children.clone();
                        children[i] = Child::Resident(c);
                        Ok(Ins::Whole(Node::new(
                            node.keys.clone(),
                            node.vals.clone(),
                            node.weights.clone(),
                            children,
                        )))
                    }
                    Ins::Split {
                        left,
                        key: mk,
                        val: mv,
                        weight: mw,
                        right,
                    } => {
                        let mut keys = node.keys.clone();
                        let mut vals = node.vals.clone();
                        let mut weights = node.weights.clone();
                        let mut children = node.children.clone();
                        keys.insert(i, mk);
                        vals.insert(i, mv);
                        weights.insert(i, mw);
                        children[i] = Child::Resident(left);
                        children.insert(i + 1, Child::Resident(right));
                        Ok(build(keys, vals, weights, children, cap))
                    }
                }
            }
        }
    }
}

/// A non-root node is **underfull** when its payload is below half a page (`cap/2`), the threshold
/// at which delete rebalances it (format.md "Delete"). The root is exempt.
fn underfull(node: &Node, cap: usize) -> bool {
    node.payload() < cap / 2
}

/// The rightmost `(key, val, weight)` of a subtree — its in-order predecessor entry. Holds an owned
/// `Arc` as it descends so a faulted rightmost leaf stays alive while it is read.
fn max_kv(node: &Arc<Node>, src: Option<&dyn LeafSource>) -> Result<(Vec<u8>, Row, u32)> {
    let mut n = node.clone();
    while !n.is_leaf() {
        let last = n.children.len() - 1;
        n = child(&n, last, src)?;
    }
    Ok((
        n.keys.last().unwrap().clone(),
        n.vals.last().unwrap().clone(),
        *n.weights.last().unwrap(),
    ))
}

/// Recursive delete (copy-on-write). Returns the rebuilt subtree (possibly underfull — the caller
/// rebalances it) and the removed row (or `None` if absent). A separator found in an interior node
/// is replaced by its in-order **predecessor** (drawn from the left subtree), which is then deleted
/// from that subtree; the touched child is rebalanced via [`rebalance_child`].
fn node_remove(
    node: &Arc<Node>,
    key: &[u8],
    src: Option<&dyn LeafSource>,
    cap: usize,
) -> Result<(Arc<Node>, Option<Row>)> {
    match node.search(key) {
        Ok(i) => {
            if node.is_leaf() {
                let mut keys = node.keys.clone();
                let mut vals = node.vals.clone();
                let mut weights = node.weights.clone();
                keys.remove(i);
                let removed = vals.remove(i);
                weights.remove(i);
                Ok((Node::new(keys, vals, weights, Vec::new()), Some(removed)))
            } else {
                let removed = node.vals[i].clone();
                // Fault the left subtree once; both the predecessor lookup and its deletion descend it.
                let left_child = child(node, i, src)?;
                let (pk, pv, pw) = max_kv(&left_child, src)?;
                let (new_child, _) = node_remove(&left_child, &pk, src, cap)?;
                let mut keys = node.keys.clone();
                let mut vals = node.vals.clone();
                let mut weights = node.weights.clone();
                let mut children = node.children.clone();
                keys[i] = pk;
                vals[i] = pv;
                weights[i] = pw;
                children[i] = Child::Resident(new_child);
                let rebuilt = Node::new(keys, vals, weights, children);
                Ok((rebalance_child(&rebuilt, i, src, cap)?, Some(removed)))
            }
        }
        Err(i) => {
            if node.is_leaf() {
                Ok((node.clone(), None))
            } else {
                let c = child(node, i, src)?;
                let (new_child, removed) = node_remove(&c, key, src, cap)?;
                if removed.is_none() {
                    return Ok((node.clone(), None));
                }
                let mut children = node.children.clone();
                children[i] = Child::Resident(new_child);
                let rebuilt = Node::new(
                    node.keys.clone(),
                    node.vals.clone(),
                    node.weights.clone(),
                    children,
                );
                Ok((rebalance_child(&rebuilt, i, src, cap)?, removed))
            }
        }
    }
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
) -> Result<Arc<Node>> {
    // `children[i]` was just rebuilt resident by `node_remove`, so inspecting it faults nothing.
    if !underfull(node.children[i].resident(), cap) {
        return Ok(node.clone());
    }
    let j = if i + 1 < node.children.len() {
        i
    } else {
        i - 1
    };
    merge_at(node, j, src, cap)
}

/// Merge `children[j]`, separator `j`, and `children[j+1]` into one node `M`. If `M` fits, it
/// replaces the pair and the parent loses separator `j` and child `j+1`. If `M` overflows, it is
/// split 2-way and the two halves + the new separator replace the pair (the parent's key count is
/// unchanged). `M < 2·cap` always (format.md), so a single split restores fit.
fn merge_at(
    node: &Arc<Node>,
    j: usize,
    src: Option<&dyn LeafSource>,
    cap: usize,
) -> Result<Arc<Node>> {
    // Fault both children — the underfull child (just rebuilt resident) and its sibling, which may
    // still be an `OnDisk` leaf the delete never touched.
    let left = child(node, j, src)?;
    let right = child(node, j + 1, src)?;

    let mut mkeys = left.keys.clone();
    let mut mvals = left.vals.clone();
    let mut mweights = left.weights.clone();
    mkeys.push(node.keys[j].clone());
    mvals.push(node.vals[j].clone());
    mweights.push(node.weights[j]);
    mkeys.extend(right.keys.iter().cloned());
    mvals.extend(right.vals.iter().cloned());
    mweights.extend(right.weights.iter().copied());
    let mut mchildren = left.children.clone();
    mchildren.extend(right.children.iter().cloned());

    let mut keys = node.keys.clone();
    let mut vals = node.vals.clone();
    let mut weights = node.weights.clone();
    let mut children = node.children.clone();

    match build(mkeys, mvals, mweights, mchildren, cap) {
        Ins::Whole(merged) => {
            keys.remove(j);
            vals.remove(j);
            weights.remove(j);
            children[j] = Child::Resident(merged);
            children.remove(j + 1);
            Ok(Node::new(keys, vals, weights, children))
        }
        Ins::Split {
            left,
            key,
            val,
            weight,
            right,
        } => {
            keys[j] = key;
            vals[j] = val;
            weights[j] = weight;
            children[j] = Child::Resident(left);
            children[j + 1] = Child::Resident(right);
            Ok(Node::new(keys, vals, weights, children))
        }
    }
}

/// In-order walk: child[0], key[0], child[1], key[1], …, key[n-1], child[n]. Clones each
/// `(key, row)` out (owned) — see [`PMap::iter`] for why the walk does not borrow. Faults each
/// `OnDisk` leaf through `src`; the faulted `Arc` is dropped as soon as its rows are copied out, so
/// the resident leaf set stays bounded by the pool, not the tree (pager.md §4).
fn collect(node: &Node, src: Option<&dyn LeafSource>, out: &mut Vec<(Vec<u8>, Row)>) -> Result<()> {
    if node.is_leaf() {
        for i in 0..node.keys.len() {
            out.push((node.keys[i].clone(), node.vals[i].clone()));
        }
        return Ok(());
    }
    for i in 0..node.keys.len() {
        let c = child(node, i, src)?;
        collect(&c, src, out)?;
        out.push((node.keys[i].clone(), node.vals[i].clone()));
    }
    let last = child(node, node.keys.len(), src)?;
    collect(&last, src, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;

    // A small page cap so a few-thousand-entry map is several levels deep — exercises split,
    // merge-then-split, root growth and collapse (the in-RAM analog of page_size 256).
    const CAP: usize = 244;

    fn row(n: i64) -> Row {
        vec![Value::Int(n)]
    }

    fn key(n: u64) -> Vec<u8> {
        n.to_be_bytes().to_vec()
    }

    /// A realistic per-entry weight: 8-byte key + a ~5-byte int value record ≈ 15 bytes, so a
    /// 244-byte node holds ~16 entries before splitting (well under RECORD_MAX = (244-12)/2 = 116).
    const W: u32 = 15;

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

    /// Every node (except the root) must fit a page and stay non-empty — the structural invariant
    /// the byte contract relies on (format.md). Checked over the whole tree.
    fn check_invariants(pm: &PMap) {
        fn walk(node: &Node, is_root: bool, cap: usize) {
            assert!(!node.keys.is_empty() || is_root, "non-root node is empty");
            assert_eq!(node.keys.len(), node.vals.len());
            assert_eq!(node.keys.len(), node.weights.len());
            if !node.is_leaf() {
                assert_eq!(
                    node.children.len(),
                    node.keys.len() + 1,
                    "interior child count"
                );
            }
            let payload: usize = node.weights.iter().map(|&w| w as usize).sum::<usize>()
                + if node.is_leaf() {
                    0
                } else {
                    4 * node.children.len()
                };
            assert!(payload <= cap, "node payload {payload} exceeds cap {cap}");
            for c in &node.children {
                walk(c.resident(), false, cap);
            }
        }
        if let Some(root) = &pm.root {
            walk(root, true, CAP);
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
                pm.insert(key(k), row(k as i64), W, CAP, None).unwrap(),
                bt.insert(key(k), row(k as i64))
            );
        }
        assert_eq!(pm.len(), bt.len());
        check_invariants(&pm);
        for k in 0..n {
            assert_eq!(pm.get(&key(k), None).unwrap().as_ref(), bt.get(&key(k)));
        }
        let got: Vec<_> = pm.iter(None).unwrap();
        let want: Vec<_> = bt.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(got, want);

        // Overwrite returns the old value and does not change len.
        let before = pm.len();
        assert_eq!(
            pm.insert(key(7), row(777), W, CAP, None).unwrap(),
            bt.insert(key(7), row(777))
        );
        assert_eq!(pm.len(), before);
        assert_eq!(pm.get(&key(7), None).unwrap(), Some(row(777)));

        // Interleave removes with invariant checks so merge-then-split is exercised mid-stream.
        for (step, k) in shuffled(n).into_iter().enumerate() {
            assert_eq!(pm.remove(&key(k), CAP, None).unwrap(), bt.remove(&key(k)));
            if step % 257 == 0 {
                check_invariants(&pm);
            }
        }
        assert!(pm.is_empty());
        assert_eq!(pm.iter(None).unwrap().len(), 0);
        assert_eq!(pm.remove(&key(123), CAP, None).unwrap(), None);
    }

    #[test]
    fn clone_is_an_independent_snapshot() {
        let mut base = PMap::new();
        for k in 0..2000 {
            base.insert(key(k), row(k as i64), W, CAP, None).unwrap();
        }
        let snap = base.clone();

        let mut other = base.clone();
        for k in 0..2000 {
            other
                .insert(key(k), row(-(k as i64)), W, CAP, None)
                .unwrap(); // overwrite every value
        }
        for k in 2000..3000 {
            other.insert(key(k), row(k as i64), W, CAP, None).unwrap(); // and grow it
        }
        for k in 0..500 {
            other.remove(&key(k), CAP, None).unwrap(); // and shrink it
        }

        // `snap` still sees the original contents, untouched.
        assert_eq!(snap.len(), 2000);
        for k in 0..2000 {
            assert_eq!(snap.get(&key(k), None).unwrap(), Some(row(k as i64)));
        }
        let snap_rows: Vec<_> = snap.iter(None).unwrap();
        assert_eq!(snap_rows.len(), 2000);
        assert_eq!(snap_rows[0], (key(0), row(0)));
        assert_eq!(snap_rows[1999], (key(1999), row(1999)));
        check_invariants(&snap);

        assert_eq!(other.len(), 2500);
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
        assert_eq!(pm.remove(&key(1), CAP, None).unwrap(), None);
        assert_eq!(pm.insert(key(1), row(1), W, CAP, None).unwrap(), None);
        assert_eq!(pm.get(&key(1), None).unwrap(), Some(row(1)));
        assert_eq!(pm.remove(&key(1), CAP, None).unwrap(), Some(row(1)));
        assert!(pm.is_empty());
        assert!(pm.root.is_none());
    }

    /// Wide values (near RECORD_MAX) force tiny fan-out — the stress case for the split point and
    /// the non-empty-halves guarantee. With weight 110 (≤ 116 cap), a node holds ~2 entries.
    #[test]
    fn wide_values_keep_nodes_valid() {
        use std::collections::BTreeMap;
        let mut pm = PMap::new();
        let mut bt: BTreeMap<Vec<u8>, Row> = BTreeMap::new();
        for k in shuffled(300) {
            pm.insert(key(k), row(k as i64), 110, CAP, None).unwrap();
            bt.insert(key(k), row(k as i64));
            check_invariants(&pm);
        }
        for k in shuffled(300) {
            pm.remove(&key(k), CAP, None).unwrap();
            bt.remove(&key(k));
            check_invariants(&pm);
        }
        assert!(pm.is_empty());
    }
}
