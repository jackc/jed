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

import (
	"bytes"
	"sort"
)

// pnode is one B-tree node. children is empty for a leaf; otherwise len(children) == len(keys)+1.
// len(keys) == len(vals) == len(weights) always. weights[i] is entry i's on-disk record size, used
// only for the size-driven split/merge. Nodes are never mutated after construction. page is the
// on-disk page index (0 when dirty), set once at the commit that first persists this node.
type pnode struct {
	keys     [][]byte
	vals     []Row
	weights  []uint32
	children []childRef
	page     uint32
}

// childRef is a B-tree node's reference to one child. Under demand paging (P6.4b, spec/design/pager.md
// §4) a clean leaf need not be resident: an interior node keeps an OnDisk page id for such a child and
// the read path faults it through the buffer pool on access. node != nil ⇒ resident (a dirty node, a
// resident interior skeleton node, or a materialized leaf); node == nil ⇒ OnDisk(page) — always a
// leaf, since only leaves page, which is what lets nodeCount avoid loading them. An in-memory database
// constructs only resident refs.
type childRef struct {
	node *pnode
	page uint32
}

func residentRef(n *pnode) childRef  { return childRef{node: n} }
func onDiskRef(page uint32) childRef { return childRef{page: page} }

// leafSource faults a clean leaf page to a resident node on demand (pager.md §4) — the buffer pool,
// behind the table's column types. Defined here so the B-tree traversal can fault without depending on
// the storage/format layers (they implement it); a fully-resident in-memory database passes a nil
// source and never faults.
type leafSource interface {
	loadLeaf(page uint32) (*pnode, error)
}

// resolveChild resolves c to a resident node, faulting an OnDisk leaf through src. A resident ref
// returns its node directly; an OnDisk leaf with no source is an internal wiring bug (an in-memory tree
// builds no OnDisk ref, and every file-backed traversal supplies a source), so it panics.
func resolveChild(c childRef, src leafSource) (*pnode, error) {
	if c.node != nil {
		return c.node, nil
	}
	if src == nil {
		panic("demand-paged leaf reached with no buffer-pool source")
	}
	return src.loadLeaf(c.page)
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

// Get looks up the row at key. src faults an OnDisk leaf on the descent (nil for a fully-resident
// in-memory tree); an I/O error propagates.
func (m *PMap) Get(key []byte, src leafSource) (Row, bool, error) {
	n := m.root
	for n != nil {
		i, found := n.search(key)
		if found {
			return n.vals[i], true, nil
		}
		if n.isLeaf() {
			return nil, false, nil
		}
		child, err := resolveChild(n.children[i], src)
		if err != nil {
			return nil, false, err
		}
		n = child
	}
	return nil, false, nil
}

// Insert inserts or overwrites key with val (on-disk record size weight); cap is the page payload
// capacity. Returns the previous row and true if key was present (an overwrite); otherwise nil and
// false (a new insert, which grows the length). An overwrite can change the weight, so it too may
// overflow and split.
func (m *PMap) Insert(key []byte, val Row, weight uint32, cap int, src leafSource) (Row, bool, error) {
	if m.root == nil {
		m.root = &pnode{keys: [][]byte{key}, vals: []Row{val}, weights: []uint32{weight}}
		m.length++
		return nil, false, nil
	}
	var old Row
	replaced := false
	out, err := nodeInsert(m.root, key, val, weight, &old, &replaced, src, cap)
	if err != nil {
		return nil, false, err
	}
	if out.whole != nil {
		m.root = out.whole
	} else {
		m.root = &pnode{
			keys: [][]byte{out.midK}, vals: []Row{out.midV}, weights: []uint32{out.midW},
			children: []childRef{residentRef(out.left), residentRef(out.right)},
		}
	}
	if !replaced {
		m.length++
	}
	return old, replaced, nil
}

// Remove deletes key. Returns the removed row and true, or (nil,false) if absent (then the map is
// unchanged). cap is the page payload capacity (the rebalance threshold).
func (m *PMap) Remove(key []byte, cap int, src leafSource) (Row, bool, error) {
	if m.root == nil {
		return nil, false, nil
	}
	newRoot, removed, ok, err := nodeRemove(m.root, key, src, cap)
	if err != nil {
		return nil, false, err
	}
	if !ok {
		return nil, false, nil
	}
	// The root may have drained to zero keys: an empty leaf becomes the empty map; an empty internal
	// node (one child) hands the root down a level (height shrinks). The root is exempt from the
	// underfull rule, so no rebalance here.
	if len(newRoot.keys) == 0 {
		if newRoot.isLeaf() {
			m.root = nil
		} else {
			// The lone surviving child becomes the new root — fault it if it is an OnDisk leaf (a
			// tree of height 2 can collapse to its single bottom child).
			child, err := resolveChild(newRoot.children[0], src)
			if err != nil {
				return nil, false, err
			}
			m.root = child
		}
	} else {
		m.root = newRoot
	}
	m.length--
	return removed, true, nil
}

// inorder returns all (key, row) pairs in ascending key order. Eager (the cost contract charges per
// row in the executor loop, not here — spec/design/cost.md), so laziness is unobservable. Faults each
// OnDisk leaf through src; the faulted node is dropped (GC) once its rows are appended, so the resident
// leaf set stays bounded by the pool, not the tree (pager.md §4).
func (m *PMap) inorder(src leafSource) ([][]byte, []Row, error) {
	keys := make([][]byte, 0, m.length)
	vals := make([]Row, 0, m.length)
	var walk func(n *pnode) error
	walk = func(n *pnode) error {
		if n == nil {
			return nil
		}
		if n.isLeaf() {
			keys = append(keys, n.keys...)
			vals = append(vals, n.vals...)
			return nil
		}
		for i := range n.keys {
			child, err := resolveChild(n.children[i], src)
			if err != nil {
				return err
			}
			if err := walk(child); err != nil {
				return err
			}
			keys = append(keys, n.keys[i])
			vals = append(vals, n.vals[i])
		}
		last, err := resolveChild(n.children[len(n.keys)], src)
		if err != nil {
			return err
		}
		return walk(last)
	}
	if err := walk(m.root); err != nil {
		return nil, nil, err
	}
	return keys, vals, nil
}

// nodeCount is the number of B-tree nodes (pages) in this tree — the page_read count a full scan
// charges (spec/design/cost.md §3 "page_read"). A scan walks every node, so this is the structural
// node count (interior + leaf); 0 for an empty map. Deterministic and byte-identical across cores
// (the node boundaries are a §8 byte contract — format.md).
func (m *PMap) nodeCount() int {
	var count func(n *pnode) int
	count = func(n *pnode) int {
		if n == nil {
			return 0
		}
		total := 1
		for _, c := range n.children {
			if c.node != nil {
				total += count(c.node)
			} else {
				// An OnDisk child is a clean leaf (only leaves page — pager.md §1/§4): it
				// contributes one node, counted WITHOUT loading it — the resident-interior-skeleton
				// dividend that keeps cost identical to P6.3 (pager.md §5).
				total++
			}
		}
		return total
	}
	return count(m.root)
}

// residentRecordBytes is the total on-disk record bytes stored in this tree — the sum of every
// entry's weight over every node (this is a B-tree: records live in interior nodes too, not only
// leaves). The deterministic, cross-core-identical measure of a temp table's storage footprint
// (spec/design/temp-tables.md §7; weight is the on-disk record_size, byte-identical across cores —
// §8). The tree is fully resident for a temp store (temp data never pages), so this never faults; an
// OnDisk child would contribute 0 (defensive — temp stores have none).
func (m *PMap) residentRecordBytes() uint64 {
	var walk func(n *pnode) uint64
	walk = func(n *pnode) uint64 {
		if n == nil {
			return 0
		}
		var here uint64
		for _, w := range n.weights {
			here += uint64(w)
		}
		for _, c := range n.children {
			if c.node != nil {
				here += walk(c.node)
			}
		}
		return here
	}
	return walk(m.root)
}

// keyBound is a contiguous range of encoded keys — the form a primary-key predicate pushes down to
// a bounded B-tree scan (spec/design/cost.md §3 "bounded scan / point lookup", encoding.md). lo/hi
// are encoded key bytes; a nil endpoint is open on that side (−∞ / +∞), and the flags say whether
// the endpoint key itself is included. Because the key encoding is order-preserving (bytes.Compare =
// value order), a byte range is a value range. A bounded scan visits exactly the nodes whose key
// span intersects this bound, so its page_read cost is proportional to what it touches, not the
// whole tree (the unbounded bound −∞..+∞ degenerates to the full scan — overlapNodeCount then
// equals nodeCount, so existing full-scan costs do not move).
type keyBound struct {
	lo    []byte
	loInc bool
	hi    []byte
	hiInc bool
}

// unboundedBound is the full-table bound (−∞..+∞): every node overlaps it, so it reproduces the
// full scan exactly.
func unboundedBound() keyBound { return keyBound{} }

// childWindow is the contiguous window [first, last] of n's child indices whose separator span can
// overlap the bound — child i spans the OPEN interval (keys[i-1], keys[i]), so it is pruned iff
// keys[i] ≤ lo (entirely at/below lo) or keys[i-1] ≥ hi (entirely at/above hi). The keys are sorted,
// so the surviving children are contiguous and both edges binary-search: first = the first child not
// below lo, last = the last child not above hi. The strict comparisons are exact regardless of
// endpoint inclusivity — the separators are entries in this node (covered by entryWindow), never in
// a child. The node's own outer brackets need no test: the parent descended here only because this
// subtree overlaps. rangeEntries (which descends) and overlapNodeCount (which counts) window
// identically, so they visit the SAME node set — the §8 cross-core determinism the page_read cost
// depends on — decided from the resident interior separators WITHOUT faulting an OnDisk leaf. A
// bound admitting only a separator entry in this node yields first > last (every child pruned): an
// empty child window, still a valid entry window.
func (b keyBound) childWindow(n *pnode) (int, int) {
	first := 0
	if b.lo != nil {
		first = sort.Search(len(n.keys), func(i int) bool { return bytes.Compare(n.keys[i], b.lo) > 0 })
	}
	last := len(n.keys)
	if b.hi != nil {
		last = sort.Search(len(n.keys), func(i int) bool { return bytes.Compare(n.keys[i], b.hi) >= 0 })
	}
	return first, last
}

// entryWindow is the contiguous half-open window [first, last) of n's own entry indices whose keys
// lie within the bound — the binary-searched equivalent of testing containment per key, honoring the
// endpoint inclusivity flags. On a leaf this is the admitted row range; on an interior node it is
// the admitted separator entries (a B-tree stores records in interior nodes too).
func (b keyBound) entryWindow(n *pnode) (int, int) {
	first := 0
	if b.lo != nil {
		first = sort.Search(len(n.keys), func(i int) bool {
			c := bytes.Compare(n.keys[i], b.lo)
			if b.loInc {
				return c >= 0
			}
			return c > 0
		})
	}
	last := len(n.keys)
	if b.hi != nil {
		last = sort.Search(len(n.keys), func(i int) bool {
			c := bytes.Compare(n.keys[i], b.hi)
			if b.hiInc {
				return c > 0
			}
			return c >= 0
		})
	}
	if last < first {
		last = first
	}
	return first, last
}

// rangeEntries returns the (key, row) pairs whose key lies within the bound, in ascending key
// order — a bounded in-order traversal that binary-searches each node's child window (the children
// whose separator span can overlap the bound — childWindow) and in-bound entry window (entryWindow),
// then walks only those, so only overlapping leaves fault through src. The unbounded bound walks the
// whole tree (identical to inorder). One asymmetric edge: a separator entry equal to an INCLUSIVE lo
// is in bound while both its adjacent children are pruned, so the entry window can start one slot
// before the child window — emitted before the descent loop.
func (m *PMap) rangeEntries(b keyBound, src leafSource) ([][]byte, []Row, error) {
	keys, vals, _, err := m.rangeEntriesCounted(b, src)
	return keys, vals, err
}

// rangeEntriesCounted is rangeEntries plus the number of B-tree nodes the bounded traversal
// visits — the page_read count overlapNodeCount would return, observed during the ONE windowed
// walk instead of a second counting descent (the visited sets are identical by construction:
// both window with childWindow).
func (m *PMap) rangeEntriesCounted(b keyBound, src leafSource) ([][]byte, []Row, int, error) {
	var keys [][]byte
	var vals []Row
	nodes := 0
	var walk func(n *pnode) error
	walk = func(n *pnode) error {
		nodes++
		ef, el := b.entryWindow(n)
		if n.isLeaf() {
			for i := ef; i < el; i++ {
				keys = append(keys, n.keys[i])
				vals = append(vals, n.vals[i])
			}
			return nil
		}
		cf, cl := b.childWindow(n)
		if ef < cf {
			keys = append(keys, n.keys[ef])
			vals = append(vals, n.vals[ef])
		}
		for i := cf; i <= cl; i++ {
			child, err := resolveChild(n.children[i], src)
			if err != nil {
				return err
			}
			if err := walk(child); err != nil {
				return err
			}
			if i >= ef && i < el {
				keys = append(keys, n.keys[i])
				vals = append(vals, n.vals[i])
			}
		}
		return nil
	}
	if m.root != nil {
		if err := walk(m.root); err != nil {
			return nil, nil, 0, err
		}
	}
	return keys, vals, nodes, nil
}

// overlapNodeCount is the number of B-tree nodes a bounded scan over b visits — the page_read it
// charges (spec/design/cost.md §3). It mirrors rangeEntries' traversal exactly (same childWindow
// prune, root always visited), counting an OnDisk leaf as one node WITHOUT faulting it (the
// resident-skeleton dividend, pager.md §5). The unbounded bound returns nodeCount() (every node
// overlaps), so a full scan's cost is unchanged.
func (m *PMap) overlapNodeCount(b keyBound) int {
	var count func(n *pnode) int
	count = func(n *pnode) int {
		if n.isLeaf() {
			return 1
		}
		total := 1
		cf, cl := b.childWindow(n)
		for i := cf; i <= cl; i++ {
			if ch := n.children[i]; ch.node != nil {
				total += count(ch.node)
			} else {
				total++
			}
		}
		return total
	}
	if m.root == nil {
		return 0
	}
	return count(m.root)
}

// scanRange visits the (key, row) pairs within the bound, in ascending key order, calling visit per
// in-bound row. visit returns (continue, error): a false `continue` STOPS the traversal — and because
// the traversal faults a leaf only when it descends into it, leaves past the stop point are never
// faulted (the LIMIT short-circuit is genuine, not a post-hoc truncation — spec/design/cost.md §3
// "LIMIT short-circuit"). Like rangeEntries it prunes non-overlapping subtrees; unlike it, it streams
// (one row at a time, no Vec) so a bounded result holds ~one leaf resident.
func (m *PMap) scanRange(b keyBound, src leafSource, visit func(key []byte, row Row) (bool, error)) error {
	var walk func(n *pnode) (bool, error)
	walk = func(n *pnode) (bool, error) {
		ef, el := b.entryWindow(n)
		if n.isLeaf() {
			for i := ef; i < el; i++ {
				cont, err := visit(n.keys[i], n.vals[i])
				if err != nil || !cont {
					return cont, err
				}
			}
			return true, nil
		}
		cf, cl := b.childWindow(n)
		if ef < cf {
			cont, err := visit(n.keys[ef], n.vals[ef])
			if err != nil || !cont {
				return cont, err
			}
		}
		for i := cf; i <= cl; i++ {
			child, err := resolveChild(n.children[i], src)
			if err != nil {
				return false, err
			}
			if cont, err := walk(child); err != nil || !cont {
				return cont, err
			}
			if i >= ef && i < el {
				cont, err := visit(n.keys[i], n.vals[i])
				if err != nil || !cont {
					return cont, err
				}
			}
		}
		return true, nil
	}
	if m.root == nil {
		return nil
	}
	_, err := walk(m.root)
	return err
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
// one median (format.md "Split point"). rightEdge says the just-edited record (the inserted/replaced
// one, or the separator a child split promoted) is the node's LAST: then the split is the append rule
// m = min(m_append, N-2) with m_append = largest m in [1,N-1] with leftpayload(m) ≤ cap — sequential
// ascending loads pack left nodes ~full. Anywhere else (and the delete path's merge-overflow, which
// has no edited position) splits BALANCED: m = min(m_balanced, m_append, N-2) with m_balanced =
// smallest m with 2·leftpayload(m) ≥ payload — without it, largest-left degenerates to [N-2 | 1]
// splinters and random-order inserts converge on a few-percent fill (benchmarks.md finding). Either
// m yields two non-empty, fitting halves under the RECORD_MAX = (cap-12)/2 cap (format.md). The < 3
// guard is defensive against an oversized record — it leaves the node whole, and the oversize is
// surfaced as 0A000 when the node is serialized (format.go).
func build(keys [][]byte, vals []Row, weights []uint32, children []childRef, cap int, rightEdge bool) insOut {
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
	balanced := 0
	for m := 1; m < n; m++ {
		prefix += int(weights[m-1])
		lp := prefix
		if interior {
			lp += 4 * (m + 1)
		}
		if lp <= cap {
			best = m
		}
		if balanced == 0 && 2*lp >= payload {
			balanced = m
		}
	}
	m := best
	if !rightEdge && balanced != 0 && balanced < m {
		m = balanced
	}
	if n-2 < m {
		m = n - 2
	}
	if m < 1 {
		m = 1
	}

	var lchildren, rchildren []childRef
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
func nodeInsert(n *pnode, key []byte, val Row, weight uint32, old *Row, replaced *bool, src leafSource, cap int) (insOut, error) {
	i, found := n.search(key)
	if found {
		vals := cloneVals(n.vals)
		weights := cloneWeights(n.weights)
		*old = vals[i]
		*replaced = true
		vals[i] = val
		weights[i] = weight
		return build(cloneKeys(n.keys), vals, weights, cloneChildren(n.children), cap, i == len(n.keys)-1), nil
	}
	if n.isLeaf() {
		return build(insertKeyAt(n.keys, i, key), insertValAt(n.vals, i, val), insertWeightAt(n.weights, i, weight), nil, cap, i == len(n.keys)), nil
	}
	// Fault the target child (a resident interior, or an OnDisk leaf brought in for mutation — it
	// becomes a dirty resident node on the rebuilt path).
	childNode, err := resolveChild(n.children[i], src)
	if err != nil {
		return insOut{}, err
	}
	sub, err := nodeInsert(childNode, key, val, weight, old, replaced, src, cap)
	if err != nil {
		return insOut{}, err
	}
	if sub.whole != nil {
		children := cloneChildren(n.children)
		children[i] = residentRef(sub.whole)
		return insOut{whole: &pnode{keys: cloneKeys(n.keys), vals: cloneVals(n.vals), weights: cloneWeights(n.weights), children: children}}, nil
	}
	keys := insertKeyAt(n.keys, i, sub.midK)
	vals := insertValAt(n.vals, i, sub.midV)
	weights := insertWeightAt(n.weights, i, sub.midW)
	children := cloneChildren(n.children)
	children[i] = residentRef(sub.left)
	children = insertChildAt(children, i+1, residentRef(sub.right))
	return build(keys, vals, weights, children, cap, i == len(n.keys)), nil
}

// maxKV is the rightmost (largest) entry of a subtree — its in-order predecessor. Faults the rightmost
// leaf through src if it is OnDisk.
func maxKV(n *pnode, src leafSource) ([]byte, Row, uint32, error) {
	for !n.isLeaf() {
		child, err := resolveChild(n.children[len(n.children)-1], src)
		if err != nil {
			return nil, nil, 0, err
		}
		n = child
	}
	return n.keys[len(n.keys)-1], n.vals[len(n.vals)-1], n.weights[len(n.weights)-1], nil
}

// nodeRemove is the recursive delete (copy-on-write). Returns the rebuilt subtree (possibly
// underfull — the caller rebalances it) and the removed row. A separator found in an interior node
// is replaced by its in-order predecessor (drawn from the left subtree), which is then deleted from
// that subtree; the touched child is rebalanced via rebalanceChild.
func nodeRemove(n *pnode, key []byte, src leafSource, cap int) (*pnode, Row, bool, error) {
	i, found := n.search(key)
	if found {
		if n.isLeaf() {
			vals, removed := removeValAt(n.vals, i)
			return &pnode{keys: removeKeyAt(n.keys, i), vals: vals, weights: removeWeightAt(n.weights, i)}, removed, true, nil
		}
		removed := n.vals[i]
		// Fault the left subtree once; both the predecessor lookup and its deletion descend it.
		leftChild, err := resolveChild(n.children[i], src)
		if err != nil {
			return nil, nil, false, err
		}
		pk, pv, pw, err := maxKV(leftChild, src)
		if err != nil {
			return nil, nil, false, err
		}
		newChild, _, _, err := nodeRemove(leftChild, pk, src, cap)
		if err != nil {
			return nil, nil, false, err
		}
		keys := cloneKeys(n.keys)
		vals := cloneVals(n.vals)
		weights := cloneWeights(n.weights)
		children := cloneChildren(n.children)
		keys[i], vals[i], weights[i], children[i] = pk, pv, pw, residentRef(newChild)
		rebuilt := &pnode{keys: keys, vals: vals, weights: weights, children: children}
		out, err := rebalanceChild(rebuilt, i, src, cap)
		if err != nil {
			return nil, nil, false, err
		}
		return out, removed, true, nil
	}
	if n.isLeaf() {
		return n, nil, false, nil
	}
	childNode, err := resolveChild(n.children[i], src)
	if err != nil {
		return nil, nil, false, err
	}
	newChild, removed, ok, err := nodeRemove(childNode, key, src, cap)
	if err != nil {
		return nil, nil, false, err
	}
	if !ok {
		return n, nil, false, nil
	}
	children := cloneChildren(n.children)
	children[i] = residentRef(newChild)
	rebuilt := &pnode{keys: cloneKeys(n.keys), vals: cloneVals(n.vals), weights: cloneWeights(n.weights), children: children}
	out, err := rebalanceChild(rebuilt, i, src, cap)
	if err != nil {
		return nil, nil, false, err
	}
	return out, removed, true, nil
}

// rebalanceChild: if children[i] is underfull (payload < cap/2), merge it with an adjacent sibling
// (prefer the right one), then split the merged node back if it overflows — the unified rebalance
// (no borrow). The returned parent may itself have lost a key and become underfull; its own parent
// handles that as the recursion unwinds.
func rebalanceChild(n *pnode, i int, src leafSource, cap int) (*pnode, error) {
	// children[i] was just rebuilt resident by nodeRemove, so inspecting it faults nothing.
	childNode, err := resolveChild(n.children[i], src)
	if err != nil {
		return nil, err
	}
	if childNode.payload() >= cap/2 {
		return n, nil
	}
	j := i
	if i+1 >= len(n.children) {
		j = i - 1
	}
	return mergeAt(n, j, src, cap)
}

// mergeAt merges children[j], separator j, and children[j+1] into one node M. If M fits, it replaces
// the pair and the parent loses separator j and child j+1. If M overflows, it is split 2-way and the
// two halves + the new separator replace the pair (the parent's key count is unchanged). M < 2·cap
// always (format.md), so a single split restores fit.
func mergeAt(n *pnode, j int, src leafSource, cap int) (*pnode, error) {
	// Fault both children — the underfull child (just rebuilt resident) and its sibling, which may
	// still be an OnDisk leaf the delete never touched.
	left, err := resolveChild(n.children[j], src)
	if err != nil {
		return nil, err
	}
	right, err := resolveChild(n.children[j+1], src)
	if err != nil {
		return nil, err
	}

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
	var mchildren []childRef
	if !left.isLeaf() {
		mchildren = make([]childRef, 0, len(left.children)+len(right.children))
		mchildren = append(mchildren, left.children...)
		mchildren = append(mchildren, right.children...)
	}

	keys := cloneKeys(n.keys)
	vals := cloneVals(n.vals)
	weights := cloneWeights(n.weights)
	children := cloneChildren(n.children)

	out := build(mkeys, mvals, mweights, mchildren, cap, false) // merge-overflow: balanced (format.md)
	if out.whole != nil {
		keys = removeKeyAt(keys, j)
		vals, _ = removeValAt(vals, j)
		weights = removeWeightAt(weights, j)
		children[j] = residentRef(out.whole)
		children = removeChildAt(children, j+1)
		return &pnode{keys: keys, vals: vals, weights: weights, children: children}, nil
	}
	keys[j], vals[j], weights[j] = out.midK, out.midV, out.midW
	children[j] = residentRef(out.left)
	children[j+1] = residentRef(out.right)
	return &pnode{keys: keys, vals: vals, weights: weights, children: children}, nil
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

func cloneChildren(s []childRef) []childRef {
	if len(s) == 0 {
		return nil
	}
	out := make([]childRef, len(s))
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

func insertChildAt(s []childRef, i int, x childRef) []childRef {
	out := make([]childRef, len(s)+1)
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

func removeChildAt(s []childRef, i int) []childRef {
	out := make([]childRef, len(s)-1)
	copy(out, s[:i])
	copy(out[i:], s[i+1:])
	return out
}
