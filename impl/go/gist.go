package jed

// GiST access method — the operation-deterministic R-tree (spec/design/gist.md).
//
// Two opclasses share one tree core (gist.md §2 — the type-specific part is the *only* part that
// differs): range_ops (GX1) over a range column accelerating && and @>, and the scalar `=` opclass
// (GX2, the in-core btree_gist equivalent) over a fixed-width keyable scalar column accelerating =.
// A range_ops bound is the row's exact range (leaf) / covering union (interior) via encodeRangeBody;
// a scalar `=` bound is [min,max] over the ORDER-PRESERVING KEY ENCODING (gist.md §6) — the executor
// encodes a value to its key bytes and the tree only ever COMPARES those bytes (no decode, no
// per-type comparator, no collation; the fixed-width set). This file is the self-contained core — the
// in-memory R-tree (build / penalty / median split), the on-disk node codec (the §4.1 byte layout,
// page types 5/6), and the consistent-descent search.
//
// Determinism (gist.md §3): every operation is a pure function of its inputs, so the identical
// mutation sequence every core replays builds the byte-identical tree. Within a node, entries are
// ordered canonically (gistBoundTotalCmp, ties by storage key / subtree-min key), so a node's bytes
// are a pure function of its entry set; pages are assigned in a canonical post-order walk. This is the
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
// Contains (@>); the scalar `=` opclass accelerates Equal (=).
type gistStrategy int

const (
	gistOverlaps gistStrategy = iota
	gistContains
	gistEqual
)

// gistOpclass is the operator class — the only type-specific part (gist.md §2). scalar=false is
// range_ops over a range column whose element ColType is elem; scalar=true is the `=` opclass over a
// fixed-width keyable scalar (whose bound is opaque key bytes the executor produces — elem unused).
type gistOpclass struct {
	scalar bool
	elem   ColType // range_ops only
}

// gistOpclassFor returns the opclass for a GiST index over a column of type ty (gist.md §5/§6):
// range_ops for a range column, the scalar `=` opclass otherwise (the CREATE INDEX gate guarantees a
// supported column type, so a non-range column here is a fixed-width keyable scalar).
func gistOpclassFor(ty Type) gistOpclass {
	if rt, ok := ty.RangeElement(); ok {
		return gistOpclass{scalar: false, elem: ScalarColType(rt.Scalar)}
	}
	return gistOpclass{scalar: true}
}

// gistBound is a bounding key: a range value (range_ops — rng non-nil) or a [min,max] pair over the
// order-preserving key encoding (scalar `=` — smin/smax, rng nil). A leaf's scalar bound is the
// degenerate [v,v]. The kind is dispatched on rng != nil, so comparison / union need no opclass.
type gistBound struct {
	rng        *RangeVal // range_ops
	smin, smax []byte    // scalar `=`: order-preserving key bytes
}

// gistQuery is a search query operand: a range constant (rng) for &&/@>, or a scalar equality
// constant's order-preserving KEY bytes (skey) for =.
type gistQuery struct {
	rng  *RangeVal
	skey []byte
}

// encodeBound serializes a bounding key to its self-delimiting bytes (no outer length prefix — the
// node codec adds the bound_len framing; the leaf-store key relies on this being self-delimiting to
// split off the trailing storage key).
func (op gistOpclass) encodeBound(b gistBound) []byte {
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

// readBound reads one self-delimiting bounding key starting at *pos, advancing it past the bound.
func (op gistOpclass) readBound(buf []byte, pos *int) (gistBound, error) {
	if !op.scalar {
		v, err := readRangeBody(op.elem, buf, pos)
		if err != nil {
			return gistBound{}, err
		}
		if v.Kind != ValRange {
			return gistBound{}, NewError(DataCorrupted, "gist: bound is not a range")
		}
		return gistBound{rng: v.Range}, nil
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

// gistBoundTotalCmp is the canonical total order over bounding keys (gist.md §3): rangeTotalCmp for
// ranges; the [min,max] key bytes lexicographically for scalars (the order-preserving key encoding
// makes raw byte order reproduce value order). Dispatches on the bound kind (rng != nil).
func gistBoundTotalCmp(a, b gistBound) int {
	if a.rng != nil {
		return rangeTotalCmp(a.rng, b.rng)
	}
	if c := bytes.Compare(a.smin, b.smin); c != 0 {
		return c
	}
	return bytes.Compare(a.smax, b.smax)
}

// gistBoundUnion is the covering union of two bounding keys — the convex-hull merge for ranges; the
// componentwise [min(min), max(max)] (byte-wise, the order-preserving key order) for scalars.
func gistBoundUnion(a, b gistBound) gistBound {
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

type gistLeafEntry struct {
	bound gistBound
	skey  []byte
}

type gistChildEntry struct {
	bound gistBound
	node  *gistNode
}

// gistNode is a leaf of row entries or an interior of child entries (each carrying its subtree's
// covering union as its bound). Unlike the ordered B-tree, an interior holds ONE bound per child.
type gistNode struct {
	leaf     bool
	entries  []gistLeafEntry  // when leaf
	children []gistChildEntry // when interior
}

// gistTree is an operation-deterministic GiST R-tree over a single column (range or scalar opclass).
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

// insert one row's (bounding key, storage key) into the tree under op.
func (t *gistTree) insert(op gistOpclass, bound gistBound, skey []byte) {
	if sib := gistInsertNode(t.root, op, bound, skey); sib != nil {
		// The root split: grow a new interior root over the old root (left) + the sibling.
		left := t.root
		children := []gistChildEntry{{bound: gistNodeUnion(left), node: left}, *sib}
		gistSortChildren(children)
		t.root = &gistNode{leaf: false, children: children}
	}
	t.len++
}

// search is the consistent-descent search: every storage key whose row satisfies the query under
// strat. The interior descend predicate is conservative (no false negatives); the exact operator is
// applied at the leaf. Returns (storage keys, nodesVisited, interiorVisited) — nodesVisited
// (interior + leaf) is the page_read charge, interiorVisited the gist_descent charge (gist.md §9).
func (t *gistTree) search(query gistQuery, strat gistStrategy) (out [][]byte, nodes, interior int) {
	gistSearchNode(t.root, query, strat, &out, &nodes, &interior)
	return
}

// gistChooseChild picks the child to descend on insert: the one whose union, merged with the new
// entry, has the lexicographically-smallest serialized bound bytes; ties keep the lower slot
// (penalty).
func gistChooseChild(children []gistChildEntry, op gistOpclass, bound gistBound) int {
	best := 0
	var bestKey []byte
	for i := range children {
		key := op.encodeBound(gistBoundUnion(children[i].bound, bound))
		if bestKey == nil || bytes.Compare(key, bestKey) < 0 {
			best = i
			bestKey = key
		}
	}
	return best
}

// gistInsertNode inserts into node, returning a new right-sibling child when the node split.
func gistInsertNode(node *gistNode, op gistOpclass, bound gistBound, skey []byte) *gistChildEntry {
	if node.leaf {
		node.entries = append(node.entries, gistLeafEntry{bound: bound, skey: skey})
		gistSortLeaf(node.entries)
	} else {
		i := gistChooseChild(node.children, op, bound)
		sib := gistInsertNode(node.children[i].node, op, bound, skey)
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
func gistNodeUnion(node *gistNode) gistBound {
	if node.leaf {
		u := node.entries[0].bound
		for i := 1; i < len(node.entries); i++ {
			u = gistBoundUnion(u, node.entries[i].bound)
		}
		return u
	}
	u := node.children[0].bound
	for i := 1; i < len(node.children); i++ {
		u = gistBoundUnion(u, node.children[i].bound)
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
		if c := gistBoundTotalCmp(entries[i].bound, entries[j].bound); c != 0 {
			return c < 0
		}
		return bytes.Compare(entries[i].skey, entries[j].skey) < 0
	})
}

func gistSortChildren(children []gistChildEntry) {
	// Recompute the subtree-min tiebreak inside the comparator (fan-out is tiny) so it tracks the
	// live element under SliceStable's swaps — a precomputed by-index slice would misalign.
	sort.SliceStable(children, func(i, j int) bool {
		if c := gistBoundTotalCmp(children[i].bound, children[j].bound); c != 0 {
			return c < 0
		}
		return bytes.Compare(gistSubtreeMinSkey(children[i].node), gistSubtreeMinSkey(children[j].node)) < 0
	})
}

// gistDescendPred is the conservative interior descend predicate (gist.md §5/§6). For && and @>, a
// matching row must overlap the query, and every row is contained in its subtree's union, so a
// non-overlapping union can hold no match — overlaps prunes safely. For =, a matching value must lie
// within the subtree's [min,max] key interval, so a query key outside it prunes safely.
func gistDescendPred(union gistBound, query gistQuery, strat gistStrategy) bool {
	switch strat {
	case gistOverlaps, gistContains:
		return rangeOverlaps(union.rng, query.rng)
	case gistEqual:
		return bytes.Compare(union.smin, query.skey) <= 0 && bytes.Compare(query.skey, union.smax) <= 0
	}
	return false
}

// gistLeafMatches is the exact operator, applied at the leaf to keep only true matches. A leaf's
// scalar bound is the degenerate [v,v], so equality is min == query key.
func gistLeafMatches(bound gistBound, query gistQuery, strat gistStrategy) bool {
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

func gistSearchNode(node *gistNode, query gistQuery, strat gistStrategy, out *[][]byte, nodes, interior *int) {
	*nodes++
	if node.leaf {
		for i := range node.entries {
			if gistLeafMatches(node.entries[i].bound, query, strat) {
				*out = append(*out, node.entries[i].skey)
			}
		}
		return
	}
	*interior++
	for i := range node.children {
		if gistDescendPred(node.children[i].bound, query, strat) {
			gistSearchNode(node.children[i].node, query, strat, out, nodes, interior)
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
func serializeGistTree(t *gistTree, op gistOpclass, alloc func() uint32) ([]gistPage, uint32) {
	var pages []gistPage
	root := gistSerializeNode(t.root, op, &pages, alloc)
	return pages, root
}

func gistSerializeNode(node *gistNode, op gistOpclass, pages *[]gistPage, alloc func() uint32) uint32 {
	if node.leaf {
		var payload []byte
		for i := range node.entries {
			b := op.encodeBound(node.entries[i].bound)
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
		childPages[i] = gistSerializeNode(node.children[i].node, op, pages, alloc)
	}
	var payload []byte
	for i := range node.children {
		b := op.encodeBound(node.children[i].bound)
		payload = appendU16(payload, uint16(len(b)))
		payload = append(payload, b...)
		payload = appendU32(payload, childPages[i])
	}
	pageNo := alloc()
	*pages = append(*pages, gistPage{pageNo: pageNo, pageType: pageGistInterior, itemCount: uint32(len(node.children)), payload: payload})
	return pageNo
}

// ---- the leaf-key codec + canonical-order build (the executor/serializer API) -----------------

// rangeGistLeafKey builds a range_ops leaf-store key for one row (the GIN term ‖ skey pattern): the
// row range's self-delimiting encodeRangeBody bytes then its storage key.
func rangeGistLeafKey(elem ScalarType, rv *RangeVal, skey []byte) []byte {
	return encodeGistLeafKey(gistOpclass{scalar: false, elem: ScalarColType(elem)}, gistBound{rng: rv}, skey)
}

// scalarGistLeafKey builds a scalar `=` leaf-store key for one row: the value's order-preserving KEY
// bytes as the degenerate [v,v] bound, then its storage key. valueKey is encodeKeyValue of the row's
// scalar value — the executor computes it (gist.go never encodes a value, only compares bytes).
func scalarGistLeafKey(valueKey, skey []byte) []byte {
	return encodeGistLeafKey(gistOpclass{scalar: true}, gistBound{smin: valueKey, smax: valueKey}, skey)
}

// encodeGistLeafKey is the leaf-store key = the bound's self-delimiting bytes ‖ the storage key.
func encodeGistLeafKey(op gistOpclass, bound gistBound, skey []byte) []byte {
	k := op.encodeBound(bound)
	return append(k, skey...)
}

// decodeGistLeafKey splits a leaf-store key back into (bound, storage key) — the inverse of
// encodeGistLeafKey.
func decodeGistLeafKey(op gistOpclass, key []byte) (gistBound, []byte, error) {
	pos := 0
	b, err := op.readBound(key, &pos)
	if err != nil {
		return gistBound{}, nil, err
	}
	return b, append([]byte(nil), key[pos:]...), nil
}

// buildGistFromLeafKeys builds the persisted R-tree from the index store's leaf keys. The keys are
// decoded and inserted in CANONICAL order (gistBoundTotalCmp, ties by storage key), so the tree is a
// pure function of the leaf SET — content-deterministic, independent of the original mutation order
// (gist.md §3); the cross-core / golden round-trip property the build relies on.
func buildGistFromLeafKeys(op gistOpclass, keys [][]byte) (*gistTree, error) {
	type entry struct {
		bound gistBound
		skey  []byte
	}
	entries := make([]entry, 0, len(keys))
	for _, k := range keys {
		bound, skey, err := decodeGistLeafKey(op, k)
		if err != nil {
			return nil, err
		}
		entries = append(entries, entry{bound: bound, skey: skey})
	}
	sort.SliceStable(entries, func(i, j int) bool {
		if c := gistBoundTotalCmp(entries[i].bound, entries[j].bound); c != 0 {
			return c < 0
		}
		return bytes.Compare(entries[i].skey, entries[j].skey) < 0
	})
	t := newGistTree()
	for _, e := range entries {
		t.insert(op, e.bound, e.skey)
	}
	return t, nil
}

// gistLeafKeysOf flattens a tree back to its leaf keys (encodeGistLeafKey per row) — used on load to
// rebuild the index store from the persisted R-tree. Order is irrelevant (the store re-sorts).
func gistLeafKeysOf(t *gistTree, op gistOpclass) [][]byte {
	out := make([][]byte, 0, t.len)
	gistCollectLeafKeys(t.root, op, &out)
	return out
}

func gistCollectLeafKeys(node *gistNode, op gistOpclass, out *[][]byte) {
	if node.leaf {
		for i := range node.entries {
			*out = append(*out, encodeGistLeafKey(op, node.entries[i].bound, node.entries[i].skey))
		}
		return
	}
	for i := range node.children {
		gistCollectLeafKeys(node.children[i].node, op, out)
	}
}

// readGistLeafKeys walks a persisted GiST R-tree (rooted at root, page types 5/6), marking every
// node page in reached (so the free-list keeps the live tree) and collecting each leaf's leaf key
// (bound ‖ skey — the opclass's self-delimiting bound bytes concatenated with the storage key).
// OPCLASS-AGNOSTIC: the bound bytes are copied verbatim (range body or [min,max] key blob), so no
// element type is needed. read returns one page's (pageType, itemCount, payload).
func readGistLeafKeys(read func(uint32) (byte, uint32, []byte, error), pageNo uint32, reached map[uint32]bool, out *[][]byte) error {
	reached[pageNo] = true
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
			if err := readGistLeafKeys(read, cp, reached, out); err != nil {
				return err
			}
		}
		return nil
	default:
		return NewError(DataCorrupted, "expected a GiST node page")
	}
}

func gistReadU16(buf []byte, pos *int) (int, error) {
	if *pos+2 > len(buf) {
		return 0, NewError(DataCorrupted, "gist: truncated u16")
	}
	v := int(binary.BigEndian.Uint16(buf[*pos:]))
	*pos += 2
	return v, nil
}

func gistReadU32(buf []byte, pos *int) (uint32, error) {
	if *pos+4 > len(buf) {
		return 0, NewError(DataCorrupted, "gist: truncated u32")
	}
	v := binary.BigEndian.Uint32(buf[*pos:])
	*pos += 4
	return v, nil
}

func gistTakeBytes(buf []byte, pos *int, n int) ([]byte, error) {
	if *pos+n > len(buf) {
		return nil, NewError(DataCorrupted, "gist: truncated bytes")
	}
	v := append([]byte(nil), buf[*pos:*pos+n]...)
	*pos += n
	return v, nil
}
