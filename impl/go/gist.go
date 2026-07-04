package jed

// GiST access method — the operation-deterministic R-tree (spec/design/gist.md).
//
// A GiST index covers ONE OR MORE columns (gist.md §4/§7), each with its own opclass. The opclasses
// this feature ships: range_ops (GX1) over a range column accelerating && and @>, and the scalar `=`
// opclass (GX2, the in-core btree_gist equivalent) over a fixed-width keyable scalar column
// accelerating =. A range_ops component bound is the row's exact range (leaf) / covering union
// (interior) via encodeRangeBody; a scalar `=` component bound is [min,max] over the ORDER-PRESERVING
// KEY ENCODING (gist.md §6) — the executor encodes a value to its key bytes and the tree only ever
// COMPARES those bytes (no decode, no per-type comparator, no collation; the fixed-width set).
//
// A MULTI-COLUMN index (GX3, the backing structure of an EXCLUDE constraint, gist.md §7) carries one
// component bound per column — its tree bound is the TUPLE of per-column bounds, compared
// lexicographically, unioned componentwise, and descended/rechecked by the conjunction (descend iff
// EVERY column's component is consistent). A single-column index is the one-component case, and its
// on-disk bytes are unchanged by this generalization (a one-element tuple encodes to exactly the
// single component's bytes — the GX1/GX2 goldens hold).
//
// This file is the self-contained core — the in-memory R-tree (build / penalty / median split), the
// on-disk node codec (the §4.1 byte layout, page types 5/6), and the consistent-descent search.
//
// Determinism (gist.md §3): every operation is a pure function of its inputs, so the identical
// mutation sequence every core replays builds the byte-identical tree. Within a node, entries are
// ordered canonically (gistTupleCmp, ties by storage key / subtree-min key), so a node's bytes are a
// pure function of its entry set; pages are assigned in a canonical post-order walk. This is the
// lockstep port of impl/rust/src/gist.rs (CLAUDE.md §2) — byte-identical by construction.

import (
	"bytes"
	"encoding/binary"
	"sort"
)

// gistFanout is the maximum entries per GiST node (gist.md §4.1); the (N+1)-th triggers a median
// picksplit. A pinned cross-core constant.
const gistFanout = 4

// GiST page types (gist.md §4.1, format.md *Page header*).
const (
	pageGistLeaf     byte = 5
	pageGistInterior byte = 6
)

// gistStrategy is the query operator a GiST opclass serves. range_ops accelerates Overlaps (&&) and
// Contains (@>); the scalar `=` opclass accelerates Equal (=). A multi-column probe (an EXCLUDE
// conjunction) supplies one strategy per column.
type gistStrategy int

const (
	gistOverlaps gistStrategy = iota
	gistContains
	gistEqual
)

// gistOpclass is one column's operator class — the only type-specific part (gist.md §2).
// scalar=false is range_ops over a range column whose element ColType is elem; scalar=true is the `=`
// opclass over a fixed-width keyable scalar (whose bound is opaque key bytes the executor produces —
// elem unused). A multi-column index threads one per column.
type gistOpclass struct {
	scalar bool
	elem   colType // range_ops only
}

// gistOpclassFor returns the opclass for a GiST index column of type ty (gist.md §5/§6): range_ops
// for a range column, the scalar `=` opclass otherwise (the gate guarantees a supported column type,
// so a non-range column here is a fixed-width keyable scalar).
func gistOpclassFor(ty dataType) gistOpclass {
	if rt, ok := ty.RangeElement(); ok {
		return gistOpclass{scalar: false, elem: scalarColType(rt.Scalar)}
	}
	return gistOpclass{scalar: true}
}

// gistOpclassesFor returns the per-column opclasses of a GiST index (one per indexed column).
func gistOpclassesFor(cols []int, columns []catColumn) []gistOpclass {
	ops := make([]gistOpclass, len(cols))
	for i, ci := range cols {
		ops[i] = gistOpclassFor(columns[ci].Type)
	}
	return ops
}

// gistBound is one column's bounding key: a range value (range_ops — rng non-nil) or a [min,max] pair
// over the order-preserving key encoding (scalar `=` — smin/smax, rng nil). A leaf's scalar component
// is the degenerate [v,v]. The kind is dispatched on rng != nil. A tree bound is a []gistBound — one
// component per indexed column (length 1 for the GX1/GX2 single-column indexes).
type gistBound struct {
	rng        *RangeVal // range_ops
	smin, smax []byte    // scalar `=`: order-preserving key bytes
}

// gistQuery is one column's search operand: a range constant (rng) for &&/@>, or a scalar equality
// constant's order-preserving KEY bytes (skey) for =. A multi-column probe supplies one per column.
type gistQuery struct {
	rng  *RangeVal
	skey []byte
}

// encodeComp serializes one component bound to its self-delimiting bytes (no outer length prefix —
// the node codec adds the bound_len framing over the whole tuple).
func (op gistOpclass) encodeComp(b gistBound) []byte {
	if !op.scalar {
		return encodeRangeBody(op.elem, b.rng)
	}
	// [min, max], each a length-prefixed key blob — self-delimiting and width-agnostic.
	out := appendU16(nil, uint16(len(b.smin)))
	out = append(out, b.smin...)
	out = appendU16(out, uint16(len(b.smax)))
	out = append(out, b.smax...)
	return out
}

// readComp reads one self-delimiting component bound starting at *pos, advancing it past the bound.
func (op gistOpclass) readComp(buf []byte, pos *int) (gistBound, error) {
	if !op.scalar {
		v, err := readRangeBody(op.elem, buf, pos, decodeConstruct)
		if err != nil {
			return gistBound{}, err
		}
		if v.Kind != ValRange {
			return gistBound{}, newError(DataCorrupted, "gist: bound is not a range")
		}
		return gistBound{rng: v.rangeVal()}, nil
	}
	mlen, err := gistReadU16(buf, pos)
	if err != nil {
		return gistBound{}, err
	}
	min, err := gistTakeBytes(buf, pos, mlen)
	if err != nil {
		return gistBound{}, err
	}
	xlen, err := gistReadU16(buf, pos)
	if err != nil {
		return gistBound{}, err
	}
	max, err := gistTakeBytes(buf, pos, xlen)
	if err != nil {
		return gistBound{}, err
	}
	return gistBound{smin: min, smax: max}, nil
}

// encodeBoundTuple serializes a whole tuple bound (one component per opclass) — the components
// concatenated in column order. For a single-column index this is exactly the one component's bytes.
func encodeBoundTuple(ops []gistOpclass, bound []gistBound) []byte {
	var out []byte
	for i := range ops {
		out = append(out, ops[i].encodeComp(bound[i])...)
	}
	return out
}

// readBoundTuple reads a whole tuple bound (one component per opclass) starting at *pos.
func readBoundTuple(ops []gistOpclass, buf []byte, pos *int) ([]gistBound, error) {
	bound := make([]gistBound, len(ops))
	for i := range ops {
		b, err := ops[i].readComp(buf, pos)
		if err != nil {
			return nil, err
		}
		bound[i] = b
	}
	return bound, nil
}

// gistCompTotalCmp is the canonical total order over one component bound (gist.md §3): rangeTotalCmp
// for ranges; the [min,max] key bytes lexicographically for scalars. Dispatches on rng != nil.
func gistCompTotalCmp(a, b gistBound) int {
	if a.rng != nil {
		return rangeTotalCmp(a.rng, b.rng)
	}
	if c := bytes.Compare(a.smin, b.smin); c != 0 {
		return c
	}
	return bytes.Compare(a.smax, b.smax)
}

// gistTupleCmp is the canonical total order over a tuple bound: lexicographic over its components.
func gistTupleCmp(a, b []gistBound) int {
	for i := range a {
		if c := gistCompTotalCmp(a[i], b[i]); c != 0 {
			return c
		}
	}
	return 0
}

// gistCompUnion is the covering union of two component bounds — the convex-hull merge for ranges; the
// componentwise [min(min), max(max)] (byte-wise, the order-preserving key order) for scalars.
func gistCompUnion(a, b gistBound) gistBound {
	if a.rng != nil {
		return gistBound{rng: mustUnion(a.rng, b.rng)}
	}
	min := a.smin
	if bytes.Compare(b.smin, min) < 0 {
		min = b.smin
	}
	max := a.smax
	if bytes.Compare(b.smax, max) > 0 {
		max = b.smax
	}
	return gistBound{smin: min, smax: max}
}

// gistTupleUnion is the componentwise covering union of two tuple bounds.
func gistTupleUnion(a, b []gistBound) []gistBound {
	out := make([]gistBound, len(a))
	for i := range a {
		out[i] = gistCompUnion(a[i], b[i])
	}
	return out
}

type gistLeafEntry struct {
	bound []gistBound
	skey  []byte
}

type gistChildEntry struct {
	bound []gistBound
	node  *gistNode
}

// gistNode is a leaf of row entries or an interior of child entries (each carrying its subtree's
// covering union as its bound). Unlike the ordered B-tree, an interior holds ONE bound per child.
type gistNode struct {
	leaf     bool
	entries  []gistLeafEntry  // when leaf
	children []gistChildEntry // when interior
}

// gistTree is an operation-deterministic GiST R-tree over one or more columns.
type gistTree struct {
	root *gistNode
	len  int
}

func newGistTree() *gistTree { return &gistTree{root: &gistNode{leaf: true}, len: 0} }

func (t *gistTree) isEmpty() bool { return t.len == 0 }

// mustUnion is rangeUnion(a, b, strict=false) — the convex hull, which never errors.
func mustUnion(a, b *RangeVal) *RangeVal {
	u, err := rangeUnion(a, b, false)
	if err != nil {
		panic("range_merge is total")
	}
	return u
}

// insert one row's (tuple bound, storage key) into the tree under ops.
func (t *gistTree) insert(ops []gistOpclass, bound []gistBound, skey []byte) {
	if sib := gistInsertNode(t.root, ops, bound, skey); sib != nil {
		// The root split: grow a new interior root over the old root (left) + the sibling.
		left := t.root
		children := []gistChildEntry{{bound: gistNodeUnion(left), node: left}, *sib}
		gistSortChildren(children)
		t.root = &gistNode{leaf: false, children: children}
	}
	t.len++
}

// search is the consistent-descent search: every storage key whose row satisfies the per-column
// query under the matching per-column strategy (a conjunction — descend iff EVERY component is
// consistent; recheck the exact operators at the leaf). query and strats are one entry per indexed
// column. Returns (storage keys, nodesVisited, interiorVisited) — nodesVisited (interior + leaf) is
// the page_read charge, interiorVisited the gist_descent charge (gist.md §9).
func (t *gistTree) search(query []gistQuery, strats []gistStrategy) (out [][]byte, nodes, interior int) {
	gistSearchNode(t.root, query, strats, &out, &nodes, &interior)
	return
}

// gistChooseChild picks the child to descend on insert: the one whose union, merged with the new
// entry, has the lexicographically-smallest serialized bound bytes; ties keep the lower slot
// (penalty).
func gistChooseChild(children []gistChildEntry, ops []gistOpclass, bound []gistBound) int {
	best := 0
	var bestKey []byte
	for i := range children {
		key := encodeBoundTuple(ops, gistTupleUnion(children[i].bound, bound))
		if bestKey == nil || bytes.Compare(key, bestKey) < 0 {
			best = i
			bestKey = key
		}
	}
	return best
}

// gistInsertNode inserts into node, returning a new right-sibling child when the node split.
func gistInsertNode(node *gistNode, ops []gistOpclass, bound []gistBound, skey []byte) *gistChildEntry {
	if node.leaf {
		node.entries = append(node.entries, gistLeafEntry{bound: bound, skey: skey})
		gistSortLeaf(node.entries)
	} else {
		i := gistChooseChild(node.children, ops, bound)
		sib := gistInsertNode(node.children[i].node, ops, bound, skey)
		// The chosen child's union may have shrunk (after a split below) or grown; recompute it.
		node.children[i].bound = gistNodeUnion(node.children[i].node)
		if sib != nil {
			node.children = append(node.children, *sib)
		}
		gistSortChildren(node.children)
	}
	return gistSplitIfOverflow(node)
}

// gistSplitIfOverflow splits an over-fan-out node at the median (entries already in canonical order)
// and returns the new right sibling; otherwise nil.
func gistSplitIfOverflow(node *gistNode) *gistChildEntry {
	if node.leaf {
		if len(node.entries) <= gistFanout {
			return nil
		}
		mid := (len(node.entries) + 1) / 2 // ceil(n/2)
		right := &gistNode{leaf: true, entries: append([]gistLeafEntry(nil), node.entries[mid:]...)}
		node.entries = node.entries[:mid]
		return &gistChildEntry{bound: gistNodeUnion(right), node: right}
	}
	if len(node.children) <= gistFanout {
		return nil
	}
	mid := (len(node.children) + 1) / 2
	right := &gistNode{leaf: false, children: append([]gistChildEntry(nil), node.children[mid:]...)}
	node.children = node.children[:mid]
	return &gistChildEntry{bound: gistNodeUnion(right), node: right}
}

// gistNodeUnion is the covering union of a node's entries (the convex-hull merge — never errors).
// The node must be non-empty (the empty tree's root leaf is never unioned).
func gistNodeUnion(node *gistNode) []gistBound {
	if node.leaf {
		u := node.entries[0].bound
		for i := 1; i < len(node.entries); i++ {
			u = gistTupleUnion(u, node.entries[i].bound)
		}
		return u
	}
	u := node.children[0].bound
	for i := 1; i < len(node.children); i++ {
		u = gistTupleUnion(u, node.children[i].bound)
	}
	return u
}

// gistSubtreeMinSkey is the smallest storage key anywhere in the subtree — a deterministic,
// sibling-unique tiebreak for canonical interior ordering.
func gistSubtreeMinSkey(node *gistNode) []byte {
	if node.leaf {
		min := node.entries[0].skey
		for i := 1; i < len(node.entries); i++ {
			if bytes.Compare(node.entries[i].skey, min) < 0 {
				min = node.entries[i].skey
			}
		}
		return min
	}
	min := gistSubtreeMinSkey(node.children[0].node)
	for i := 1; i < len(node.children); i++ {
		if s := gistSubtreeMinSkey(node.children[i].node); bytes.Compare(s, min) < 0 {
			min = s
		}
	}
	return min
}

func gistSortLeaf(entries []gistLeafEntry) {
	sort.SliceStable(entries, func(i, j int) bool {
		if c := gistTupleCmp(entries[i].bound, entries[j].bound); c != 0 {
			return c < 0
		}
		return bytes.Compare(entries[i].skey, entries[j].skey) < 0
	})
}

func gistSortChildren(children []gistChildEntry) {
	// Recompute the subtree-min tiebreak inside the comparator (fan-out is tiny) so it tracks the
	// live element under SliceStable's swaps — a precomputed by-index slice would misalign.
	sort.SliceStable(children, func(i, j int) bool {
		if c := gistTupleCmp(children[i].bound, children[j].bound); c != 0 {
			return c < 0
		}
		return bytes.Compare(gistSubtreeMinSkey(children[i].node), gistSubtreeMinSkey(children[j].node)) < 0
	})
}

// gistDescendComp is the conservative interior descend predicate for one column (gist.md §5/§6). For
// && and @>, a matching row must overlap the query, and every row is contained in its subtree's
// union, so a non-overlapping union can hold no match. For =, a matching value must lie within the
// subtree's [min,max] key interval.
func gistDescendComp(union gistBound, query gistQuery, strat gistStrategy) bool {
	switch strat {
	case gistOverlaps, gistContains:
		return rangeOverlaps(union.rng, query.rng)
	case gistEqual:
		return bytes.Compare(union.smin, query.skey) <= 0 && bytes.Compare(query.skey, union.smax) <= 0
	}
	return false
}

// gistDescend descends into a child iff EVERY column's component is consistent with its query (a
// conjunction — the exclusion-probe and single-column descent are the one- and many-column cases).
func gistDescend(union []gistBound, query []gistQuery, strats []gistStrategy) bool {
	for i := range union {
		if !gistDescendComp(union[i], query[i], strats[i]) {
			return false
		}
	}
	return true
}

// gistLeafMatchComp is the exact operator for one column, applied at the leaf. A leaf's scalar
// component is the degenerate [v,v], so equality is min == query key.
func gistLeafMatchComp(bound gistBound, query gistQuery, strat gistStrategy) bool {
	switch strat {
	case gistOverlaps:
		return rangeOverlaps(bound.rng, query.rng)
	case gistContains:
		return rangeContains(bound.rng, query.rng)
	case gistEqual:
		return bytes.Equal(bound.smin, query.skey)
	}
	return false
}

// gistLeafMatches: a leaf row matches iff EVERY column's exact operator is TRUE (the full
// conjunction). For a single-column index this is the lone operator; for an EXCLUDE probe it is the
// whole (expr_i op_i) conjunction, so a leaf hit IS a conflicting row (gist.md §7).
func gistLeafMatches(bound []gistBound, query []gistQuery, strats []gistStrategy) bool {
	for i := range bound {
		if !gistLeafMatchComp(bound[i], query[i], strats[i]) {
			return false
		}
	}
	return true
}

func gistSearchNode(node *gistNode, query []gistQuery, strats []gistStrategy, out *[][]byte, nodes, interior *int) {
	*nodes++
	if node.leaf {
		for i := range node.entries {
			if gistLeafMatches(node.entries[i].bound, query, strats) {
				*out = append(*out, node.entries[i].skey)
			}
		}
		return
	}
	*interior++
	for i := range node.children {
		if gistDescend(node.children[i].bound, query, strats) {
			gistSearchNode(node.children[i].node, query, strats, out, nodes, interior)
		}
	}
}

// ---- on-disk node codec (gist.md §4.1) -------------------------------------------------------

// gistPage is one serialized GiST node page: its page number, type (leaf 5 / interior 6), the entry
// count (the page header's item_count), and the payload bytes after the 16-byte header. Page
// allocation is post-order (children before parent, the root last) so page numbers are a
// deterministic function of the tree.
type gistPage struct {
	pageNo    uint32
	pageType  byte
	itemCount uint32
	payload   []byte
}

// serializeGistTree serializes the whole tree to its node pages in canonical post-order (children
// before parent, the root last). alloc hands out the next page number. Returns the pages (each with
// its allocated number) and the root page.
func serializeGistTree(t *gistTree, ops []gistOpclass, alloc func() uint32) ([]gistPage, uint32) {
	var pages []gistPage
	root := gistSerializeNode(t.root, ops, &pages, alloc)
	return pages, root
}

func gistSerializeNode(node *gistNode, ops []gistOpclass, pages *[]gistPage, alloc func() uint32) uint32 {
	if node.leaf {
		var payload []byte
		for i := range node.entries {
			b := encodeBoundTuple(ops, node.entries[i].bound)
			payload = appendU16(payload, uint16(len(b)))
			payload = append(payload, b...)
			payload = appendU16(payload, uint16(len(node.entries[i].skey)))
			payload = append(payload, node.entries[i].skey...)
		}
		pageNo := alloc()
		*pages = append(*pages, gistPage{pageNo: pageNo, pageType: pageGistLeaf, itemCount: uint32(len(node.entries)), payload: payload})
		return pageNo
	}
	// Children first (post-order), in the node's canonical entry order.
	childPages := make([]uint32, len(node.children))
	for i := range node.children {
		childPages[i] = gistSerializeNode(node.children[i].node, ops, pages, alloc)
	}
	var payload []byte
	for i := range node.children {
		b := encodeBoundTuple(ops, node.children[i].bound)
		payload = appendU16(payload, uint16(len(b)))
		payload = append(payload, b...)
		payload = appendU32(payload, childPages[i])
	}
	pageNo := alloc()
	*pages = append(*pages, gistPage{pageNo: pageNo, pageType: pageGistInterior, itemCount: uint32(len(node.children)), payload: payload})
	return pageNo
}

// ---- the leaf-key codec + canonical-order build (the executor/serializer API) -----------------

// gistLeafKey builds a row's leaf-store key from its tuple bound (the GIN term ‖ skey pattern): each
// component's self-delimiting bytes in column order, then the storage key. For a single-column index
// the bytes equal the one component's encoding (the GX1/GX2 leaf-store form is unchanged).
func gistLeafKey(ops []gistOpclass, bound []gistBound, skey []byte) []byte {
	return append(encodeBoundTuple(ops, bound), skey...)
}

// rangeGistLeafKey builds a single-column range_ops leaf-store key (the GX1 convenience).
func rangeGistLeafKey(elem scalarType, rv *RangeVal, skey []byte) []byte {
	ops := []gistOpclass{{scalar: false, elem: scalarColType(elem)}}
	return gistLeafKey(ops, []gistBound{{rng: rv}}, skey)
}

// scalarGistLeafKey builds a single-column scalar `=` leaf-store key (the GX2 convenience): the
// value's order-preserving KEY bytes as the degenerate [v,v] bound, then its storage key. valueKey is
// encodeKeyValue of the row's scalar value — the executor computes it (gist.go never encodes a value).
func scalarGistLeafKey(valueKey, skey []byte) []byte {
	return gistLeafKey([]gistOpclass{{scalar: true}}, []gistBound{{smin: valueKey, smax: valueKey}}, skey)
}

// decodeGistLeafKey splits a leaf-store key back into (tuple bound, storage key) — the inverse of
// gistLeafKey (each component is self-delimiting, so the remainder is the storage key).
func decodeGistLeafKey(ops []gistOpclass, key []byte) ([]gistBound, []byte, error) {
	pos := 0
	b, err := readBoundTuple(ops, key, &pos)
	if err != nil {
		return nil, nil, err
	}
	return b, append([]byte(nil), key[pos:]...), nil
}

// buildGistFromLeafKeys builds the persisted R-tree from the index store's leaf keys. The keys are
// decoded and inserted in CANONICAL order (gistTupleCmp, ties by storage key), so the tree is a pure
// function of the leaf SET — content-deterministic, independent of the original mutation order
// (gist.md §3); the cross-core / golden round-trip property the build relies on.
func buildGistFromLeafKeys(ops []gistOpclass, keys [][]byte) (*gistTree, error) {
	type entry struct {
		bound []gistBound
		skey  []byte
	}
	entries := make([]entry, 0, len(keys))
	for _, k := range keys {
		bound, skey, err := decodeGistLeafKey(ops, k)
		if err != nil {
			return nil, err
		}
		entries = append(entries, entry{bound: bound, skey: skey})
	}
	sort.SliceStable(entries, func(i, j int) bool {
		if c := gistTupleCmp(entries[i].bound, entries[j].bound); c != 0 {
			return c < 0
		}
		return bytes.Compare(entries[i].skey, entries[j].skey) < 0
	})
	t := newGistTree()
	for _, e := range entries {
		t.insert(ops, e.bound, e.skey)
	}
	return t, nil
}

// gistLeafKeysOf flattens a tree back to its leaf keys (gistLeafKey per row) — used on load to
// rebuild the index store from the persisted R-tree. Order is irrelevant (the store re-sorts).
func gistLeafKeysOf(t *gistTree, ops []gistOpclass) [][]byte {
	out := make([][]byte, 0, t.len)
	gistCollectLeafKeys(t.root, ops, &out)
	return out
}

func gistCollectLeafKeys(node *gistNode, ops []gistOpclass, out *[][]byte) {
	if node.leaf {
		for i := range node.entries {
			*out = append(*out, gistLeafKey(ops, node.entries[i].bound, node.entries[i].skey))
		}
		return
	}
	for i := range node.children {
		gistCollectLeafKeys(node.children[i].node, ops, out)
	}
}

// readGistLeafKeys walks a persisted GiST R-tree (rooted at root, page types 5/6), collecting each
// leaf's leaf key (bound ‖ skey — the tuple's self-delimiting bound bytes concatenated with the
// storage key). OPCLASS-AGNOSTIC: the whole bound blob is copied verbatim (single- or multi-column),
// so no element type is needed. read returns one page's (pageType, itemCount, payload).
func readGistLeafKeys(read func(uint32) (byte, uint32, []byte, error), pageNo uint32, out *[][]byte) error {
	pageType, n, payload, err := read(pageNo)
	if err != nil {
		return err
	}
	pos := 0
	switch pageType {
	case pageGistLeaf:
		for i := uint32(0); i < n; i++ {
			blen, err := gistReadU16(payload, &pos)
			if err != nil {
				return err
			}
			bound, err := gistTakeBytes(payload, &pos, blen)
			if err != nil {
				return err
			}
			slen, err := gistReadU16(payload, &pos)
			if err != nil {
				return err
			}
			skey, err := gistTakeBytes(payload, &pos, slen)
			if err != nil {
				return err
			}
			key := append(append([]byte(nil), bound...), skey...)
			*out = append(*out, key)
		}
		return nil
	case pageGistInterior:
		children := make([]uint32, 0, n)
		for i := uint32(0); i < n; i++ {
			blen, err := gistReadU16(payload, &pos)
			if err != nil {
				return err
			}
			if _, err := gistTakeBytes(payload, &pos, blen); err != nil { // skip the union bound
				return err
			}
			cp, err := gistReadU32(payload, &pos)
			if err != nil {
				return err
			}
			children = append(children, cp)
		}
		for _, cp := range children {
			if err := readGistLeafKeys(read, cp, out); err != nil {
				return err
			}
		}
		return nil
	default:
		return newError(DataCorrupted, "expected a GiST node page")
	}
}

func gistReadU16(buf []byte, pos *int) (int, error) {
	if *pos+2 > len(buf) {
		return 0, newError(DataCorrupted, "gist: truncated u16")
	}
	v := int(binary.BigEndian.Uint16(buf[*pos:]))
	*pos += 2
	return v, nil
}

func gistReadU32(buf []byte, pos *int) (uint32, error) {
	if *pos+4 > len(buf) {
		return 0, newError(DataCorrupted, "gist: truncated u32")
	}
	v := binary.BigEndian.Uint32(buf[*pos:])
	*pos += 4
	return v, nil
}

func gistTakeBytes(buf []byte, pos *int, n int) ([]byte, error) {
	if *pos+n > len(buf) {
		return nil, newError(DataCorrupted, "gist: truncated bytes")
	}
	v := append([]byte(nil), buf[*pos:*pos+n]...)
	*pos += n
	return v, nil
}
