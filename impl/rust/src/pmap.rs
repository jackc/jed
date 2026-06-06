//! Persistent (copy-on-write) ordered map — the in-memory store primitive
//! (decision **B1**, spec/design/transactions.md §3).
//!
//! Keyed by the encoded key bytes (`Vec<u8>`, whose `Ord` is lexicographic = the
//! order-preserving key encoding's memcmp contract, spec/design/encoding.md). Every
//! mutation returns a **new** map that shares structure with the old one — the old root
//! is provably unchanged — so a snapshot is an O(1) `Arc` clone and a commit is a pointer
//! swap (transactions.md §2). The concrete shape is a **copy-on-write B-tree**: the
//! in-memory precursor of the Phase-6 on-disk B-tree, chosen so that page-backing it later
//! is an additive change rather than a rebuild (transactions.md §3, TODO Phase 6).
//!
//! Only the **iteration order** is a cross-core contract this slice; the in-RAM node
//! structure (fan-out, split points) is a private detail (transactions.md §3) — it becomes
//! a byte contract only at Phase 6 when the tree is the on-disk format.
//!
//! Boring and explicit (CLAUDE.md §10): one `Node` type (a leaf has no children), recursive
//! insert with split-on-overflow, recursive delete via in-order-successor replacement.
//! **Delete does not rebalance** this slice (a leaf may end underfull or empty) — correct
//! for search and iteration; balance-on-delete is a deferred hardening (it never affects the
//! logical contents or order, only node occupancy).

use std::sync::Arc;

use crate::storage::Row;

/// Minimum degree `t`: every node holds between `t-1` and `2t-1` keys (the root may hold
/// fewer). A node overflows at `2t` keys and is split. The value is a private tuning knob —
/// it changes only the in-RAM shape, never the observable order (transactions.md §3).
const T: usize = 16;
const MAX_KEYS: usize = 2 * T - 1;

/// One B-tree node. `children` is empty for a leaf; otherwise `children.len() ==
/// keys.len() + 1`. `keys.len() == vals.len()` always. Nodes are shared behind `Arc`, so a
/// mutation clones only the root→leaf path and shares every untouched subtree.
struct Node {
    keys: Vec<Vec<u8>>,
    vals: Vec<Row>,
    children: Vec<Arc<Node>>,
}

impl Node {
    fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }

    /// Binary-search this node's keys: `Ok(i)` if `key` sits at index `i`, else `Err(i)` for
    /// the child/insertion slot. `Vec<u8>::cmp` is lexicographic (memcmp) — the key contract.
    fn search(&self, key: &[u8]) -> std::result::Result<usize, usize> {
        self.keys.binary_search_by(|k| k.as_slice().cmp(key))
    }
}

/// The result of inserting into a subtree: either the rebuilt subtree, or a node that
/// overflowed and split into `left`, a median `(key,val)` to promote, and `right`.
enum Ins {
    Whole(Arc<Node>),
    Split {
        left: Arc<Node>,
        key: Vec<u8>,
        val: Row,
        right: Arc<Node>,
    },
}

/// A persistent ordered map from encoded key to [`Row`]. `Clone` is O(1) (an `Arc` bump on
/// the root plus a length copy) and yields an independent snapshot: mutating the clone leaves
/// this map untouched.
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

    /// Look up the row at `key`, or `None`.
    pub fn get(&self, key: &[u8]) -> Option<&Row> {
        let mut node = self.root.as_deref()?;
        loop {
            match node.search(key) {
                Ok(i) => return Some(&node.vals[i]),
                Err(i) => {
                    if node.is_leaf() {
                        return None;
                    }
                    node = &node.children[i];
                }
            }
        }
    }

    /// Insert or overwrite `key`. Returns the previous row if `key` was present (an
    /// overwrite), else `None` (a new insert, which grows `len`).
    pub fn insert(&mut self, key: Vec<u8>, val: Row) -> Option<Row> {
        let mut old = None;
        let new_root = match &self.root {
            None => Node::leaf(vec![key], vec![val]),
            Some(root) => match node_insert(root, key, val, &mut old) {
                Ins::Whole(n) => n,
                Ins::Split {
                    left,
                    key,
                    val,
                    right,
                } => Arc::new(Node {
                    keys: vec![key],
                    vals: vec![val],
                    children: vec![left, right],
                }),
            },
        };
        self.root = Some(new_root);
        if old.is_none() {
            self.len += 1;
        }
        old
    }

    /// Remove `key`. Returns the removed row, or `None` if absent (then `self` is unchanged).
    pub fn remove(&mut self, key: &[u8]) -> Option<Row> {
        let root = self.root.as_ref()?;
        let (new_root, removed) = node_remove(root, key);
        if removed.is_some() {
            // The root may have drained to zero keys: an empty leaf becomes the empty map; an
            // empty internal node (one child) hands the root down a level (height shrinks).
            self.root = if new_root.keys.is_empty() {
                if new_root.is_leaf() {
                    None
                } else {
                    Some(new_root.children[0].clone())
                }
            } else {
                Some(new_root)
            };
            self.len -= 1;
        }
        removed
    }

    /// Iterate `(key, row)` pairs in ascending key order. Eagerly walks the tree into a
    /// vector of borrows (the cost contract charges per row in the executor loop, not here —
    /// spec/design/cost.md), so laziness is unobservable and a deferred optimization.
    pub fn iter(&self) -> impl Iterator<Item = (&Vec<u8>, &Row)> + '_ {
        let mut out = Vec::with_capacity(self.len);
        if let Some(root) = &self.root {
            collect(root, &mut out);
        }
        out.into_iter()
    }
}

impl Node {
    fn leaf(keys: Vec<Vec<u8>>, vals: Vec<Row>) -> Arc<Node> {
        Arc::new(Node {
            keys,
            vals,
            children: Vec::new(),
        })
    }
}

/// Recursive insert. On overwrite, sets `*old` and rebuilds the path with the value
/// replaced (no count change, so never splits). On a new key, inserts into the target leaf
/// and splits overflowing nodes back up the path.
fn node_insert(node: &Arc<Node>, key: Vec<u8>, val: Row, old: &mut Option<Row>) -> Ins {
    match node.search(&key) {
        Ok(i) => {
            let mut vals = node.vals.clone();
            *old = Some(std::mem::replace(&mut vals[i], val));
            Ins::Whole(Arc::new(Node {
                keys: node.keys.clone(),
                vals,
                children: node.children.clone(),
            }))
        }
        Err(i) => {
            if node.is_leaf() {
                let mut keys = node.keys.clone();
                let mut vals = node.vals.clone();
                keys.insert(i, key);
                vals.insert(i, val);
                split_if_needed(keys, vals, Vec::new())
            } else {
                match node_insert(&node.children[i], key, val, old) {
                    Ins::Whole(c) => {
                        let mut children = node.children.clone();
                        children[i] = c;
                        Ins::Whole(Arc::new(Node {
                            keys: node.keys.clone(),
                            vals: node.vals.clone(),
                            children,
                        }))
                    }
                    Ins::Split {
                        left,
                        key: mk,
                        val: mv,
                        right,
                    } => {
                        let mut keys = node.keys.clone();
                        let mut vals = node.vals.clone();
                        let mut children = node.children.clone();
                        keys.insert(i, mk);
                        vals.insert(i, mv);
                        children[i] = left;
                        children.insert(i + 1, right);
                        split_if_needed(keys, vals, children)
                    }
                }
            }
        }
    }
}

/// Build a node from `keys`/`vals`/`children`; if it overflows (`> 2t-1` keys), split it at
/// the midpoint and promote the median. `children` empty ⇒ leaf. The split point is
/// `keys.len()/2` — deterministic, and (being in-RAM only) free to choose (transactions.md §3).
fn split_if_needed(
    mut keys: Vec<Vec<u8>>,
    mut vals: Vec<Row>,
    mut children: Vec<Arc<Node>>,
) -> Ins {
    if keys.len() <= MAX_KEYS {
        return Ins::Whole(Arc::new(Node {
            keys,
            vals,
            children,
        }));
    }
    let mid = keys.len() / 2;
    let leaf = children.is_empty();

    // `split_off(mid+1)` leaves the left half plus the median; `pop` lifts the median out.
    let rkeys = keys.split_off(mid + 1);
    let mkey = keys.pop().unwrap();
    let rvals = vals.split_off(mid + 1);
    let mval = vals.pop().unwrap();

    let (lchildren, rchildren) = if leaf {
        (Vec::new(), Vec::new())
    } else {
        let rc = children.split_off(mid + 1);
        (children, rc)
    };

    Ins::Split {
        left: Arc::new(Node {
            keys,
            vals,
            children: lchildren,
        }),
        key: mkey,
        val: mval,
        right: Arc::new(Node {
            keys: rkeys,
            vals: rvals,
            children: rchildren,
        }),
    }
}

/// Minimum keys a non-root node may hold. A node "can spare" a key when it holds strictly
/// more, so handing one to a sibling still leaves it valid.
const MIN_KEYS: usize = T - 1;

fn can_spare(node: &Node) -> bool {
    node.keys.len() > MIN_KEYS
}

/// The leftmost (smallest) `(key, val)` of a subtree — its in-order successor entry.
fn min_kv(node: &Arc<Node>) -> (Vec<u8>, Row) {
    let mut n = node;
    while !n.is_leaf() {
        n = &n.children[0];
    }
    (n.keys[0].clone(), n.vals[0].clone())
}

/// The rightmost (largest) `(key, val)` of a subtree — its in-order predecessor entry.
fn max_kv(node: &Arc<Node>) -> (Vec<u8>, Row) {
    let mut n = node;
    while !n.is_leaf() {
        n = n.children.last().unwrap();
    }
    (
        n.keys.last().unwrap().clone(),
        n.vals.last().unwrap().clone(),
    )
}

/// Recursive delete (Cormen's B-tree deletion, copy-on-write). Returns the rebuilt subtree
/// and the removed row (or `None` if absent). Maintains the invariant that any node it
/// descends into holds at least `T` keys, so the deletion can never underflow it — a key in
/// an internal node is replaced by a predecessor/successor drawn from a child that can spare
/// one (else the two children and the separator are merged first). This rebalancing is what
/// keeps every leaf non-empty, so [`min_kv`]/[`max_kv`] are always well-defined.
fn node_remove(node: &Arc<Node>, key: &[u8]) -> (Arc<Node>, Option<Row>) {
    match node.search(key) {
        Ok(i) => {
            if node.is_leaf() {
                let mut keys = node.keys.clone();
                let mut vals = node.vals.clone();
                keys.remove(i);
                let removed = vals.remove(i);
                (Node::leaf(keys, vals), Some(removed))
            } else {
                let removed = node.vals[i].clone();
                if can_spare(&node.children[i]) {
                    // Replace with the predecessor, then delete it from the left subtree.
                    let (pk, pv) = max_kv(&node.children[i]);
                    let (new_child, _) = node_remove(&node.children[i], &pk);
                    let mut keys = node.keys.clone();
                    let mut vals = node.vals.clone();
                    let mut children = node.children.clone();
                    keys[i] = pk;
                    vals[i] = pv;
                    children[i] = new_child;
                    (rebuild(keys, vals, children), Some(removed))
                } else if can_spare(&node.children[i + 1]) {
                    // Replace with the successor, then delete it from the right subtree.
                    let (sk, sv) = min_kv(&node.children[i + 1]);
                    let (new_child, _) = node_remove(&node.children[i + 1], &sk);
                    let mut keys = node.keys.clone();
                    let mut vals = node.vals.clone();
                    let mut children = node.children.clone();
                    keys[i] = sk;
                    vals[i] = sv;
                    children[i + 1] = new_child;
                    (rebuild(keys, vals, children), Some(removed))
                } else {
                    // Both children are minimal: merge them around the separator, then delete.
                    let parent = merge_at(node, i);
                    let (new_parent, _) = finish_descend(parent, i, key);
                    (new_parent, Some(removed))
                }
            }
        }
        Err(i) => {
            if node.is_leaf() {
                (node.clone(), None)
            } else {
                descend_remove(node, i, key)
            }
        }
    }
}

/// Descend into child `i` to delete `key`, first ensuring that child holds at least `T` keys
/// — borrow one from an adjacent sibling that can spare it, else merge with a sibling (which
/// shrinks this node by one key and one child).
fn descend_remove(node: &Arc<Node>, i: usize, key: &[u8]) -> (Arc<Node>, Option<Row>) {
    if node.children[i].keys.len() >= T {
        finish_descend(node.clone(), i, key)
    } else if i > 0 && can_spare(&node.children[i - 1]) {
        finish_descend(borrow_from_left(node, i), i, key)
    } else if i + 1 < node.children.len() && can_spare(&node.children[i + 1]) {
        finish_descend(borrow_from_right(node, i), i, key)
    } else if i > 0 {
        // Merge with the left sibling; the merged child lands at i-1.
        finish_descend(merge_at(node, i - 1), i - 1, key)
    } else {
        // Merge with the right sibling; the merged child stays at i.
        finish_descend(merge_at(node, i), i, key)
    }
}

/// Recurse into child `i` (now guaranteed `>= T` keys) and splice the result back in.
fn finish_descend(node: Arc<Node>, i: usize, key: &[u8]) -> (Arc<Node>, Option<Row>) {
    let (new_child, removed) = node_remove(&node.children[i], key);
    if removed.is_none() {
        return (node, None);
    }
    let mut children = node.children.clone();
    children[i] = new_child;
    (
        rebuild(node.keys.clone(), node.vals.clone(), children),
        removed,
    )
}

/// Child `i` borrows a key from its left sibling, rotating through separator `i-1`.
fn borrow_from_left(node: &Arc<Node>, i: usize) -> Arc<Node> {
    let left = &node.children[i - 1];
    let cur = &node.children[i];

    let mut lkeys = left.keys.clone();
    let mut lvals = left.vals.clone();
    let mut lchildren = left.children.clone();
    let up_key = lkeys.pop().unwrap();
    let up_val = lvals.pop().unwrap();
    let moved = if left.is_leaf() {
        None
    } else {
        lchildren.pop()
    };

    let mut ckeys = cur.keys.clone();
    let mut cvals = cur.vals.clone();
    let mut cchildren = cur.children.clone();
    ckeys.insert(0, node.keys[i - 1].clone());
    cvals.insert(0, node.vals[i - 1].clone());
    if let Some(c) = moved {
        cchildren.insert(0, c);
    }

    let mut keys = node.keys.clone();
    let mut vals = node.vals.clone();
    let mut children = node.children.clone();
    keys[i - 1] = up_key;
    vals[i - 1] = up_val;
    children[i - 1] = rebuild(lkeys, lvals, lchildren);
    children[i] = rebuild(ckeys, cvals, cchildren);
    rebuild(keys, vals, children)
}

/// Child `i` borrows a key from its right sibling, rotating through separator `i`.
fn borrow_from_right(node: &Arc<Node>, i: usize) -> Arc<Node> {
    let cur = &node.children[i];
    let right = &node.children[i + 1];

    let mut rkeys = right.keys.clone();
    let mut rvals = right.vals.clone();
    let mut rchildren = right.children.clone();
    let up_key = rkeys.remove(0);
    let up_val = rvals.remove(0);
    let moved = if right.is_leaf() {
        None
    } else {
        Some(rchildren.remove(0))
    };

    let mut ckeys = cur.keys.clone();
    let mut cvals = cur.vals.clone();
    let mut cchildren = cur.children.clone();
    ckeys.push(node.keys[i].clone());
    cvals.push(node.vals[i].clone());
    if let Some(c) = moved {
        cchildren.push(c);
    }

    let mut keys = node.keys.clone();
    let mut vals = node.vals.clone();
    let mut children = node.children.clone();
    keys[i] = up_key;
    vals[i] = up_val;
    children[i] = rebuild(ckeys, cvals, cchildren);
    children[i + 1] = rebuild(rkeys, rvals, rchildren);
    rebuild(keys, vals, children)
}

/// Merge `children[i]`, separator `i`, and `children[i+1]` into one node (`2t-1` keys), and
/// remove the separator and the now-absorbed right child from this node.
fn merge_at(node: &Arc<Node>, i: usize) -> Arc<Node> {
    let left = &node.children[i];
    let right = &node.children[i + 1];

    let mut mkeys = left.keys.clone();
    let mut mvals = left.vals.clone();
    let mut mchildren = left.children.clone();
    mkeys.push(node.keys[i].clone());
    mvals.push(node.vals[i].clone());
    mkeys.extend(right.keys.iter().cloned());
    mvals.extend(right.vals.iter().cloned());
    mchildren.extend(right.children.iter().cloned());
    let merged = rebuild(mkeys, mvals, mchildren);

    let mut keys = node.keys.clone();
    let mut vals = node.vals.clone();
    let mut children = node.children.clone();
    keys.remove(i);
    vals.remove(i);
    children[i] = merged;
    children.remove(i + 1);
    rebuild(keys, vals, children)
}

/// Allocate a node from its parts (leaf iff `children` is empty).
fn rebuild(keys: Vec<Vec<u8>>, vals: Vec<Row>, children: Vec<Arc<Node>>) -> Arc<Node> {
    Arc::new(Node {
        keys,
        vals,
        children,
    })
}

/// In-order walk: child[0], key[0], child[1], key[1], …, key[n-1], child[n].
fn collect<'a>(node: &'a Node, out: &mut Vec<(&'a Vec<u8>, &'a Row)>) {
    if node.is_leaf() {
        for i in 0..node.keys.len() {
            out.push((&node.keys[i], &node.vals[i]));
        }
        return;
    }
    for i in 0..node.keys.len() {
        collect(&node.children[i], out);
        out.push((&node.keys[i], &node.vals[i]));
    }
    collect(&node.children[node.keys.len()], out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Value;

    fn row(n: i64) -> Row {
        vec![Value::Int(n)]
    }

    fn key(n: u64) -> Vec<u8> {
        n.to_be_bytes().to_vec()
    }

    /// A deterministic permutation of `0..n` (an LCG-driven shuffle) — no wall-clock / RNG, so
    /// the test is reproducible (CLAUDE.md §10).
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

    #[test]
    fn insert_get_remove_against_reference() {
        use std::collections::BTreeMap;
        let mut pm = PMap::new();
        let mut bt: BTreeMap<Vec<u8>, Row> = BTreeMap::new();
        let n = 4000;

        for k in shuffled(n) {
            assert_eq!(
                pm.insert(key(k), row(k as i64)),
                bt.insert(key(k), row(k as i64))
            );
        }
        assert_eq!(pm.len(), bt.len());
        for k in 0..n {
            assert_eq!(pm.get(&key(k)), bt.get(&key(k)));
        }
        // Iteration is in ascending key order and matches the reference exactly.
        let got: Vec<_> = pm.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let want: Vec<_> = bt.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(got, want);

        // Overwrite returns the old value and does not change len (kept in sync with the
        // reference so the remove loop below still matches).
        let before = pm.len();
        assert_eq!(pm.insert(key(7), row(777)), bt.insert(key(7), row(777)));
        assert_eq!(pm.len(), before);
        assert_eq!(pm.get(&key(7)), Some(&row(777)));

        for k in shuffled(n) {
            assert_eq!(pm.remove(&key(k)), bt.remove(&key(k)));
        }
        assert!(pm.is_empty());
        assert_eq!(pm.iter().count(), 0);
        assert_eq!(pm.remove(&key(123)), None);
    }

    #[test]
    fn clone_is_an_independent_snapshot() {
        // Build a base big enough to be several levels deep.
        let mut base = PMap::new();
        for k in 0..2000 {
            base.insert(key(k), row(k as i64));
        }
        let snap = base.clone();

        // Mutate the clone heavily; the snapshot must be byte-for-byte unchanged.
        let mut other = base.clone();
        for k in 0..2000 {
            other.insert(key(k), row(-(k as i64))); // overwrite every value
        }
        for k in 2000..3000 {
            other.insert(key(k), row(k as i64)); // and grow it
        }
        for k in 0..500 {
            other.remove(&key(k)); // and shrink it
        }

        // `snap` still sees the original contents.
        assert_eq!(snap.len(), 2000);
        for k in 0..2000 {
            assert_eq!(snap.get(&key(k)), Some(&row(k as i64)));
        }
        let snap_rows: Vec<_> = snap.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(snap_rows.len(), 2000);
        assert_eq!(snap_rows[0], (key(0), row(0)));
        assert_eq!(snap_rows[1999], (key(1999), row(1999)));

        // `other` reflects all of its own edits.
        assert_eq!(other.len(), 2500);
        assert_eq!(other.get(&key(0)), None);
        assert_eq!(other.get(&key(1000)), Some(&row(-1000)));
        assert_eq!(other.get(&key(2500)), Some(&row(2500)));
    }

    #[test]
    fn empty_and_single() {
        let mut pm = PMap::new();
        assert!(pm.is_empty());
        assert_eq!(pm.get(&key(1)), None);
        assert_eq!(pm.remove(&key(1)), None);
        assert_eq!(pm.insert(key(1), row(1)), None);
        assert_eq!(pm.get(&key(1)), Some(&row(1)));
        assert_eq!(pm.remove(&key(1)), Some(row(1)));
        assert!(pm.is_empty());
        assert!(pm.root.is_none());
    }
}
