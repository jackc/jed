package jed

// Persistent (copy-on-write) ordered map — the page-backed B-tree (decision B1,
// spec/design/transactions.md §3; spec/fileformat/format.md "The per-table data B-tree").
//
// Keyed by the encoded key bytes (compared with bytes.Compare = memcmp = the order-preserving
// key encoding's contract, spec/design/encoding.md). Every mutation returns a new map that shares
// structure with the old one; nodes are immutable by convention (never mutated after construction),
// so copying a PMap value (which shares the root pointer) is an O(1) independent snapshot. That
// cheap, structurally-shared snapshot carries the §3 staging-buffer / transaction model.
//
// Since Phase 6 (P6.1) this IS the on-disk B-tree, node-for-page: its fan-out is size-driven — a
// node holds as many entries as fit a page payload cap (= page_size − 12) and splits when it would
// overflow, so the node boundaries (and serialized bytes) are a §8 byte contract (format.md). The
// caller supplies each entry's on-disk weight (record size) so this map can sum payloads without
// knowing the value codec; cap is passed per call (held by the TableStore). Each node also carries
// a set-once on-disk page id (0 = dirty) for the incremental commit (P6.1 part B). Delete rebalances
// by merge-then-maybe-split (no borrow — merge subsumes it; format.md "Delete").

import "bytes"

// pnode is one B-tree node. children is empty for a leaf; otherwise len(children) == len(keys)+1.
// len(keys) == len(vals) == len(weights) always. weights[i] is entry i's on-disk record size, used
// only for the size-driven split/merge. Nodes are never mutated after construction. page is the
// on-disk page index (0 when dirty), set once at the commit that first persists this node.
type pnode struct {
	keys     [][]byte
	vals     []Row
	weights  []uint32
	children []*pnode
	page     uint32
}

func (n *pnode) isLeaf() bool { return len(n.children) == 0 }

// payload is this node's serialized size (format.md): Σ weights plus, for an interior node,
// 4·(N+1) for its child pointers.
func (n *pnode) payload() int {
	total := 0
	for _, w := range n.weights {
		total += int(w)
	}
	if !n.isLeaf() {
		total += 4 * len(n.children)
	}
	return total
}

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

// root exposes the root node to the serializer (format.go). nil for an empty map.
func (m *PMap) rootNode() *pnode { return m.root }

// fromLoaded reconstructs a map from a loaded root (format.go LoadDatabase).
func fromLoaded(root *pnode, length int) PMap { return PMap{root: root, length: length} }

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

// Insert inserts or overwrites key with val (on-disk record size weight); cap is the page payload
// capacity. Returns the previous row and true if key was present (an overwrite); otherwise nil and
// false (a new insert, which grows the length). An overwrite can change the weight, so it too may
// overflow and split.
func (m *PMap) Insert(key []byte, val Row, weight uint32, cap int) (Row, bool) {
	if m.root == nil {
		m.root = &pnode{keys: [][]byte{key}, vals: []Row{val}, weights: []uint32{weight}}
		m.length++
		return nil, false
	}
	var old Row
	replaced := false
	out := nodeInsert(m.root, key, val, weight, &old, &replaced, cap)
	if out.whole != nil {
		m.root = out.whole
	} else {
		m.root = &pnode{
			keys: [][]byte{out.midK}, vals: []Row{out.midV}, weights: []uint32{out.midW},
			children: []*pnode{out.left, out.right},
		}
	}
	if !replaced {
		m.length++
	}
	return old, replaced
}

// Remove deletes key. Returns the removed row and true, or (nil,false) if absent (then the map is
// unchanged). cap is the page payload capacity (the rebalance threshold).
func (m *PMap) Remove(key []byte, cap int) (Row, bool) {
	if m.root == nil {
		return nil, false
	}
	newRoot, removed, ok := nodeRemove(m.root, key, cap)
	if !ok {
		return nil, false
	}
	// The root may have drained to zero keys: an empty leaf becomes the empty map; an empty internal
	// node (one child) hands the root down a level (height shrinks). The root is exempt from the
	// underfull rule, so no rebalance here.
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

// inorder returns all (key, row) pairs in ascending key order. Eager (the cost contract charges per
// row in the executor loop, not here — spec/design/cost.md), so laziness is unobservable.
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
	midW  uint32
	right *pnode
}

// build constructs a node from the parts; if its payload overflows cap it splits 2-way and promotes
// one median. The split point m = min(largest m in [1,N-1] with leftpayload(m) ≤ cap, N-2) always
// yields two non-empty, fitting halves under the RECORD_MAX = (cap-12)/2 cap (format.md). The < 3
// guard is defensive against an oversized record — it leaves the node whole, and the oversize is
// surfaced as 0A000 when the node is serialized (format.go).
func build(keys [][]byte, vals []Row, weights []uint32, children []*pnode, cap int) insOut {
	interior := len(children) > 0
	payload := 0
	for _, w := range weights {
		payload += int(w)
	}
	if interior {
		payload += 4 * len(children)
	}
	if payload <= cap || len(keys) < 3 {
		return insOut{whole: &pnode{keys: keys, vals: vals, weights: weights, children: children}}
	}

	n := len(keys)
	best := 1
	prefix := 0
	for m := 1; m < n; m++ {
		prefix += int(weights[m-1])
		lp := prefix
		if interior {
			lp += 4 * (m + 1)
		}
		if lp <= cap {
			best = m
		}
	}
	m := best
	if n-2 < m {
		m = n - 2
	}
	if m < 1 {
		m = 1
	}

	var lchildren, rchildren []*pnode
	if interior {
		lchildren = cloneChildren(children[:m+1])
		rchildren = cloneChildren(children[m+1:])
	}
	return insOut{
		left: &pnode{
			keys: cloneKeys(keys[:m]), vals: cloneVals(vals[:m]),
			weights: cloneWeights(weights[:m]), children: lchildren,
		},
		midK: keys[m], midV: vals[m], midW: weights[m],
		right: &pnode{
			keys: cloneKeys(keys[m+1:]), vals: cloneVals(vals[m+1:]),
			weights: cloneWeights(weights[m+1:]), children: rchildren,
		},
	}
}

// nodeInsert is the recursive insert. On overwrite it sets *old/*replaced and rebuilds the path with
// the value+weight replaced (which may now overflow). On a new key it inserts into the leaf and
// splits overflowing nodes back up the path.
func nodeInsert(n *pnode, key []byte, val Row, weight uint32, old *Row, replaced *bool, cap int) insOut {
	i, found := n.search(key)
	if found {
		vals := cloneVals(n.vals)
		weights := cloneWeights(n.weights)
		*old = vals[i]
		*replaced = true
		vals[i] = val
		weights[i] = weight
		return build(cloneKeys(n.keys), vals, weights, cloneChildren(n.children), cap)
	}
	if n.isLeaf() {
		return build(insertKeyAt(n.keys, i, key), insertValAt(n.vals, i, val), insertWeightAt(n.weights, i, weight), nil, cap)
	}
	child := nodeInsert(n.children[i], key, val, weight, old, replaced, cap)
	if child.whole != nil {
		children := cloneChildren(n.children)
		children[i] = child.whole
		return insOut{whole: &pnode{keys: cloneKeys(n.keys), vals: cloneVals(n.vals), weights: cloneWeights(n.weights), children: children}}
	}
	keys := insertKeyAt(n.keys, i, child.midK)
	vals := insertValAt(n.vals, i, child.midV)
	weights := insertWeightAt(n.weights, i, child.midW)
	children := cloneChildren(n.children)
	children[i] = child.left
	children = insertChildAt(children, i+1, child.right)
	return build(keys, vals, weights, children, cap)
}

// maxKV is the rightmost (largest) entry of a subtree — its in-order predecessor.
func maxKV(n *pnode) ([]byte, Row, uint32) {
	for !n.isLeaf() {
		n = n.children[len(n.children)-1]
	}
	return n.keys[len(n.keys)-1], n.vals[len(n.vals)-1], n.weights[len(n.weights)-1]
}

// nodeRemove is the recursive delete (copy-on-write). Returns the rebuilt subtree (possibly
// underfull — the caller rebalances it) and the removed row. A separator found in an interior node
// is replaced by its in-order predecessor (drawn from the left subtree), which is then deleted from
// that subtree; the touched child is rebalanced via rebalanceChild.
func nodeRemove(n *pnode, key []byte, cap int) (*pnode, Row, bool) {
	i, found := n.search(key)
	if found {
		if n.isLeaf() {
			vals, removed := removeValAt(n.vals, i)
			return &pnode{keys: removeKeyAt(n.keys, i), vals: vals, weights: removeWeightAt(n.weights, i)}, removed, true
		}
		removed := n.vals[i]
		pk, pv, pw := maxKV(n.children[i])
		newChild, _, _ := nodeRemove(n.children[i], pk, cap)
		keys := cloneKeys(n.keys)
		vals := cloneVals(n.vals)
		weights := cloneWeights(n.weights)
		children := cloneChildren(n.children)
		keys[i], vals[i], weights[i], children[i] = pk, pv, pw, newChild
		rebuilt := &pnode{keys: keys, vals: vals, weights: weights, children: children}
		return rebalanceChild(rebuilt, i, cap), removed, true
	}
	if n.isLeaf() {
		return n, nil, false
	}
	newChild, removed, ok := nodeRemove(n.children[i], key, cap)
	if !ok {
		return n, nil, false
	}
	children := cloneChildren(n.children)
	children[i] = newChild
	rebuilt := &pnode{keys: cloneKeys(n.keys), vals: cloneVals(n.vals), weights: cloneWeights(n.weights), children: children}
	return rebalanceChild(rebuilt, i, cap), removed, true
}

// rebalanceChild: if children[i] is underfull (payload < cap/2), merge it with an adjacent sibling
// (prefer the right one), then split the merged node back if it overflows — the unified rebalance
// (no borrow). The returned parent may itself have lost a key and become underfull; its own parent
// handles that as the recursion unwinds.
func rebalanceChild(n *pnode, i, cap int) *pnode {
	if n.children[i].payload() >= cap/2 {
		return n
	}
	j := i
	if i+1 >= len(n.children) {
		j = i - 1
	}
	return mergeAt(n, j, cap)
}

// mergeAt merges children[j], separator j, and children[j+1] into one node M. If M fits, it replaces
// the pair and the parent loses separator j and child j+1. If M overflows, it is split 2-way and the
// two halves + the new separator replace the pair (the parent's key count is unchanged). M < 2·cap
// always (format.md), so a single split restores fit.
func mergeAt(n *pnode, j, cap int) *pnode {
	left := n.children[j]
	right := n.children[j+1]

	mkeys := make([][]byte, 0, len(left.keys)+1+len(right.keys))
	mkeys = append(mkeys, left.keys...)
	mkeys = append(mkeys, n.keys[j])
	mkeys = append(mkeys, right.keys...)
	mvals := make([]Row, 0, len(left.vals)+1+len(right.vals))
	mvals = append(mvals, left.vals...)
	mvals = append(mvals, n.vals[j])
	mvals = append(mvals, right.vals...)
	mweights := make([]uint32, 0, len(left.weights)+1+len(right.weights))
	mweights = append(mweights, left.weights...)
	mweights = append(mweights, n.weights[j])
	mweights = append(mweights, right.weights...)
	var mchildren []*pnode
	if !left.isLeaf() {
		mchildren = make([]*pnode, 0, len(left.children)+len(right.children))
		mchildren = append(mchildren, left.children...)
		mchildren = append(mchildren, right.children...)
	}

	keys := cloneKeys(n.keys)
	vals := cloneVals(n.vals)
	weights := cloneWeights(n.weights)
	children := cloneChildren(n.children)

	out := build(mkeys, mvals, mweights, mchildren, cap)
	if out.whole != nil {
		keys = removeKeyAt(keys, j)
		vals, _ = removeValAt(vals, j)
		weights = removeWeightAt(weights, j)
		children[j] = out.whole
		children = removeChildAt(children, j+1)
		return &pnode{keys: keys, vals: vals, weights: weights, children: children}
	}
	keys[j], vals[j], weights[j] = out.midK, out.midV, out.midW
	children[j] = out.left
	children[j+1] = out.right
	return &pnode{keys: keys, vals: vals, weights: weights, children: children}
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

func cloneWeights(s []uint32) []uint32 {
	if len(s) == 0 {
		return nil
	}
	out := make([]uint32, len(s))
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

func insertWeightAt(s []uint32, i int, x uint32) []uint32 {
	out := make([]uint32, len(s)+1)
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

func removeWeightAt(s []uint32, i int) []uint32 {
	out := make([]uint32, len(s)-1)
	copy(out, s[:i])
	copy(out[i:], s[i+1:])
	return out
}

func removeChildAt(s []*pnode, i int) []*pnode {
	out := make([]*pnode, len(s)-1)
	copy(out, s[:i])
	copy(out[i:], s[i+1:])
	return out
}
