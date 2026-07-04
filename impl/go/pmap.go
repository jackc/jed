package jed

// Persistent (copy-on-write) ordered map — the page-backed B+tree (decision B1,
// spec/design/bplus-reshape.md; spec/design/transactions.md §3; spec/fileformat/format.md "The
// per-table data B+tree").
//
// Keyed by the encoded key bytes (compared with bytes.Compare = memcmp = the order-preserving
// key encoding's contract, spec/design/encoding.md). Every mutation returns a new map that shares
// structure with the old one; nodes are immutable by convention (never mutated after construction),
// so copying a PMap value (which shares the root pointer) is an O(1) independent snapshot. That
// cheap, structurally-shared snapshot carries the §3 staging-buffer / transaction model.
//
// This IS the on-disk B+tree, node-for-page (v24). Records live ONLY in leaves; an interior node is
// a record-free routing skeleton — separator keys + child pointers. A separator is a COPY of a
// boundary key (a leaf split copies the right half's first key up; an interior split pushes its
// median separator up) and may go stale after deletes — it keeps routing (left < sep ≤ right holds
// forever). Fan-out is size-driven: a node holds as many entries as fit a page payload cap
// (= page_size − 16) and splits when it would overflow, so the node boundaries (and serialized
// bytes) are a §8 byte contract (format.md). The caller supplies each leaf entry's on-disk weight
// (record size) so this map can sum leaf payloads without knowing the value codec; interior
// payloads come from the separators themselves. cap and the leaf's column-class shape are passed
// per call (properties of the database's page size and the table's column types, held by the
// TableStore). Each node also carries a set-once on-disk page id (0 = dirty) for the incremental
// commit (P6.1 part B). Delete rebalances by merge-then-maybe-split (no borrow — merge subsumes it;
// an interior merge whose result cannot 2-way split is ABANDONED, format.md "Delete").

import (
	"bytes"
	"sort"
)

// pnode is one B+tree node. A LEAF has no children and len(keys) == len(vals) == len(weights)
// (or a packed block in place of vals). An INTERIOR node has len(children) == len(keys)+1 and
// EMPTY vals/weights — its keys are the routing separators, its payload is derived from the
// separator bytes themselves (v24, record-free). Nodes are never mutated after construction.
// page is the on-disk page index (0 when dirty), set once at the commit that first persists it.
type pnode struct {
	keys [][]byte
	// The decoded value rows — populated for a Decoded LEAF (a writer's transient
	// materialize-mutate-repack buffer; the post-commit residency flip demotes it once persisted,
	// so Decoded survives a commit only in a root leaf, a GiST leaf-key store, or a bare scratch
	// engine), nil for a Packed leaf (which reconstructs on demand from packed) and for every
	// INTERIOR node (record-free, v24).
	// Read only through the rowAt / colAt / decodedRows seam, never indexed directly, so the two
	// leaf forms are interchangeable (packed-leaf.md §3/§4).
	vals     []storedRow
	weights  []uint32
	children []childRef
	// packed is the block-backed resident form of a demand-paged clean leaf (packed-leaf.md §5): the
	// page block + PAX directories, from which vals are reconstructed on demand. nil for a Decoded
	// node (interior nodes, in-memory/loaded leaves, and any dirty leaf — mutation materializes
	// Packed→Decoded first, §7). A Packed leaf is always clean (page != 0), so it is never serialized.
	packed *packedLeaf
	page   uint32
}

// rowAt reconstructs value row i as a storedRow — the value-read seam (packed-leaf.md §4), on a
// LEAF. A Decoded leaf returns the stored row (shared, read-only by convention); a Packed leaf
// reconstructs it from the retained PAX directories on demand. Errors on a corrupt touched inline
// body (XX001); the Decoded path never errors.
func (n *pnode) rowAt(i int) (storedRow, error) {
	if n.packed == nil {
		return n.vals[i], nil
	}
	return n.packed.row(i)
}

// colAt reconstructs ONLY column c of row i — the touched-column path (packed-leaf.md §4/§6, the
// OP_Column model PAX's column regions make O(1)).
func (n *pnode) colAt(i, c int) (Value, error) {
	if n.packed == nil {
		return n.vals[i][c], nil
	}
	return n.packed.value(c, i)
}

// decodedRows returns every value row of a LEAF — the mutation-descent materialization
// (packed-leaf.md §7). A Decoded leaf clones vals; a Packed leaf reconstructs every row so the
// rebuilt node is Decoded (buildLeaf / nodeInsert / nodeRemove / mergeAt then run unchanged).
func (n *pnode) decodedRows() ([]storedRow, error) {
	if n.packed == nil {
		return cloneVals(n.vals), nil
	}
	rows := make([]storedRow, n.packed.n)
	for i := range rows {
		r, err := n.packed.row(i)
		if err != nil {
			return nil, err
		}
		rows[i] = r
	}
	return rows, nil
}

// childRef is a B+tree node's reference to one child. Under demand paging (P6.4b, spec/design/pager.md
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
// behind the table's column types. Defined here so the B+tree traversal can fault without depending on
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

// payload is this node's serialized size (format.md): a leaf is Σ weights +
// leafOverhead(N, shape); an interior node is 8·N + 4 + Σ sep_len (child pointers + separator
// directory + key blob — record-free, v24).
func (n *pnode) payload(shape leafShape) int {
	if n.isLeaf() {
		total := 0
		for _, w := range n.weights {
			total += int(w)
		}
		return total + leafOverhead(len(n.keys), shape)
	}
	total := 8*len(n.keys) + 4
	for _, k := range n.keys {
		total += len(k)
	}
	return total
}

// search binary-searches a LEAF's keys, returning (index, found): found ⇒ key is at keys[index];
// else index is the insertion slot.
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

// childSlot is the child an INTERIOR descent takes for key: partition_point(sep ≤ key) — a key
// equal to a separator lies in the RIGHT subtree (the copy-up separator is the right half's first
// key; format.md "Interior node").
func (n *pnode) childSlot(key []byte) int {
	return sort.Search(len(n.keys), func(i int) bool { return bytes.Compare(n.keys[i], key) > 0 })
}

// PMap is a persistent ordered map from encoded key to Row. A value copy is an O(1) independent
// snapshot (the root pointer is shared; nodes are immutable).
//
// length is the exact row count when known. A map built from empty by Insert/Remove maintains it
// for free; a map loaded from a disk skeleton (fromSkeleton) leaves lengthUnknown set — open reads
// only the interior spine and never walks the leaves to sum it (spec/design/storage.md §6), and
// nothing needs the exact count of a loaded table. lengthUnknown is deliberately the NON-zero-value
// flag: a fresh (zero-value) pMap and newPMap are known-0, matching newTableStore's zero-value rows.
type pMap struct {
	root          *pnode
	length        int
	lengthUnknown bool
}

// NewPMap returns an empty map (known count 0).
func newPMap() pMap { return pMap{} }

// Count returns the exact row count and whether it is known. A disk-loaded skeleton returns
// (0, false); nothing in the engine needs a loaded table's exact count (IsEmpty uses the root).
func (m *pMap) Count() (int, bool) { return m.length, !m.lengthUnknown }

// IsEmpty reports whether the map has no rows — derived from the root (exact, O(1)), independent of
// whether the count is known.
func (m *pMap) IsEmpty() bool { return m.root == nil }

// root exposes the root node to the serializer (format.go). nil for an empty map.
func (m *pMap) rootNode() *pnode { return m.root }

// fromSkeleton reconstructs a map from a disk-loaded skeleton root (format.go loadEnginePaged). The
// count is unknown — open reads only the interior spine (spec/design/storage.md §6).
func fromSkeleton(root *pnode) pMap { return pMap{root: root, lengthUnknown: true} }

// Get looks up the row at key — a root→leaf descent (interior nodes only route, v24). src faults an
// OnDisk leaf on the descent (nil for a fully-resident in-memory tree); an I/O error propagates.
func (m *pMap) Get(key []byte, src leafSource) (storedRow, bool, error) {
	n := m.root
	if n == nil {
		return nil, false, nil
	}
	for !n.isLeaf() {
		child, err := resolveChild(n.children[n.childSlot(key)], src)
		if err != nil {
			return nil, false, err
		}
		n = child
	}
	i, found := n.search(key)
	if !found {
		return nil, false, nil
	}
	row, err := n.rowAt(i)
	if err != nil {
		return nil, false, err
	}
	return row, true, nil
}

// Insert inserts or overwrites key with val (on-disk record size weight); cap is the page payload
// capacity and shape the leaf's column-class shape. Returns the previous row and true if key was
// present (an overwrite); otherwise nil and false (a new insert, which grows the length). An
// overwrite can change the weight, so it too may overflow and split.
func (m *pMap) Insert(key []byte, val storedRow, weight uint32, cap int, shape leafShape, src leafSource) (storedRow, bool, error) {
	if m.root == nil {
		m.root = &pnode{keys: [][]byte{key}, vals: []storedRow{val}, weights: []uint32{weight}}
		if !m.lengthUnknown {
			m.length++
		}
		return nil, false, nil
	}
	var old storedRow
	replaced := false
	out, err := nodeInsert(m.root, key, val, weight, &old, &replaced, src, cap, shape)
	if err != nil {
		return nil, false, err
	}
	if out.whole != nil {
		m.root = out.whole
	} else {
		m.root = &pnode{
			keys:     [][]byte{out.sep},
			children: []childRef{residentRef(out.left), residentRef(out.right)},
		}
	}
	if !replaced && !m.lengthUnknown {
		m.length++
	}
	return old, replaced, nil
}

// Remove deletes key. Returns the removed row and true, or (nil,false) if absent (then the map is
// unchanged). cap is the page payload capacity (the rebalance threshold).
func (m *pMap) Remove(key []byte, cap int, shape leafShape, src leafSource) (storedRow, bool, error) {
	if m.root == nil {
		return nil, false, nil
	}
	newRoot, removed, ok, err := nodeRemove(m.root, key, src, cap, shape)
	if err != nil {
		return nil, false, err
	}
	if !ok {
		return nil, false, nil
	}
	// The root may have drained to zero keys: an empty leaf becomes the empty map; a 0-key interior
	// root (one child) hands the root down a level (height shrinks). The root is exempt from the
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
	if !m.lengthUnknown {
		m.length--
	}
	return removed, true, nil
}

// demoteCleanLeaves demotes every clean, PERSISTED resident leaf to its OnDisk(page) child
// reference — the post-commit residency flip (bplus-reshape.md B4): after a commit assigns page
// ids to the dirty nodes it wrote, the committed tree sheds its leaf payloads and becomes the
// skeletal `interior nodes + OnDisk leaves` shape every load already produces, so reads everywhere
// go through the one Packed pool path and Decoded survives only inside an uncommitted writer. A
// ROOT leaf stays resident (the pMap root is always a node — the open/load convention); an
// unpersisted (page 0) leaf is left alone (defensive — a bare scratch engine that never persists).
// Rebuilds only the interior spine above changed children; an unchanged subtree keeps its node
// pointer (and its set-once page id), so the flip is O(interior nodes) and the flipped tree stays
// clean for the next incremental commit.
func (m *pMap) demoteCleanLeaves() {
	var demote func(n *pnode) *pnode
	demote = func(n *pnode) *pnode {
		if n.isLeaf() {
			return nil // handled by the parent (a root leaf stays resident)
		}
		changed := false
		children := make([]childRef, 0, len(n.children))
		for _, c := range n.children {
			nc := c
			if c.node != nil {
				if c.node.isLeaf() {
					if c.node.page != 0 {
						changed = true
						nc = onDiskRef(c.node.page)
					}
				} else if rebuilt := demote(c.node); rebuilt != nil {
					changed = true
					nc = residentRef(rebuilt)
				}
			}
			children = append(children, nc)
		}
		if !changed {
			return nil
		}
		// The rebuilt interior keeps its keys AND its page id — its serialized bytes are unchanged
		// (children reference the same pages), so it must stay clean or the next incremental commit
		// would rewrite the whole spine every time.
		return &pnode{keys: n.keys, children: children, page: n.page}
	}
	if m.root != nil {
		if rebuilt := demote(m.root); rebuilt != nil {
			m.root = rebuilt
		}
	}
}

// inorder returns all (key, row) pairs in ascending key order — a leaf walk in key order (records
// are leaf-only, v24). Eager (the cost contract charges per row in the executor loop, not here —
// spec/design/cost.md), so laziness is unobservable. Faults each OnDisk leaf through src; the faulted
// node is dropped (GC) once its rows are appended, so the resident leaf set stays bounded by the
// pool, not the tree (pager.md §4).
func (m *pMap) inorder(src leafSource) ([][]byte, []storedRow, error) {
	keys := make([][]byte, 0, m.length)
	vals := make([]storedRow, 0, m.length)
	var walk func(n *pnode) error
	walk = func(n *pnode) error {
		if n == nil {
			return nil
		}
		if n.isLeaf() {
			for i := range n.keys {
				row, err := n.rowAt(i)
				if err != nil {
					return err
				}
				keys = append(keys, n.keys[i])
				vals = append(vals, row)
			}
			return nil
		}
		for i := range n.children {
			child, err := resolveChild(n.children[i], src)
			if err != nil {
				return err
			}
			if err := walk(child); err != nil {
				return err
			}
		}
		return nil
	}
	if err := walk(m.root); err != nil {
		return nil, nil, err
	}
	return keys, vals, nil
}

// nodeCount is the number of B+tree nodes (pages) in this tree — the page_read count a full scan
// charges (spec/design/cost.md §3 "page_read"). A scan walks every node, so this is the structural
// node count (interior + leaf); 0 for an empty map. Deterministic and byte-identical across cores
// (the node boundaries are a §8 byte contract — format.md).
func (m *pMap) nodeCount() int {
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
// leaf entry's weight (records live only in leaves, v24; interior weights are empty). The
// deterministic, cross-core-identical measure of a temp table's storage footprint
// (spec/design/temp-tables.md §7; weight is the on-disk record_size, byte-identical across cores —
// §8). The tree is fully resident for a temp store (temp data never pages), so this never faults; an
// OnDisk child would contribute 0 (defensive — temp stores have none).
func (m *pMap) residentRecordBytes() uint64 {
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
// a bounded B+tree scan (spec/design/cost.md §3 "bounded scan / point lookup", encoding.md). lo/hi
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

// childWindow is the contiguous window [first, last] of n's child indices whose key span can
// overlap the bound. Child i spans [keys[i-1], keys[i]) (v24 — a key equal to a separator lies
// right), so child i is pruned iff keys[i] ≤ lo (entirely at/below lo) or keys[i-1] is at/above
// hi — > hi for an INCLUSIVE hi (a child whose low separator equals hi can still hold hi itself),
// ≥ hi for an exclusive one. The separators are sorted, so the surviving children are contiguous
// and both edges binary-search. rangeEntries (which descends) and overlapNodeCount (which counts)
// window identically, so they visit the SAME node set — the §8 cross-core determinism the
// page_read cost depends on — decided from the resident interior separators WITHOUT faulting an
// OnDisk leaf.
func (b keyBound) childWindow(n *pnode) (int, int) {
	first := 0
	if b.lo != nil {
		first = sort.Search(len(n.keys), func(i int) bool { return bytes.Compare(n.keys[i], b.lo) > 0 })
	}
	last := len(n.keys)
	if b.hi != nil {
		if b.hiInc {
			last = sort.Search(len(n.keys), func(i int) bool { return bytes.Compare(n.keys[i], b.hi) > 0 })
		} else {
			last = sort.Search(len(n.keys), func(i int) bool { return bytes.Compare(n.keys[i], b.hi) >= 0 })
		}
	}
	if last < first {
		last = first
	}
	return first, last
}

// entryWindow is the contiguous half-open window [first, last) of a LEAF's record indices whose
// keys lie within the bound — the binary-searched equivalent of testing containment per key,
// honoring the endpoint inclusivity flags. Applies only at leaves (v24 — interior nodes hold no
// records).
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
// order — a bounded in-order traversal that binary-searches each interior node's child window (the
// children whose separator span can overlap the bound — childWindow) and each leaf's in-bound entry
// window (entryWindow), then walks only those, so only overlapping leaves fault through src. The
// unbounded bound walks the whole tree (identical to inorder).
func (m *pMap) rangeEntries(b keyBound, src leafSource) ([][]byte, []storedRow, error) {
	keys, vals, _, err := m.rangeEntriesCounted(b, src)
	return keys, vals, err
}

// rangeEntriesCounted is rangeEntries plus the number of B+tree nodes the bounded traversal
// visits — the page_read count overlapNodeCount would return, observed during the ONE windowed
// walk instead of a second counting descent (the visited sets are identical by construction:
// both window with childWindow). The old two-form masked/unmasked reconstruction seam is
// collapsed (bplus-reshape.md B4): a Packed leaf's reconstruction is uniformly lazy, so a
// reconstruction mask no longer exists — the query's touched set survives as the cost basis + the
// scan layer's resolve prefetch, and a missed value resolves on touch (the demand-fault backstop).
func (m *pMap) rangeEntriesCounted(b keyBound, src leafSource) ([][]byte, []storedRow, int, error) {
	var keys [][]byte
	var vals []storedRow
	nodes := 0
	var walk func(n *pnode) error
	walk = func(n *pnode) error {
		nodes++
		if n.isLeaf() {
			ef, el := b.entryWindow(n)
			for i := ef; i < el; i++ {
				row, err := n.rowAt(i)
				if err != nil {
					return err
				}
				keys = append(keys, n.keys[i])
				vals = append(vals, row)
			}
			return nil
		}
		cf, cl := b.childWindow(n)
		for i := cf; i <= cl; i++ {
			child, err := resolveChild(n.children[i], src)
			if err != nil {
				return err
			}
			if err := walk(child); err != nil {
				return err
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

// columnarScan walks the bounded scan gathering ONLY the columns mask selects into dense per-column
// lanes (cols[c] of length rowCount for each selected c; nil otherwise), never building a full-width
// storedRow — the A2 columnar-gather feed (packed-leaf.md §11 Track A2, the allocation dividend A1
// leaves on the table). It mirrors rangeEntriesCounted's traversal EXACTLY (same node visits ⇒ the
// same page_read count; same in-order record sequence — leaf-only, v24), but reads each admitted
// row's selected columns via colAt — an O(1) PAX column span on a Packed leaf, vals[i][c] on a
// Decoded leaf — so a wide-table single-column scan never materializes the untouched columns NOR a
// full-width row. Each cols[c] is in scan order, so it equals the column-c stride of
// rangeEntriesCounted's rows. rowCount is the admitted entry count.
func (m *pMap) columnarScan(b keyBound, src leafSource, mask []bool) ([][]Value, int, int, error) {
	k := len(mask)
	cols := make([][]Value, k)
	rowCount, nodes := 0, 0
	var walk func(n *pnode) error
	walk = func(n *pnode) error {
		nodes++
		if n.isLeaf() {
			ef, el := b.entryWindow(n)
			for i := ef; i < el; i++ {
				for c := 0; c < k; c++ {
					if !mask[c] {
						continue
					}
					v, err := n.colAt(i, c)
					if err != nil {
						return err
					}
					cols[c] = append(cols[c], v)
				}
				rowCount++
			}
			return nil
		}
		cf, cl := b.childWindow(n)
		for i := cf; i <= cl; i++ {
			child, err := resolveChild(n.children[i], src)
			if err != nil {
				return err
			}
			if err := walk(child); err != nil {
				return err
			}
		}
		return nil
	}
	if m.root != nil {
		if err := walk(m.root); err != nil {
			return nil, 0, 0, err
		}
	}
	return cols, rowCount, nodes, nil
}

// foldScan walks the bounded scan calling visit(n, i) for each admitted leaf record i (in scan / key
// order) of the leaf n it belongs to, faulting leaves via src — the fold-during-walk twin of
// columnarScan (packed-leaf.md §11): the aggregate folds each row's touched columns (read on demand
// via n.colAt) straight into its accumulator, so a whole-table/grouped aggregate never materializes a
// per-column lane (O(1) memory instead of O(rows)). It visits the IDENTICAL nodes columnarScan does
// and returns the same (rowCount, nodeCount), so the caller charges the identical page_read /
// storage_row_read. visit's error aborts the walk.
func (m *pMap) foldScan(b keyBound, src leafSource, visit func(n *pnode, i int) error) (rowCount, nodes int, err error) {
	var walk func(n *pnode) error
	walk = func(n *pnode) error {
		nodes++
		if n.isLeaf() {
			ef, el := b.entryWindow(n)
			for i := ef; i < el; i++ {
				rowCount++
				if e := visit(n, i); e != nil {
					return e
				}
			}
			return nil
		}
		cf, cl := b.childWindow(n)
		for i := cf; i <= cl; i++ {
			child, e := resolveChild(n.children[i], src)
			if e != nil {
				return e
			}
			if e := walk(child); e != nil {
				return e
			}
		}
		return nil
	}
	if m.root != nil {
		if e := walk(m.root); e != nil {
			return 0, 0, e
		}
	}
	return rowCount, nodes, nil
}

// overlapNodeCount is the number of B+tree nodes a bounded scan over b visits — the page_read it
// charges (spec/design/cost.md §3). It mirrors rangeEntries' traversal exactly (same childWindow
// prune, root always visited), counting an OnDisk leaf as one node WITHOUT faulting it (the
// resident-skeleton dividend, pager.md §5). The unbounded bound returns nodeCount() (every node
// overlaps), so a full scan's cost is unchanged.
func (m *pMap) overlapNodeCount(b keyBound) int {
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
func (m *pMap) scanRange(b keyBound, src leafSource, visit func(key []byte, row storedRow) (bool, error)) error {
	var walk func(n *pnode) (bool, error)
	walk = func(n *pnode) (bool, error) {
		if n.isLeaf() {
			ef, el := b.entryWindow(n)
			for i := ef; i < el; i++ {
				row, err := n.rowAt(i)
				if err != nil {
					return false, err
				}
				cont, err := visit(n.keys[i], row)
				if err != nil || !cont {
					return cont, err
				}
			}
			return true, nil
		}
		cf, cl := b.childWindow(n)
		for i := cf; i <= cl; i++ {
			child, err := resolveChild(n.children[i], src)
			if err != nil {
				return false, err
			}
			if cont, err := walk(child); err != nil || !cont {
				return cont, err
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

// scanRangeRev is scanRange in reverse: it visits the in-bound (key, row) pairs in DESCENDING key
// order — the exact reverse of scanRange's row sequence — for a DESC reverse scan (spec/design/
// cost.md §3 "ORDER BY satisfied by primary-key order"). It windows with the same childWindow/
// entryWindow prune (so the visited-node set and page_read cost match), and stops the moment visit
// returns a false `continue` without faulting leaves past the stop point (a reverse top-N faults
// from the high end). An interior node walks its windowed children from cl down to cf.
func (m *pMap) scanRangeRev(b keyBound, src leafSource, visit func(key []byte, row storedRow) (bool, error)) error {
	var walk func(n *pnode) (bool, error)
	walk = func(n *pnode) (bool, error) {
		if n.isLeaf() {
			ef, el := b.entryWindow(n)
			for i := el - 1; i >= ef; i-- {
				row, err := n.rowAt(i)
				if err != nil {
					return false, err
				}
				cont, err := visit(n.keys[i], row)
				if err != nil || !cont {
					return cont, err
				}
			}
			return true, nil
		}
		cf, cl := b.childWindow(n)
		for i := cl; i >= cf; i-- {
			child, err := resolveChild(n.children[i], src)
			if err != nil {
				return false, err
			}
			if cont, err := walk(child); err != nil || !cont {
				return cont, err
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

// scanFrame is one node on a rangeCursor's explicit traversal stack: the node and the half-open span
// [lo, hi) of positions still to process. A LEAF's positions are its in-bound record indices (its
// entry window). An INTERIOR node's positions are its overlapping child indices (its child window) —
// interior nodes emit nothing (records are leaf-only, v24), so the frame only descends. Reversal
// consumes [lo, hi) from the back, with no separate forward/reverse logic.
type scanFrame struct {
	node   *pnode
	isLeaf bool
	lo, hi int
}

func newScanFrame(n *pnode, b keyBound) scanFrame {
	if n.isLeaf() {
		ef, el := b.entryWindow(n)
		return scanFrame{node: n, isLeaf: true, lo: ef, hi: el}
	}
	cf, cl := b.childWindow(n)
	return scanFrame{node: n, isLeaf: false, lo: cf, hi: cl + 1}
}

// rangeCursor is a PULL (stateful) cursor over a pMap's (key, row) pairs within a keyBound, in
// ascending (reverse=false) or descending (reverse=true) key order — the pull-model equivalent of
// scanRange / scanRangeRev (the S2 pull B+tree scan cursor, spec/design/streaming.md §3/§5). Where
// scanRange PUSHES each row to a visit callback and owns the control flow, this cursor lets the
// CALLER own it: each next() yields the next in-bound pair, advancing an explicit frame stack over
// the persistent map. That is the VDBE-forward shape (streaming.md §3): a stateful next/rewind cursor
// a future bytecode VM can drive, where a push callback cannot.
//
// It yields the EXACT same sequence as scanRange (reverse=false) / scanRangeRev (reverse=true) — same
// rows, same order, faulting a clean leaf through src only when the traversal descends into it, so a
// caller that stops pulling early faults no leaves past where it stopped (the genuine LIMIT short-
// circuit, cost.md §3). The yielded row is the stored slice (a reference, like scanRange's callback
// and rangeEntries' appended rows); the Go GC keeps a faulted leaf's backing array alive as long as a
// pulled row references it, even after the buffer pool evicts the leaf (pager.md §4).
type rangeCursor struct {
	stack   []scanFrame
	bound   keyBound
	src     leafSource
	reverse bool
}

// rangeCursor returns a pull cursor over the (key, row) pairs within b. The first node on the stack is
// the root (always resident); descendants fault through src on descent. See rangeCursor (the type).
func (m *pMap) rangeCursor(b keyBound, src leafSource, reverse bool) *rangeCursor {
	c := &rangeCursor{bound: b, src: src, reverse: reverse}
	if m.root != nil {
		c.stack = append(c.stack, newScanFrame(m.root, b))
	}
	return c
}

// next yields the next in-bound (key, row), or ok=false when the traversal is exhausted. Each call
// advances the frame stack until it emits a leaf row, descends into (and faults) a child, or pops an
// exhausted frame.
func (c *rangeCursor) next() (key []byte, row storedRow, ok bool, err error) {
	for len(c.stack) > 0 {
		// &c.stack[top] is mutated before any append reslices the stack, so no stale pointer is
		// used across a reallocation (the loop re-fetches the top each iteration).
		fr := &c.stack[len(c.stack)-1]
		if fr.lo >= fr.hi {
			c.stack = c.stack[:len(c.stack)-1]
			continue
		}
		var p int
		if c.reverse {
			fr.hi--
			p = fr.hi
		} else {
			p = fr.lo
			fr.lo++
		}
		if fr.isLeaf {
			row, err := fr.node.rowAt(p)
			if err != nil {
				return nil, nil, false, err
			}
			return fr.node.keys[p], row, true, nil
		}
		child, e := resolveChild(fr.node.children[p], c.src)
		if e != nil {
			return nil, nil, false, e
		}
		c.stack = append(c.stack, newScanFrame(child, c.bound))
	}
	return nil, nil, false, nil
}

// insOut is the result of inserting into a subtree: a whole rebuilt node, or a node that overflowed
// and split into left, a SEPARATOR key for the parent, and right. A leaf split COPIES the right
// leaf's first key up (no record leaves the leaf level); an interior split PUSHES its median
// separator up (format.md "Fan-out").
type insOut struct {
	whole *pnode // non-nil ⇒ no split
	left  *pnode
	sep   []byte
	right *pnode
}

// splitPoint is the kind-shared split decision (format.md "Split point"): given the per-boundary
// leftpayload/rightpayload functions over m in [mLo, mHi], pick
// m = rightEdge ? m_max : clamp(min(m_balanced, m_max), m_min, m_max), or ok=false when no m in the
// range keeps both sides fitting (the interior merge-abandon case — unreachable on the insert path,
// format.md "Why the record cap"). leftpayload is nondecreasing in m and rightpayload nonincreasing,
// so both bounds scan cleanly; the ranges are tiny (page fan-out), so a linear scan is clearest.
func splitPoint(mLo, mHi, payload, cap int, rightEdge bool, leftpayload, rightpayload func(int) int) (int, bool) {
	mMax, haveMax := 0, false
	for m := mLo; m <= mHi; m++ {
		if leftpayload(m) <= cap {
			mMax, haveMax = m, true
		} else {
			break
		}
	}
	if !haveMax {
		return 0, false
	}
	mMin, haveMin := 0, false
	for m := mHi; m >= mLo; m-- {
		if rightpayload(m) <= cap {
			mMin, haveMin = m, true
		} else {
			break
		}
	}
	if !haveMin || mMin > mMax {
		return 0, false
	}
	if rightEdge {
		return mMax, true
	}
	mBalanced := mMax
	for m := mLo; m <= mHi; m++ {
		if 2*leftpayload(m) >= payload {
			mBalanced = m
			break
		}
	}
	m := mBalanced
	if m > mMax {
		m = mMax
	}
	if m < mMin {
		m = mMin
	}
	return m, true
}

// buildLeaf builds a leaf from its parts; if its payload overflows cap, it splits 2-way COPY-UP
// (format.md "Leaf split"): the left leaf keeps records [0, m), the right leaf [m, N), and the
// separator handed up is a COPY of keys[m] (the right leaf's first key). edited is the index of the
// just-inserted/replaced record (-1 for the delete path's merge-overflow, which splits balanced). A
// leaf with a single over-cap record is left whole (defensive — the oversize surfaces as 0A000 when
// serialized).
func buildLeaf(keys [][]byte, vals []storedRow, weights []uint32, cap int, shape leafShape, edited int) insOut {
	n := len(keys)
	payload := 0
	for _, w := range weights {
		payload += int(w)
	}
	payload += leafOverhead(n, shape)
	if payload <= cap || n < 2 {
		return insOut{whole: &pnode{keys: keys, vals: vals, weights: weights}}
	}
	prefix := make([]int, n+1)
	for i, w := range weights {
		prefix[i+1] = prefix[i] + int(w)
	}
	total := prefix[n]
	leftpayload := func(m int) int { return prefix[m] + leafOverhead(m, shape) }
	rightpayload := func(m int) int { return total - prefix[m] + leafOverhead(n-m, shape) }
	m, ok := splitPoint(1, n-1, payload, cap, edited == n-1, leftpayload, rightpayload)
	if !ok {
		// Unreachable under the RECORD_MAX cap (a two-record leaf always fits — format.md "Why the
		// record cap"); defensively leave the node whole (0A000 at serialize).
		return insOut{whole: &pnode{keys: keys, vals: vals, weights: weights}}
	}
	return insOut{
		left:  &pnode{keys: cloneKeys(keys[:m]), vals: cloneVals(vals[:m]), weights: cloneWeights(weights[:m])},
		sep:   keys[m],
		right: &pnode{keys: cloneKeys(keys[m:]), vals: cloneVals(vals[m:]), weights: cloneWeights(weights[m:])},
	}
}

// buildInterior builds an interior node from its parts; if its payload overflows cap, it splits
// 2-way PUSH-UP (format.md "Interior split"): the left node keeps separators [0, m) + children
// [0, m], separator m moves up, the right node keeps [m+1, N) + children [m+1, N]. With N = 2 (only
// reachable with near-cap separators) the split is pinned to m = 1, producing a legal N = 0 right
// interior (the degenerate fan-out contract). Returns ok=false when the node overflows and no valid
// split point exists — the caller (only the interior MERGE path can hit it) abandons the merge.
func buildInterior(keys [][]byte, children []childRef, cap int, edited int) (insOut, bool) {
	n := len(keys)
	payload := 8*n + 4
	for _, k := range keys {
		payload += len(k)
	}
	if payload <= cap || n < 2 {
		return insOut{whole: &pnode{keys: keys, children: children}}, true
	}
	var m int
	if n == 2 {
		// The degenerate pin (format.md "Interior split"): the left keeps sep[0] (fits, by the
		// minimum-fanout invariant), sep[1] moves up, the right is the legal N = 0 interior.
		m = 1
	} else {
		prefix := make([]int, n+1)
		for i, k := range keys {
			prefix[i+1] = prefix[i] + len(k)
		}
		total := prefix[n]
		leftpayload := func(m int) int { return 8*m + 4 + prefix[m] }
		rightpayload := func(m int) int { return 8*(n-1-m) + 4 + (total - prefix[m+1]) }
		var ok bool
		m, ok = splitPoint(1, n-2, payload, cap, edited == n-1, leftpayload, rightpayload)
		if !ok {
			return insOut{}, false
		}
	}
	return insOut{
		left:  &pnode{keys: cloneKeys(keys[:m]), children: cloneChildren(children[:m+1])},
		sep:   keys[m],
		right: &pnode{keys: cloneKeys(keys[m+1:]), children: cloneChildren(children[m+1:])},
	}, true
}

// nodeInsert is the recursive insert. It descends to the holding leaf (interior nodes only route,
// via childSlot); on overwrite it sets *old/*replaced and rebuilds with the value+weight replaced
// (which may now overflow). Splits propagate back up: a leaf split copies its boundary key up, an
// interior receiving a separator may push-split in turn.
func nodeInsert(n *pnode, key []byte, val storedRow, weight uint32, old *storedRow, replaced *bool, src leafSource, cap int, shape leafShape) (insOut, error) {
	if n.isLeaf() {
		i, found := n.search(key)
		var keys [][]byte
		var vals []storedRow
		var weights []uint32
		if found {
			rows, err := n.decodedRows()
			if err != nil {
				return insOut{}, err
			}
			*old = rows[i]
			*replaced = true
			rows[i] = val
			keys = cloneKeys(n.keys)
			vals = rows
			weights = cloneWeights(n.weights)
			weights[i] = weight
		} else {
			rows, err := n.decodedRows()
			if err != nil {
				return insOut{}, err
			}
			keys = insertKeyAt(n.keys, i, key)
			vals = insertValAt(rows, i, val)
			weights = insertWeightAt(n.weights, i, weight)
		}
		return buildLeaf(keys, vals, weights, cap, shape, i), nil
	}
	// Fault the target child (a resident interior, or an OnDisk leaf brought in for mutation — it
	// becomes a dirty resident node on the rebuilt path).
	i := n.childSlot(key)
	childNode, err := resolveChild(n.children[i], src)
	if err != nil {
		return insOut{}, err
	}
	sub, err := nodeInsert(childNode, key, val, weight, old, replaced, src, cap, shape)
	if err != nil {
		return insOut{}, err
	}
	if sub.whole != nil {
		// This node's separators are unchanged, so it cannot overflow — rebuild whole.
		children := cloneChildren(n.children)
		children[i] = residentRef(sub.whole)
		return insOut{whole: &pnode{keys: cloneKeys(n.keys), children: children}}, nil
	}
	keys := insertKeyAt(n.keys, i, sub.sep)
	children := cloneChildren(n.children)
	children[i] = residentRef(sub.left)
	children = insertChildAt(children, i+1, residentRef(sub.right))
	out, ok := buildInterior(keys, children, cap, i)
	if !ok {
		panic("insert-path interior split always has a valid split point")
	}
	return out, nil
}

// underfull: a non-root node is underfull when its payload is below half a page (cap/2), the
// threshold at which delete rebalances it (format.md "Delete"). The root is exempt.
func underfull(n *pnode, cap int, shape leafShape) bool {
	return n.payload(shape) < cap/2
}

// nodeRemove is the recursive delete (copy-on-write). It descends to the holding LEAF (a separator
// equal to the key just routes right — it is never itself deleted or replaced; separators may go
// stale, format.md "Delete"). Returns the rebuilt subtree (possibly underfull — the caller
// rebalances it) and the removed row. The touched child is rebalanced via rebalanceChild.
func nodeRemove(n *pnode, key []byte, src leafSource, cap int, shape leafShape) (*pnode, storedRow, bool, error) {
	if n.isLeaf() {
		i, found := n.search(key)
		if !found {
			return n, nil, false, nil
		}
		rows, err := n.decodedRows()
		if err != nil {
			return nil, nil, false, err
		}
		vals, removed := removeValAt(rows, i)
		return &pnode{keys: removeKeyAt(n.keys, i), vals: vals, weights: removeWeightAt(n.weights, i)}, removed, true, nil
	}
	i := n.childSlot(key)
	childNode, err := resolveChild(n.children[i], src)
	if err != nil {
		return nil, nil, false, err
	}
	newChild, removed, ok, err := nodeRemove(childNode, key, src, cap, shape)
	if err != nil {
		return nil, nil, false, err
	}
	if !ok {
		return n, nil, false, nil
	}
	children := cloneChildren(n.children)
	children[i] = residentRef(newChild)
	rebuilt := &pnode{keys: cloneKeys(n.keys), children: children}
	out, err := rebalanceChild(rebuilt, i, src, cap, shape)
	if err != nil {
		return nil, nil, false, err
	}
	return out, removed, true, nil
}

// rebalanceChild: if children[i] is underfull, merge it with an adjacent sibling (prefer the right
// one), then split the merged node back if it overflows — the unified rebalance (no borrow). The
// returned parent may itself have lost a key and become underfull; its own parent handles that as
// the recursion unwinds.
func rebalanceChild(n *pnode, i int, src leafSource, cap int, shape leafShape) (*pnode, error) {
	// children[i] was just rebuilt resident by nodeRemove, so inspecting it faults nothing.
	childNode, err := resolveChild(n.children[i], src)
	if err != nil {
		return nil, err
	}
	if !underfull(childNode, cap, shape) {
		return n, nil
	}
	if len(n.children) < 2 {
		// A 0-key interior (one child, the degenerate max-separator shape) has no sibling to merge
		// with — its own parent merges IT away; the root case collapses in pMap.Remove.
		return n, nil
	}
	j := i
	if i+1 >= len(n.children) {
		j = i - 1
	}
	return mergeAt(n, j, src, cap, shape)
}

// mergeAt merges children[j] and children[j+1] into one node M (format.md "Delete"): a LEAF merge
// concatenates the two record lists and the parent separator j is REMOVED (it was a routing copy —
// nothing comes down); an INTERIOR merge PULLS the separator DOWN between the two key lists (the
// merged children need a routing key between them). If M fits, it replaces the pair (the parent
// loses one key); if it overflows, it is split 2-way by the balanced rule and the halves + the new
// separator replace the pair (the parent's key count is unchanged). An INTERIOR M that overflows
// but admits no valid split (near-cap separators) ABANDONS the merge — the parent is returned
// unchanged (format.md "Delete", the deterministic abandon rule).
func mergeAt(n *pnode, j int, src leafSource, cap int, shape leafShape) (*pnode, error) {
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

	var merged insOut
	if left.isLeaf() {
		// Materialize both leaves (either may be Packed) before merging — the merged node is Decoded.
		leftRows, err := left.decodedRows()
		if err != nil {
			return nil, err
		}
		rightRows, err := right.decodedRows()
		if err != nil {
			return nil, err
		}
		mkeys := make([][]byte, 0, len(left.keys)+len(right.keys))
		mkeys = append(mkeys, left.keys...)
		mkeys = append(mkeys, right.keys...)
		mvals := make([]storedRow, 0, len(leftRows)+len(rightRows))
		mvals = append(mvals, leftRows...)
		mvals = append(mvals, rightRows...)
		mweights := make([]uint32, 0, len(left.weights)+len(right.weights))
		mweights = append(mweights, left.weights...)
		mweights = append(mweights, right.weights...)
		merged = buildLeaf(mkeys, mvals, mweights, cap, shape, -1) // merge-overflow: balanced
	} else {
		mkeys := make([][]byte, 0, len(left.keys)+1+len(right.keys))
		mkeys = append(mkeys, left.keys...)
		mkeys = append(mkeys, n.keys[j]) // the separator pulls down
		mkeys = append(mkeys, right.keys...)
		mchildren := make([]childRef, 0, len(left.children)+len(right.children))
		mchildren = append(mchildren, left.children...)
		mchildren = append(mchildren, right.children...)
		var ok bool
		merged, ok = buildInterior(mkeys, mchildren, cap, -1)
		if !ok {
			// No valid 2-way split point (near-cap separators): abandon the merge — the two
			// children and the parent separator stay exactly as they were (underfull tolerated).
			return n, nil
		}
	}

	keys := cloneKeys(n.keys)
	children := cloneChildren(n.children)
	if merged.whole != nil {
		keys = removeKeyAt(keys, j)
		children[j] = residentRef(merged.whole)
		children = removeChildAt(children, j+1)
		return &pnode{keys: keys, children: children}, nil
	}
	keys[j] = merged.sep
	children[j] = residentRef(merged.left)
	children[j+1] = residentRef(merged.right)
	return &pnode{keys: keys, children: children}, nil
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

func cloneVals(s []storedRow) []storedRow {
	if len(s) == 0 {
		return nil
	}
	out := make([]storedRow, len(s))
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

func insertValAt(s []storedRow, i int, x storedRow) []storedRow {
	out := make([]storedRow, len(s)+1)
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

func removeValAt(s []storedRow, i int) ([]storedRow, storedRow) {
	removed := s[i]
	out := make([]storedRow, len(s)-1)
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
