package jed

// Persistent (copy-on-write) ordered map — the in-memory store primitive (decision B1,
// spec/design/transactions.md §3).
//
// Keyed by the encoded key bytes (compared with bytes.Compare = memcmp = the order-preserving
// key encoding's contract, spec/design/encoding.md). Every mutation returns a new map that
// shares structure with the old one; nodes are immutable by convention (never mutated after
// construction), so copying a PMap value (which shares the root pointer) is an O(1) independent
// snapshot — mutating the copy leaves the original untouched. That cheap, structurally-shared
// snapshot carries the §3 staging-buffer / transaction model (transactions.md §2). The concrete
// shape is a copy-on-write B-tree: the in-memory precursor of the Phase-6 on-disk B-tree.
//
// Only the iteration order is a cross-core contract this slice; the in-RAM node shape (fan-out,
// split points) is private (transactions.md §3) — it becomes a byte contract only at Phase 6.
// Delete rebalances (Cormen's algorithm) so leaves stay non-empty.

import "bytes"

// btreeT is the minimum degree t: a node holds between t-1 and 2t-1 keys (the root may hold
// fewer) and overflows at 2t. Private tuning — it changes only the in-RAM shape, never order.
const (
	btreeT       = 16
	btreeMaxKeys = 2*btreeT - 1
	btreeMinKeys = btreeT - 1
)

// pnode is one B-tree node. children is empty for a leaf; otherwise len(children) ==
// len(keys)+1. len(keys) == len(vals) always. Nodes are never mutated after construction, so a
// mutation rebuilds only the root->leaf path and shares every untouched subtree.
type pnode struct {
	keys     [][]byte
	vals     []Row
	children []*pnode
}

func (n *pnode) isLeaf() bool { return len(n.children) == 0 }

// search returns (index, found): found ⇒ key is at keys[index]; else index is the child slot.
func (n *pnode) search(key []byte) (int, bool) {
	lo, hi := 0, len(n.keys)
	for lo < hi {
		mid := int(uint(lo+hi) >> 1)
		switch bytes.Compare(n.keys[mid], key) {
		case 0:
			return mid, true
		case -1:
			lo = mid + 1
		default:
			hi = mid
		}
	}
	return lo, false
}

// PMap is a persistent ordered map from encoded key to Row. A value copy is an O(1) independent
// snapshot (the root pointer is shared; nodes are immutable).
type PMap struct {
	root   *pnode
	length int
}

// NewPMap returns an empty map.
func NewPMap() PMap { return PMap{} }

// Len returns the entry count.
func (m *PMap) Len() int { return m.length }

// Get looks up the row at key.
func (m *PMap) Get(key []byte) (Row, bool) {
	n := m.root
	for n != nil {
		i, found := n.search(key)
		if found {
			return n.vals[i], true
		}
		if n.isLeaf() {
			return nil, false
		}
		n = n.children[i]
	}
	return nil, false
}

// Insert inserts or overwrites key. Returns the previous row and true if key was present (an
// overwrite); otherwise nil and false (a new insert, which grows the length).
func (m *PMap) Insert(key []byte, val Row) (Row, bool) {
	if m.root == nil {
		m.root = &pnode{keys: [][]byte{key}, vals: []Row{val}}
		m.length++
		return nil, false
	}
	var old Row
	replaced := false
	out := nodeInsert(m.root, key, val, &old, &replaced)
	if out.whole != nil {
		m.root = out.whole
	} else {
		m.root = &pnode{keys: [][]byte{out.midK}, vals: []Row{out.midV}, children: []*pnode{out.left, out.right}}
	}
	if !replaced {
		m.length++
	}
	return old, replaced
}

// Remove deletes key. Returns the removed row and true, or (nil,false) if absent (then the map
// is unchanged).
func (m *PMap) Remove(key []byte) (Row, bool) {
	if m.root == nil {
		return nil, false
	}
	newRoot, removed, ok := nodeRemove(m.root, key)
	if !ok {
		return nil, false
	}
	// The root may have drained to zero keys: an empty leaf becomes the empty map; an empty
	// internal node (one child) hands the root down a level (height shrinks).
	if len(newRoot.keys) == 0 {
		if newRoot.isLeaf() {
			m.root = nil
		} else {
			m.root = newRoot.children[0]
		}
	} else {
		m.root = newRoot
	}
	m.length--
	return removed, true
}

// inorder returns all (key, row) pairs in ascending key order. Eager (the cost contract charges
// per row in the executor loop, not here — spec/design/cost.md), so laziness is unobservable.
func (m *PMap) inorder() ([][]byte, []Row) {
	keys := make([][]byte, 0, m.length)
	vals := make([]Row, 0, m.length)
	var walk func(n *pnode)
	walk = func(n *pnode) {
		if n == nil {
			return
		}
		if n.isLeaf() {
			keys = append(keys, n.keys...)
			vals = append(vals, n.vals...)
			return
		}
		for i := range n.keys {
			walk(n.children[i])
			keys = append(keys, n.keys[i])
			vals = append(vals, n.vals[i])
		}
		walk(n.children[len(n.keys)])
	}
	walk(m.root)
	return keys, vals
}

// insOut is the result of inserting into a subtree: a whole rebuilt node, or a split.
type insOut struct {
	whole *pnode // non-nil ⇒ no split
	left  *pnode
	midK  []byte
	midV  Row
	right *pnode
}

// nodeInsert is the recursive insert. On overwrite it sets *old/*replaced and rebuilds the path
// with the value replaced (no split). On a new key it inserts into the leaf and splits
// overflowing nodes back up the path.
func nodeInsert(n *pnode, key []byte, val Row, old *Row, replaced *bool) insOut {
	i, found := n.search(key)
	if found {
		vals := cloneVals(n.vals)
		*old = vals[i]
		*replaced = true
		vals[i] = val
		return insOut{whole: &pnode{keys: cloneKeys(n.keys), vals: vals, children: cloneChildren(n.children)}}
	}
	if n.isLeaf() {
		return splitIfNeeded(insertKeyAt(n.keys, i, key), insertValAt(n.vals, i, val), nil)
	}
	child := nodeInsert(n.children[i], key, val, old, replaced)
	if child.whole != nil {
		children := cloneChildren(n.children)
		children[i] = child.whole
		return insOut{whole: &pnode{keys: cloneKeys(n.keys), vals: cloneVals(n.vals), children: children}}
	}
	keys := insertKeyAt(n.keys, i, child.midK)
	vals := insertValAt(n.vals, i, child.midV)
	children := cloneChildren(n.children)
	children[i] = child.left
	children = insertChildAt(children, i+1, child.right)
	return splitIfNeeded(keys, vals, children)
}

// splitIfNeeded builds a node from the parts; if it overflows (> 2t-1 keys) it splits at the
// midpoint and promotes the median. children empty ⇒ leaf. The split point is deterministic and
// (being in-RAM only) free to choose (transactions.md §3).
func splitIfNeeded(keys [][]byte, vals []Row, children []*pnode) insOut {
	if len(keys) <= btreeMaxKeys {
		return insOut{whole: &pnode{keys: keys, vals: vals, children: children}}
	}
	mid := len(keys) / 2
	leaf := len(children) == 0
	var lchildren, rchildren []*pnode
	if !leaf {
		lchildren = children[:mid+1]
		rchildren = children[mid+1:]
	}
	return insOut{
		left:  &pnode{keys: cloneKeys(keys[:mid]), vals: cloneVals(vals[:mid]), children: cloneChildren(lchildren)},
		midK:  keys[mid],
		midV:  vals[mid],
		right: &pnode{keys: cloneKeys(keys[mid+1:]), vals: cloneVals(vals[mid+1:]), children: cloneChildren(rchildren)},
	}
}

func canSpare(n *pnode) bool { return len(n.keys) > btreeMinKeys }

// minKV is the leftmost (smallest) entry of a subtree — its in-order successor.
func minKV(n *pnode) ([]byte, Row) {
	for !n.isLeaf() {
		n = n.children[0]
	}
	return n.keys[0], n.vals[0]
}

// maxKV is the rightmost (largest) entry of a subtree — its in-order predecessor.
func maxKV(n *pnode) ([]byte, Row) {
	for !n.isLeaf() {
		n = n.children[len(n.children)-1]
	}
	return n.keys[len(n.keys)-1], n.vals[len(n.vals)-1]
}

// nodeRemove is the recursive delete (Cormen's B-tree deletion, copy-on-write). It maintains the
// invariant that any node it descends into holds at least t keys, so a delete cannot underflow
// it — a key in an internal node is replaced by a predecessor/successor drawn from a child that
// can spare one (else the two children and the separator are merged first). That rebalancing
// keeps every leaf non-empty, so minKV/maxKV are always well-defined.
func nodeRemove(n *pnode, key []byte) (*pnode, Row, bool) {
	i, found := n.search(key)
	if found {
		if n.isLeaf() {
			vals, removed := removeValAt(n.vals, i)
			return &pnode{keys: removeKeyAt(n.keys, i), vals: vals}, removed, true
		}
		removed := n.vals[i]
		if canSpare(n.children[i]) {
			pk, pv := maxKV(n.children[i])
			newChild, _, _ := nodeRemove(n.children[i], pk)
			keys := cloneKeys(n.keys)
			vals := cloneVals(n.vals)
			children := cloneChildren(n.children)
			keys[i], vals[i], children[i] = pk, pv, newChild
			return &pnode{keys: keys, vals: vals, children: children}, removed, true
		}
		if canSpare(n.children[i+1]) {
			sk, sv := minKV(n.children[i+1])
			newChild, _, _ := nodeRemove(n.children[i+1], sk)
			keys := cloneKeys(n.keys)
			vals := cloneVals(n.vals)
			children := cloneChildren(n.children)
			keys[i], vals[i], children[i+1] = sk, sv, newChild
			return &pnode{keys: keys, vals: vals, children: children}, removed, true
		}
		newParent, _, _ := finishDescend(mergeAt(n, i), i, key)
		return newParent, removed, true
	}
	if n.isLeaf() {
		return n, nil, false
	}
	return descendRemove(n, i, key)
}

// descendRemove descends into child i to delete key, first ensuring that child holds at least t
// keys — borrow from a sibling that can spare it, else merge with a sibling.
func descendRemove(n *pnode, i int, key []byte) (*pnode, Row, bool) {
	if len(n.children[i].keys) >= btreeT {
		return finishDescend(n, i, key)
	}
	if i > 0 && canSpare(n.children[i-1]) {
		return finishDescend(borrowFromLeft(n, i), i, key)
	}
	if i+1 < len(n.children) && canSpare(n.children[i+1]) {
		return finishDescend(borrowFromRight(n, i), i, key)
	}
	if i > 0 {
		return finishDescend(mergeAt(n, i-1), i-1, key)
	}
	return finishDescend(mergeAt(n, i), i, key)
}

// finishDescend recurses into child i (now guaranteed >= t keys) and splices the result back in.
func finishDescend(n *pnode, i int, key []byte) (*pnode, Row, bool) {
	newChild, removed, ok := nodeRemove(n.children[i], key)
	if !ok {
		return n, nil, false
	}
	children := cloneChildren(n.children)
	children[i] = newChild
	return &pnode{keys: cloneKeys(n.keys), vals: cloneVals(n.vals), children: children}, removed, true
}

// borrowFromLeft: child i borrows a key from its left sibling, rotating through separator i-1.
func borrowFromLeft(n *pnode, i int) *pnode {
	left := n.children[i-1]
	cur := n.children[i]

	upKey := left.keys[len(left.keys)-1]
	upVal := left.vals[len(left.vals)-1]
	newLeftKeys := cloneKeys(left.keys[:len(left.keys)-1])
	newLeftVals := cloneVals(left.vals[:len(left.vals)-1])
	var newLeftChildren, newCurChildren []*pnode
	if !left.isLeaf() {
		newLeftChildren = cloneChildren(left.children[:len(left.children)-1])
		newCurChildren = insertChildAt(cur.children, 0, left.children[len(left.children)-1])
	}

	newCurKeys := insertKeyAt(cur.keys, 0, n.keys[i-1])
	newCurVals := insertValAt(cur.vals, 0, n.vals[i-1])

	keys := cloneKeys(n.keys)
	vals := cloneVals(n.vals)
	children := cloneChildren(n.children)
	keys[i-1], vals[i-1] = upKey, upVal
	children[i-1] = &pnode{keys: newLeftKeys, vals: newLeftVals, children: newLeftChildren}
	children[i] = &pnode{keys: newCurKeys, vals: newCurVals, children: newCurChildren}
	return &pnode{keys: keys, vals: vals, children: children}
}

// borrowFromRight: child i borrows a key from its right sibling, rotating through separator i.
func borrowFromRight(n *pnode, i int) *pnode {
	cur := n.children[i]
	right := n.children[i+1]

	upKey := right.keys[0]
	upVal := right.vals[0]
	newRightKeys := cloneKeys(right.keys[1:])
	newRightVals := cloneVals(right.vals[1:])
	var newRightChildren, newCurChildren []*pnode
	if !right.isLeaf() {
		newRightChildren = cloneChildren(right.children[1:])
		newCurChildren = insertChildAt(cur.children, len(cur.children), right.children[0])
	}

	newCurKeys := insertKeyAt(cur.keys, len(cur.keys), n.keys[i])
	newCurVals := insertValAt(cur.vals, len(cur.vals), n.vals[i])

	keys := cloneKeys(n.keys)
	vals := cloneVals(n.vals)
	children := cloneChildren(n.children)
	keys[i], vals[i] = upKey, upVal
	children[i] = &pnode{keys: newCurKeys, vals: newCurVals, children: newCurChildren}
	children[i+1] = &pnode{keys: newRightKeys, vals: newRightVals, children: newRightChildren}
	return &pnode{keys: keys, vals: vals, children: children}
}

// mergeAt merges children[i], separator i, and children[i+1] into one node (2t-1 keys), and
// removes the separator and the absorbed right child from this node.
func mergeAt(n *pnode, i int) *pnode {
	left := n.children[i]
	right := n.children[i+1]

	mkeys := make([][]byte, 0, len(left.keys)+1+len(right.keys))
	mkeys = append(mkeys, left.keys...)
	mkeys = append(mkeys, n.keys[i])
	mkeys = append(mkeys, right.keys...)
	mvals := make([]Row, 0, len(left.vals)+1+len(right.vals))
	mvals = append(mvals, left.vals...)
	mvals = append(mvals, n.vals[i])
	mvals = append(mvals, right.vals...)
	var mchildren []*pnode
	if !left.isLeaf() {
		mchildren = make([]*pnode, 0, len(left.children)+len(right.children))
		mchildren = append(mchildren, left.children...)
		mchildren = append(mchildren, right.children...)
	}
	merged := &pnode{keys: mkeys, vals: mvals, children: mchildren}

	keys := removeKeyAt(n.keys, i)
	vals, _ := removeValAt(n.vals, i)
	children := cloneChildren(n.children)
	children[i] = merged
	children = removeChildAt(children, i+1)
	return &pnode{keys: keys, vals: vals, children: children}
}

// --- immutable slice helpers (each returns a fresh slice, leaving the input untouched) -------

func cloneKeys(s [][]byte) [][]byte {
	if len(s) == 0 {
		return nil
	}
	out := make([][]byte, len(s))
	copy(out, s)
	return out
}

func cloneVals(s []Row) []Row {
	if len(s) == 0 {
		return nil
	}
	out := make([]Row, len(s))
	copy(out, s)
	return out
}

func cloneChildren(s []*pnode) []*pnode {
	if len(s) == 0 {
		return nil
	}
	out := make([]*pnode, len(s))
	copy(out, s)
	return out
}

func insertKeyAt(s [][]byte, i int, x []byte) [][]byte {
	out := make([][]byte, len(s)+1)
	copy(out, s[:i])
	out[i] = x
	copy(out[i+1:], s[i:])
	return out
}

func insertValAt(s []Row, i int, x Row) []Row {
	out := make([]Row, len(s)+1)
	copy(out, s[:i])
	out[i] = x
	copy(out[i+1:], s[i:])
	return out
}

func insertChildAt(s []*pnode, i int, x *pnode) []*pnode {
	out := make([]*pnode, len(s)+1)
	copy(out, s[:i])
	out[i] = x
	copy(out[i+1:], s[i:])
	return out
}

func removeKeyAt(s [][]byte, i int) [][]byte {
	out := make([][]byte, len(s)-1)
	copy(out, s[:i])
	copy(out[i:], s[i+1:])
	return out
}

func removeValAt(s []Row, i int) ([]Row, Row) {
	removed := s[i]
	out := make([]Row, len(s)-1)
	copy(out, s[:i])
	copy(out[i:], s[i+1:])
	return out, removed
}

func removeChildAt(s []*pnode, i int) []*pnode {
	out := make([]*pnode, len(s)-1)
	copy(out, s[:i])
	copy(out[i:], s[i+1:])
	return out
}
