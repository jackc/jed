package jed

// GiST access method — the operation-deterministic R-tree (spec/design/gist.md).
//
// GX1 ships the range_ops opclass: a GiST index over a range column, accelerating the overlap (&&)
// and containment (@>) operators. This file is the self-contained core — the in-memory R-tree
// (build / penalty / median split), the on-disk node codec (the §4.1 byte layout, page types 5/6),
// and the consistent-descent search. Catalog/format integration (IndexGist, the grammar,
// formatVersion 20, the planner gather) is wired separately and reuses these primitives.
//
// Determinism (gist.md §3): every operation is a pure function of its inputs, so the identical
// mutation sequence every core replays builds the byte-identical tree. Within a node, entries are
// ordered canonically (rangeTotalCmp, ties by storage key / subtree-min key), so a node's bytes are
// a pure function of its entry set; pages are assigned in a canonical post-order walk. This is the
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

// gistStrategy is the query operator range_ops serves. GX1 accelerates Overlaps (&&) and Contains
// (@>); the positional operators, <@, =, and the empty-query edge cases stay full-scan this slice.
type gistStrategy int

const (
	gistOverlaps gistStrategy = iota
	gistContains
)

type gistLeafEntry struct {
	bound *RangeVal
	skey  []byte
}

type gistChildEntry struct {
	bound *RangeVal
	node  *gistNode
}

// gistNode is a leaf of row entries or an interior of child entries (each carrying its subtree's
// covering union as its bound). Unlike the ordered B-tree, an interior holds ONE bound per child.
type gistNode struct {
	leaf     bool
	entries  []gistLeafEntry  // when leaf
	children []gistChildEntry // when interior
}

// gistTree is an operation-deterministic GiST R-tree over a single range column.
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

// insert one row's (range bound, storage key) into the tree. elem is the range's element subtype,
// used by the value codec and the penalty metric.
func (t *gistTree) insert(elem ColType, bound *RangeVal, skey []byte) {
	if sib := gistInsertNode(t.root, elem, bound, skey); sib != nil {
		// The root split: grow a new interior root over the old root (left) + the sibling.
		left := t.root
		children := []gistChildEntry{{bound: gistNodeUnion(left), node: left}, *sib}
		gistSortChildren(children)
		t.root = &gistNode{leaf: false, children: children}
	}
	t.len++
}

// search is the consistent-descent search: every storage key whose row satisfies query OP col under
// strat. The interior descend predicate is conservative (no false negatives); the exact operator is
// applied at the leaf. Returns (storage keys, nodesVisited, interiorVisited) — nodesVisited
// (interior + leaf) is the page_read charge, interiorVisited the gist_descent charge (gist.md §9).
func (t *gistTree) search(query *RangeVal, strat gistStrategy) (out [][]byte, nodes, interior int) {
	gistSearchNode(t.root, query, strat, &out, &nodes, &interior)
	return
}

// gistChooseChild picks the child to descend on insert: the one whose union, merged with the new
// entry, has the lexicographically-smallest value-codec bytes; ties keep the lower slot (penalty).
func gistChooseChild(children []gistChildEntry, elem ColType, bound *RangeVal) int {
	best := 0
	var bestKey []byte
	for i := range children {
		key := encodeRangeBody(elem, mustUnion(children[i].bound, bound))
		if bestKey == nil || bytes.Compare(key, bestKey) < 0 {
			best = i
			bestKey = key
		}
	}
	return best
}

// gistInsertNode inserts into node, returning a new right-sibling child when the node split.
func gistInsertNode(node *gistNode, elem ColType, bound *RangeVal, skey []byte) *gistChildEntry {
	if node.leaf {
		node.entries = append(node.entries, gistLeafEntry{bound: bound, skey: skey})
		gistSortLeaf(node.entries)
	} else {
		i := gistChooseChild(node.children, elem, bound)
		sib := gistInsertNode(node.children[i].node, elem, bound, skey)
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
func gistNodeUnion(node *gistNode) *RangeVal {
	if node.leaf {
		u := node.entries[0].bound
		for i := 1; i < len(node.entries); i++ {
			u = mustUnion(u, node.entries[i].bound)
		}
		return u
	}
	u := node.children[0].bound
	for i := 1; i < len(node.children); i++ {
		u = mustUnion(u, node.children[i].bound)
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
		if c := rangeTotalCmp(entries[i].bound, entries[j].bound); c != 0 {
			return c < 0
		}
		return bytes.Compare(entries[i].skey, entries[j].skey) < 0
	})
}

func gistSortChildren(children []gistChildEntry) {
	// Recompute the subtree-min tiebreak inside the comparator (fan-out is tiny) so it tracks the
	// live element under SliceStable's swaps — a precomputed by-index slice would misalign.
	sort.SliceStable(children, func(i, j int) bool {
		if c := rangeTotalCmp(children[i].bound, children[j].bound); c != 0 {
			return c < 0
		}
		return bytes.Compare(gistSubtreeMinSkey(children[i].node), gistSubtreeMinSkey(children[j].node)) < 0
	})
}

// gistDescendPred is the conservative interior descend predicate (gist.md §5). For && and @>, a
// matching row must overlap the query, and every row is contained in its subtree's union, so a
// non-overlapping union can hold no match — overlaps prunes safely.
func gistDescendPred(union, query *RangeVal, strat gistStrategy) bool {
	switch strat {
	case gistOverlaps, gistContains:
		return rangeOverlaps(union, query)
	}
	return false
}

// gistLeafMatches is the exact operator, applied at the leaf to keep only true matches.
func gistLeafMatches(bound, query *RangeVal, strat gistStrategy) bool {
	switch strat {
	case gistOverlaps:
		return rangeOverlaps(bound, query)
	case gistContains:
		return rangeContains(bound, query)
	}
	return false
}

func gistSearchNode(node *gistNode, query *RangeVal, strat gistStrategy, out *[][]byte, nodes, interior *int) {
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
func serializeGistTree(t *gistTree, elem ColType, alloc func() uint32) ([]gistPage, uint32) {
	var pages []gistPage
	root := gistSerializeNode(t.root, elem, &pages, alloc)
	return pages, root
}

func gistSerializeNode(node *gistNode, elem ColType, pages *[]gistPage, alloc func() uint32) uint32 {
	if node.leaf {
		var payload []byte
		for i := range node.entries {
			b := encodeRangeBody(elem, node.entries[i].bound)
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
		childPages[i] = gistSerializeNode(node.children[i].node, elem, pages, alloc)
	}
	var payload []byte
	for i := range node.children {
		b := encodeRangeBody(elem, node.children[i].bound)
		payload = appendU16(payload, uint16(len(b)))
		payload = append(payload, b...)
		payload = appendU32(payload, childPages[i])
	}
	pageNo := alloc()
	*pages = append(*pages, gistPage{pageNo: pageNo, pageType: pageGistInterior, itemCount: uint32(len(node.children)), payload: payload})
	return pageNo
}

// ---- the leaf-key codec + canonical-order build (the executor/serializer API) -----------------

// encodeGistLeafKey is the index store's per-row key: encodeRangeBody(bound) ‖ storage key (the GIN
// term ‖ skey pattern). encodeRangeBody is self-delimiting, so decodeGistLeafKey recovers
// (bound, skey). This is what the GiST index maintenance produces, so all existing insert/update/
// delete index maintenance is reused unchanged.
func encodeGistLeafKey(elem ColType, bound *RangeVal, skey []byte) []byte {
	k := encodeRangeBody(elem, bound)
	return append(k, skey...)
}

// decodeGistLeafKey splits a leaf key back into (bound, storage key) — the inverse of
// encodeGistLeafKey.
func decodeGistLeafKey(elem ColType, key []byte) (*RangeVal, []byte, error) {
	pos := 0
	v, err := readRangeBody(elem, key, &pos)
	if err != nil {
		return nil, nil, err
	}
	if v.Kind != ValRange {
		return nil, nil, NewError(DataCorrupted, "gist: leaf key is not a range")
	}
	return v.Range, append([]byte(nil), key[pos:]...), nil
}

// buildGistFromLeafKeys builds the persisted R-tree from the index store's leaf keys. The keys are
// decoded and inserted in CANONICAL order (rangeTotalCmp, ties by storage key), so the tree is a
// pure function of the leaf SET — content-deterministic, independent of the original mutation order
// (gist.md §3); the cross-core / golden round-trip property the build relies on.
func buildGistFromLeafKeys(elem ColType, keys [][]byte) (*gistTree, error) {
	type entry struct {
		bound *RangeVal
		skey  []byte
	}
	entries := make([]entry, 0, len(keys))
	for _, k := range keys {
		bound, skey, err := decodeGistLeafKey(elem, k)
		if err != nil {
			return nil, err
		}
		entries = append(entries, entry{bound: bound, skey: skey})
	}
	sort.SliceStable(entries, func(i, j int) bool {
		if c := rangeTotalCmp(entries[i].bound, entries[j].bound); c != 0 {
			return c < 0
		}
		return bytes.Compare(entries[i].skey, entries[j].skey) < 0
	})
	t := newGistTree()
	for _, e := range entries {
		t.insert(elem, e.bound, e.skey)
	}
	return t, nil
}

// gistLeafKeysOf flattens a tree back to its leaf keys (encodeGistLeafKey per row) — used on load to
// rebuild the index store from the persisted R-tree. Order is irrelevant (the store re-sorts).
func gistLeafKeysOf(t *gistTree, elem ColType) [][]byte {
	out := make([][]byte, 0, t.len)
	gistCollectLeafKeys(t.root, elem, &out)
	return out
}

func gistCollectLeafKeys(node *gistNode, elem ColType, out *[][]byte) {
	if node.leaf {
		for i := range node.entries {
			*out = append(*out, encodeGistLeafKey(elem, node.entries[i].bound, node.entries[i].skey))
		}
		return
	}
	for i := range node.children {
		gistCollectLeafKeys(node.children[i].node, elem, out)
	}
}

// readGistLeafKeys walks a persisted GiST R-tree (rooted at root, page types 5/6), marking every
// node page in reached (so the free-list keeps the live tree) and collecting each leaf's leaf key
// (encodeRangeBody(bound) ‖ skey — the bound bytes concatenated with the storage key). Pure byte
// walk — no element type needed. read returns one page's (pageType, itemCount, payload).
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
