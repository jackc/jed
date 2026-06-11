package jed

// On-disk single-file format: serialize / load (spec/fileformat/format.md).
//
// Whole-image model (step-5b): a commit serializes the entire database to one byte
// image; loading reconstructs it. The byte layout is the canonical contract
// (spec/fileformat/format.md) and is verified byte-for-byte against shared goldens
// so a file written by this core is byte-identical to one written by the Rust core
// (CLAUDE.md §8). All multi-byte integers are big-endian.

import (
	"bytes"
	"encoding/binary"
	"sort"
	"strings"
	"unicode/utf8"
)

// magic — ASCII "JEDB" (the engine is named `jed`).
var magic = [4]byte{'J', 'E', 'D', 'B'}

const (
	formatVersion uint16 = 3               // on-disk format version (3 = + overflow pages, large-values.md §12)
	pageHeader           = 12              // bytes of the catalog/B-tree page header
	pageCatalog   byte   = 1               // page_type for a catalog page
	pageLeaf      byte   = 2               // page_type for a B-tree leaf node
	pageInterior  byte   = 3               // page_type for a B-tree interior node
	pageOverflow  byte   = 4               // page_type for an out-of-line value slab (large-values.md §12)
	rootPage      uint32 = 2               // catalog root of a fresh empty db (relocatable thereafter)
	minPageSize          = pageHeader + 36 // smallest valid page size (page + 36-byte meta header; format.md)
	maxPageSize          = 65536           // largest valid page size, 64 KiB (format.md *Page model*; CLAUDE.md §13)

	// Value-codec presence tags beyond 0x00 present-inline-plain / 0x01 NULL (large-values.md
	// §12/§13; format.md "Large values"): 0x02 external-plain (u32 first_page + u32 payload_len),
	// 0x03 inline-compressed (u32 raw_len + u16 comp_len + LZ4 block — lz4.md), 0x04
	// external-compressed (u32 first_page + u32 stored_len + u32 raw_len; the chain carries the
	// COMPRESSED block). The *Len constants are each form's full in-record size (tag included).
	tagExternal     byte = 0x02
	tagInlineComp   byte = 0x03
	tagExternalComp byte = 0x04
	externalPtrLen       = 1 + 4 + 4 // tag + first_page(u32) + payload_len(u32) in a record
	// inlineCompOverhead is the inline-compressed form's overhead: tag + raw_len(u32) + comp_len(u16).
	inlineCompOverhead = 1 + 4 + 2
	externalCompPtrLen = 1 + 4 + 4 + 4 // tag + first_page + stored_len + raw_len
	// sCompress: content payloads below this many bytes are never fed to the LZ4 encoder (header
	// overhead dominates; PostgreSQL pglz's default min_input_size — large-values.md §13).
	sCompress = 32
)

// typeCodeForScalar maps a scalar type to its stable on-disk code, independent of
// the in-memory iota discriminant (which may be reordered). See format.md.
func typeCodeForScalar(ty ScalarType) byte {
	switch ty {
	case Int16:
		return 1
	case Int32:
		return 2
	case Int64:
		return 3
	case Text:
		return 4
	case Bool:
		return 5
	case DecimalType:
		return 6
	case Bytea:
		return 7
	case Uuid:
		return 8
	case Timestamp:
		return 9
	case Timestamptz:
		return 10
	default:
		return 0
	}
}

// scalarForTypeCode is the inverse of typeCodeForScalar; ok=false for an unknown code.
func scalarForTypeCode(code byte) (ScalarType, bool) {
	switch code {
	case 1:
		return Int16, true
	case 2:
		return Int32, true
	case 3:
		return Int64, true
	case 4:
		return Text, true
	case 5:
		return Bool, true
	case 6:
		return DecimalType, true
	case 7:
		return Bytea, true
	case 8:
		return Uuid, true
	case 9:
		return Timestamp, true
	case 10:
		return Timestamptz, true
	default:
		return 0, false
	}
}

// crc32IEEE is CRC-32/IEEE (reflected, poly 0xEDB88320, init/final 0xFFFFFFFF) — the
// standard zlib CRC32, hand-rolled so no dependency is needed. Pinned by the vector
// crc32("123456789") == 0xCBF43926.
func crc32IEEE(data []byte) uint32 {
	crc := uint32(0xFFFFFFFF)
	for _, b := range data {
		crc ^= uint32(b)
		for i := 0; i < 8; i++ {
			mask := -(crc & 1) // 0xFFFFFFFF if low bit set, else 0
			crc = (crc >> 1) ^ (0xEDB88320 & mask)
		}
	}
	return ^crc
}

// encodeValue is the value codec (format.md): a 1-byte presence tag (0x01 = NULL), then
// the type's present-value body. Integers reuse the order-preserving key encoding; text is
// where the seam diverges — a stored text value needs no ordering, so it is a compact u16
// byte-length + UTF-8 bytes (collation C, verbatim). A text value whose UTF-8 length exceeds
// uint16's max is unsupported; in practice it also exceeds a page and is caught by the
// oversized-item rule in pack (0A000), so the cast here is sound for every supported page
// size (spec/fileformat/format.md). boolean is a single bool-byte body — 0x00 false, 0x01
// true (types.md §9).
func encodeValue(ty ScalarType, v Value) []byte {
	switch v.Kind {
	case ValNull:
		return EncodeNullable(ty, nil)
	case ValText, ValBytea:
		// text (UTF-8) and bytea (raw bytes) share the compact length-prefixed body; both
		// hold their bytes in Str, so the on-disk form is identical.
		out := make([]byte, 0, 3+len(v.Str))
		out = append(out, 0x00) // present
		out = appendU16(out, uint16(len(v.Str)))
		return append(out, v.Str...)
	case ValUuid:
		// Fixed 16-byte body, NO length prefix (the first fixed-width non-integer value) —
		// spec/fileformat/format.md. The 16 raw bytes live in Str.
		out := make([]byte, 0, 1+16)
		out = append(out, 0x00) // present
		return append(out, v.Str...)
	case ValBool:
		b := byte(0x00)
		if v.Bool {
			b = 0x01
		}
		return []byte{0x00, b} // present tag + bool-byte (0x00 false, 0x01 true)
	case ValDecimal:
		// Decimal value codec (spec/fileformat/format.md): tag, flags (sign), u16 scale, u16
		// ndigits, then that many big-endian base-10^4 coefficient groups (MS-first).
		neg, scale, groups := v.Dec.ToCodec()
		out := make([]byte, 0, 6+len(groups)*2)
		out = append(out, 0x00) // present
		var flags byte
		if neg {
			flags = 1 // bit0 = sign
		}
		out = append(out, flags)
		out = appendU16(out, uint16(scale))
		out = appendU16(out, uint16(len(groups)))
		for _, g := range groups {
			out = appendU16(out, g)
		}
		return out
	default:
		n := v.Int
		return EncodeNullable(ty, &n)
	}
}

func appendU16(b []byte, v uint16) []byte { return append(b, byte(v>>8), byte(v)) }
func appendU32(b []byte, v uint32) []byte {
	return append(b, byte(v>>24), byte(v>>16), byte(v>>8), byte(v))
}

// ToImage serializes the whole committed state to one on-disk image (format.md). A thin wrapper
// over Snapshot.ToImage for the committed snapshot — txid is written into both meta slots. (The
// writer's working snapshot is serialized directly via Snapshot.ToImage at commit; this serves
// callers/tests holding a *Database.)
func (db *Database) ToImage(pageSize uint32, txid uint64) ([]byte, error) {
	return db.committed.ToImage(pageSize, txid)
}

// ToImage serializes this snapshot's whole state to one on-disk image (format.md). pageSize
// is recorded in the meta page; txid is written into both meta slots.
func (s *Snapshot) ToImage(pageSize uint32, txid uint64) ([]byte, error) {
	ps := int(pageSize)
	if ps < minPageSize {
		return nil, NewError(FeatureNotSupported, "page size too small for the format")
	}
	if ps > maxPageSize {
		return nil, NewError(FeatureNotSupported, "page size too large for the format")
	}
	capacity := ps - pageHeader

	// Tables in ascending lowercased-name order (no map-iteration order leak).
	keys := make([]string, 0, len(s.tables))
	for k := range s.tables {
		keys = append(keys, k)
	}
	sort.Strings(keys)

	// Serialize each table's B-tree post-order, body pages allocated from page 2. Each entry is
	// (index, page_type, item_count, payload); children precede their parent so parent child-pointers
	// reference already-allocated pages (format.md).
	var body []bodyPage
	rootDataPage := make([]uint32, len(keys))
	nextIndex := rootPage
	for ti, k := range keys {
		if root := s.stores[k].treeRoot(); root != nil {
			rp, np, err := serializeNode(root, s.tables[k], capacity, nextIndex, &body)
			if err != nil {
				return nil, err
			}
			rootDataPage[ti] = rp
			nextIndex = np
		}
	}

	// The catalog chain follows the data; its head is the relocatable root_page.
	catRoot := nextIndex
	entrySizes := make([]int, len(keys))
	for ti, k := range keys {
		entrySizes[ti] = len(tableEntryBytes(s.tables[k], 0))
	}
	catGroups, err := pack(entrySizes, capacity)
	if err != nil {
		return nil, err
	}
	pageCount := catRoot + uint32(len(catGroups))

	image := make([]byte, int(pageCount)*ps)

	// Meta: both slots hold the current meta (a fresh from-scratch image has no distinct prior
	// version; slot alternation is the live incremental-commit path — format.md).
	writeMeta(image, ps, 0, pageSize, txid, catRoot, pageCount)
	writeMeta(image, ps, 1, pageSize, txid, catRoot, pageCount)

	// B-tree node + overflow pages.
	for _, bp := range body {
		writePage(image, ps, int(bp.index), bp.pageType, bp.itemCount, bp.nextPage, bp.payload)
	}

	// Catalog chain.
	for gi, group := range catGroups {
		index := catRoot + uint32(gi)
		var next uint32
		if gi+1 < len(catGroups) {
			next = index + 1
		}
		var payload []byte
		for _, ti := range group {
			payload = append(payload, tableEntryBytes(s.tables[keys[ti]], rootDataPage[ti])...)
		}
		writePage(image, ps, int(index), pageCatalog, uint32(len(group)), next, payload)
	}

	return image, nil
}

// bodyPage is one serialized page awaiting write: its index, type, key count, chain link, payload.
// nextPage is 0 for B-tree nodes and the chain link for overflow pages (large-values.md §12).
type bodyPage struct {
	index     uint32
	pageType  byte
	itemCount uint32
	nextPage  uint32
	payload   []byte
}

// serializeNode serializes one node and its subtree post-order, appending each to *body, and returns
// this node's assigned page index and the next free index. A leaf's payload is its records; an
// interior's is its N+1 child pointers (big-endian u32) then its N records (format.md). A node whose
// payload would exceed the page is an oversized record (over RECORD_MAX) — feature_not_supported.
func serializeNode(n *pnode, table *Table, capacity int, nextIndex uint32, body *[]bodyPage) (uint32, uint32, error) {
	childPages := make([]uint32, len(n.children))
	for i, c := range n.children {
		// Whole-image serialize renumbers pages from scratch and runs only on a fully-resident
		// in-memory database (create's empty image, the golden generator) — a paged file commits
		// incrementally via serializeDirty. An OnDisk child would carry a page id from a different
		// layout, so it must not appear here.
		if c.node == nil {
			panic("whole-image serialize hit an OnDisk leaf")
		}
		cp, np, err := serializeNode(c.node, table, capacity, nextIndex, body)
		if err != nil {
			return 0, 0, err
		}
		childPages[i] = cp
		nextIndex = np
	}
	index := nextIndex
	nextIndex++

	var payload []byte
	pageType := pageLeaf
	if len(n.children) > 0 {
		pageType = pageInterior
		for _, cp := range childPages {
			payload = appendU32(payload, cp)
		}
	}
	// Encode records, spilling over-large values to overflow pages allocated after this node's index
	// (post-order traversal + column order → deterministic, golden-pinnable layout).
	var ovf []overflowPageOut
	take := func() uint32 { p := nextIndex; nextIndex++; return p }
	for i := range n.keys {
		payload = append(payload, encodeRecord(table, n.keys[i], n.vals[i], capacity, take, &ovf)...)
	}
	if len(payload) > capacity {
		return 0, 0, NewError(FeatureNotSupported, "a record larger than the per-row limit is not supported")
	}
	*body = append(*body, bodyPage{index: index, pageType: pageType, itemCount: uint32(len(n.keys)), payload: payload})
	for _, o := range ovf {
		*body = append(*body, bodyPage{index: o.index, pageType: pageOverflow, itemCount: o.itemCount, nextPage: o.nextPage, payload: o.payload})
	}
	return index, nextIndex, nil
}

// dirtyPage is one full pageSize image awaiting a pwrite at its index (P6.1 part B).
type dirtyPage struct {
	index uint32
	bytes []byte
}

// incrementalWrite is the pages an incremental commit must write durably, plus the new catalog root
// and high-water for the meta slot (spec/fileformat/format.md, P6.1 part B). file.go pwrites pages,
// then publishes rootPage/pageCount in the alternate meta slot.
type incrementalWrite struct {
	pages     []dirtyPage
	rootPage  uint32
	pageCount uint32
	// freeRemaining is the free-list entries this commit did not consume — the new free-list (P6.2).
	// file.go stores it back on the handle for the next commit (spec/fileformat/format.md *Reclamation*).
	freeRemaining []uint32
}

// pageAlloc hands out page indices for an incremental commit: the free-list first (lowest index, the
// pages a prior root abandoned — spec/fileformat/format.md *Reclamation*), then fresh indices at the
// high-water once the free-list is exhausted. The free-list is pre-sorted ascending, so lowest-first
// allocation is deterministic and the bytes stay cross-core identical. Reusing a free page is
// torn-write-safe: it left the free-list only here, becoming part of the new committed version, so it
// is reachable from no fallback snapshot.
type pageAlloc struct {
	free   []uint32
	cursor int
	next   uint32
}

func (a *pageAlloc) take() uint32 {
	if a.cursor < len(a.free) {
		p := a.free[a.cursor]
		a.cursor++
		return p
	}
	p := a.next
	a.next++
	return p
}

// incrementalImage assembles the dirty body pages + freshly-rewritten catalog for an incremental
// commit, appending page allocation from startPage (the on-disk high-water) — the write path's
// counterpart to the whole-image ToImage (spec/fileformat/format.md, *Allocation & incremental
// commit*). Only dirty nodes are emitted (clean subtrees keep their pages — the incremental win); the
// catalog chain is always rewritten (it carries each table's possibly-moved root). The dirty nodes'
// set-once page ids are assigned here. The page size was validated at file creation, so no size check
// is repeated.
func (s *Snapshot) incrementalImage(pageSize, startPage uint32, free []uint32) (incrementalWrite, error) {
	ps := int(pageSize)
	capacity := ps - pageHeader

	keys := make([]string, 0, len(s.tables))
	for k := range s.tables {
		keys = append(keys, k)
	}
	sort.Strings(keys)

	// Allocate from the free-list first (reclaiming dead pages), then extend the file.
	alloc := &pageAlloc{free: free, next: startPage}

	var pages []dirtyPage
	rootDataPage := make([]uint32, len(keys))
	for ti, k := range keys {
		if root := s.stores[k].treeRoot(); root != nil {
			rp, err := serializeDirty(root, s.tables[k], capacity, ps, alloc, &pages)
			if err != nil {
				return incrementalWrite{}, err
			}
			rootDataPage[ti] = rp
		}
	}

	// The catalog chain is rewritten to fresh pages every commit (table roots move). Allocate its
	// page indices up front — they may be reused free pages, hence not contiguous — so each page can
	// point at the next (pack always returns ≥ 1 group, so catPages is non-empty).
	entrySizes := make([]int, len(keys))
	for ti, k := range keys {
		entrySizes[ti] = len(tableEntryBytes(s.tables[k], 0))
	}
	catGroups, err := pack(entrySizes, capacity)
	if err != nil {
		return incrementalWrite{}, err
	}
	catPages := make([]uint32, len(catGroups))
	for i := range catPages {
		catPages[i] = alloc.take()
	}
	catRoot := catPages[0]
	for gi, group := range catGroups {
		var nextPage uint32
		if gi+1 < len(catGroups) {
			nextPage = catPages[gi+1]
		}
		var payload []byte
		for _, ti := range group {
			payload = append(payload, tableEntryBytes(s.tables[keys[ti]], rootDataPage[ti])...)
		}
		pages = append(pages, dirtyPage{index: catPages[gi], bytes: makePage(ps, pageCatalog, uint32(len(group)), nextPage, payload)})
	}

	return incrementalWrite{pages: pages, rootPage: catRoot, pageCount: alloc.next, freeRemaining: alloc.free[alloc.cursor:]}, nil
}

// serializeDirty assigns a page to one dirty node (and its dirty descendants) post-order, appending
// each as a full pageSize page to *pages, and returns this node's page index. A clean node (already
// persisted, page != 0) short-circuits: its whole subtree is on disk unchanged (copy-on-write only
// rebuilds the modified path), so nothing is written and its existing page is returned. The node's
// set-once page id is stored here — safe, as the working tree is owned by the single writer at commit.
// Page indices come from the allocator (free-list first, then the high-water). Mirrors serializeNode
// for the byte layout.
func serializeDirty(n *pnode, table *Table, capacity, ps int, alloc *pageAlloc, pages *[]dirtyPage) (uint32, error) {
	if n.page != 0 {
		return n.page, nil
	}
	childPages := make([]uint32, len(n.children))
	for i, c := range n.children {
		// A resident child recurses (dirty descendants get pages); an OnDisk child is a clean leaf
		// already durable at its page — keep it, write nothing (the incremental-commit win).
		if c.node == nil {
			childPages[i] = c.page
			continue
		}
		cp, err := serializeDirty(c.node, table, capacity, ps, alloc, pages)
		if err != nil {
			return 0, err
		}
		childPages[i] = cp
	}
	var payload []byte
	pageType := pageLeaf
	if len(n.children) > 0 {
		pageType = pageInterior
		for _, cp := range childPages {
			payload = appendU32(payload, cp)
		}
	}
	// Encode records, spilling over-large values to overflow pages drawn from the same allocator
	// (free-list first, then high-water — large-values.md §12).
	var ovf []overflowPageOut
	for i := range n.keys {
		payload = append(payload, encodeRecord(table, n.keys[i], n.vals[i], capacity, alloc.take, &ovf)...)
	}
	if len(payload) > capacity {
		return 0, NewError(FeatureNotSupported, "a record larger than the per-row limit is not supported")
	}
	index := alloc.take()
	n.page = index
	*pages = append(*pages, dirtyPage{index: index, bytes: makePage(ps, pageType, uint32(len(n.keys)), 0, payload)})
	for _, o := range ovf {
		*pages = append(*pages, dirtyPage{index: o.index, bytes: makePage(ps, pageOverflow, o.itemCount, o.nextPage, o.payload)})
	}
	return index, nil
}

// LoadDatabase reconstructs a database from an on-disk image (inverse of ToImage).
// Returns a structured data_corrupted (XX001) error for malformed input.
func LoadDatabase(image []byte) (*Database, error) {
	if len(image) < 12 {
		return nil, NewError(DataCorrupted, "image smaller than a meta header")
	}
	pageSize := int(binary.BigEndian.Uint32(image[8:12]))
	if pageSize < minPageSize || pageSize > maxPageSize || len(image) < pageSize*2 {
		return nil, NewError(DataCorrupted, "invalid page size")
	}
	mt, err := selectMeta(image, pageSize)
	if err != nil {
		return nil, err
	}

	// Build the committed snapshot from the image, then wrap it in a fresh handle that adopts the
	// file's serialization parameters (spec/design/api.md §2).
	snap := newSnapshot()
	snap.txid = mt.txid
	// Reconstruct the free-list (P6.2): collect every page reachable from the committed root — the
	// catalog chain plus each table's B-tree nodes — as we load it; the rest of [2, pageCount) is dead
	// space the next incremental commit may reuse (spec/fileformat/format.md *Reclamation*).
	reached := make(map[uint32]bool)
	catPage := mt.rootPage
	for catPage != 0 {
		reached[catPage] = true
		pg, err := readPage(image, pageSize, catPage)
		if err != nil {
			return nil, err
		}
		if pg.pageType != pageCatalog {
			return nil, NewError(DataCorrupted, "expected a catalog page")
		}
		pos := 0
		for i := uint32(0); i < pg.itemCount; i++ {
			table, tableRoot, err := decodeTableEntry(pg.payload, &pos)
			if err != nil {
				return nil, err
			}
			colTypes := make([]ScalarType, len(table.Columns))
			for j, c := range table.Columns {
				colTypes[j] = c.Type
			}
			name := table.Name
			hasPK := table.PrimaryKeyIndex() >= 0
			snap.putTable(table, uint32(pageSize))
			if tableRoot != 0 {
				root, length, err := readTree(image, pageSize, tableRoot, colTypes, reached)
				if err != nil {
					return nil, err
				}
				store := snap.stores[strings.ToLower(name)]
				store.setTree(root, length)
				// No-PK keys are synthetic int64 rowids — advance the counter past the largest (the
				// last entry in key order) so future inserts don't collide. In-memory load (nil
				// source) never faults, so the error is inert.
				if !hasPK && length > 0 {
					keys, _, err := store.rows.inorder(nil)
					if err != nil {
						return nil, err
					}
					store.BumpRowidTo(DecodeInt(Int64, keys[len(keys)-1]) + 1)
				}
			}
		}
		catPage = pg.nextPage
	}
	db := NewDatabase()
	db.pageSize = uint32(pageSize)
	db.pageCount = mt.pageCount // the on-disk high-water for the next incremental commit
	// The free-list: every body page [2, pageCount) the committed root does not reach (P6.2).
	// Ascending by construction, so the allocator reuses lowest-first.
	for p := rootPage; p < mt.pageCount; p++ {
		if !reached[p] {
			db.freePages = append(db.freePages, p)
		}
	}
	db.committed = snap
	return db, nil
}

// LoadDatabasePaged opens a file-backed database demand-paged (spec/design/pager.md, P6.4b): it loads
// only the interior B-tree skeleton resident, leaving each leaf an OnDisk page faulted through the
// bounded buffer pool on access — so the resident set is bounded by the pool, not the file size. The
// inverse of an incremental commit, reading pages through pgr instead of a whole image.
//
// This slice reads every leaf page once (to count its rows for length and mark it reachable for the
// free-list), then discards it — memory stays bounded (only the skeleton is retained), but open is
// O(pages). Making open O(skeleton) needs a per-subtree row count in the format (a deferred follow-on,
// pager.md §6); the residency win — a bounded resident set — already holds.
func LoadDatabasePaged(pgr *pager, capacity int) (*Database, error) {
	pageSize := int(pgr.pageSize)
	if pageSize < minPageSize || pageSize > maxPageSize {
		return nil, NewError(DataCorrupted, "invalid page size")
	}
	paging := newSharedPaging(pgr, capacity)

	// Select the live meta from slots 0 and 1 (highest valid txid; the lone valid slot on a torn
	// write), read as individual blocks through the pager.
	b0, err := pgr.readBlock(0)
	if err != nil {
		return nil, err
	}
	b1, err := pgr.readBlock(1)
	if err != nil {
		return nil, err
	}
	mt, ok := parseMeta(b0)
	if mb, okb := parseMeta(b1); okb && (!ok || mb.txid > mt.txid) {
		mt, ok = mb, true
	}
	if !ok {
		return nil, NewError(DataCorrupted, "no valid meta page")
	}

	snap := newSnapshot()
	snap.txid = mt.txid
	// Reconstruct the free-list (P6.2) from the pages the skeleton load marks reachable — every
	// interior node, plus each leaf's page id (recorded without retaining the leaf).
	reached := make(map[uint32]bool)
	catPage := mt.rootPage
	for catPage != 0 {
		reached[catPage] = true
		block, err := pgr.readBlock(catPage)
		if err != nil {
			return nil, err
		}
		pg, err := parsePage(block)
		if err != nil {
			return nil, err
		}
		if pg.pageType != pageCatalog {
			return nil, NewError(DataCorrupted, "expected a catalog page")
		}
		pos := 0
		for i := uint32(0); i < pg.itemCount; i++ {
			table, tableRoot, err := decodeTableEntry(pg.payload, &pos)
			if err != nil {
				return nil, err
			}
			colTypes := make([]ScalarType, len(table.Columns))
			for j, c := range table.Columns {
				colTypes[j] = c.Type
			}
			name := strings.ToLower(table.Name)
			hasPK := table.PrimaryKeyIndex() >= 0
			snap.putTable(table, uint32(pageSize))
			store := snap.stores[name]
			store.attachPaging(paging)
			if tableRoot != 0 {
				root, length, err := readSkeleton(paging, tableRoot, colTypes, reached)
				if err != nil {
					return nil, err
				}
				// The skeleton leaves leaves OnDisk (unread), so their records' overflow chains are
				// invisible to the reachability walk above. For a table with spillable columns, read
				// the leaves now to collect those live chains — else the free-list would reclaim still-
				// referenced overflow pages (large-values.md §12; default open is this paged path).
				// Dead chains still leak until the next open, matching the P6.2 orphan model.
				if anySpillable(colTypes) {
					if err := collectLeafOverflow(paging, tableRoot, colTypes, reached); err != nil {
						return nil, err
					}
				}
				store.setTree(root, length)
				if !hasPK && length > 0 {
					// No-PK rowid reconstruction faults the leaves to find the largest key; only for
					// keyless tables (most have a PK), bounded by the pool.
					keys, _, err := store.rows.inorder(store.leafSrc())
					if err != nil {
						return nil, err
					}
					store.BumpRowidTo(DecodeInt(Int64, keys[len(keys)-1]) + 1)
				}
			}
		}
		catPage = pg.nextPage
	}

	db := NewDatabase()
	db.pageSize = uint32(pageSize)
	db.pageCount = mt.pageCount
	for p := rootPage; p < mt.pageCount; p++ {
		if !reached[p] {
			db.freePages = append(db.freePages, p)
		}
	}
	db.committed = snap
	db.paging = paging
	return db, nil
}

// anySpillableMasked is anySpillable restricted to the columns a query's touched set selects —
// the gate for the masked scan-units walk (cost.md §3 "The touched set"): if no TOUCHED column
// can spill, the whole walk yields zero and is skipped.
func anySpillableMasked(colTypes []ScalarType, mask []bool) bool {
	for i, ty := range colTypes {
		if mask[i] && isSpillable(ty) {
			return true
		}
	}
	return false
}

// anySpillable reports whether any column type can spill out-of-line (large-values.md §12).
func anySpillable(colTypes []ScalarType) bool {
	for _, ty := range colTypes {
		if isSpillable(ty) {
			return true
		}
	}
	return false
}

// collectLeafOverflow walks a table's on-disk B-tree, reading each leaf and adding the overflow chain
// pages its records reference to reached (large-values.md §12). Interior separators are skipped here —
// readSkeletonNode already collected their chains. Used only for tables with spillable columns during
// the paged-open free-list reconstruction; it reads (and transiently materializes) every leaf, the
// deliberate cost of reconstruct-on-open reclamation for overflow.
func collectLeafOverflow(paging *sharedPaging, pageIdx uint32, colTypes []ScalarType, reached map[uint32]bool) error {
	block, err := paging.pgr.readBlock(pageIdx)
	if err != nil {
		return err
	}
	pg, err := parsePage(block)
	if err != nil {
		return err
	}
	switch pg.pageType {
	case pageLeaf:
		fetch := func(p uint32) ([]byte, error) { return paging.pgr.readBlock(p) }
		pos := 0
		for i := uint32(0); i < pg.itemCount; i++ {
			_, _, ovf, err := decodeRecord(colTypes, pg.payload, &pos, fetch)
			if err != nil {
				return err
			}
			for _, p := range ovf {
				reached[p] = true
			}
		}
		return nil
	case pageInterior:
		n := int(pg.itemCount)
		pos := 0
		cps := make([]uint32, 0, n+1)
		for i := 0; i < n+1; i++ {
			cp, err := readU32(pg.payload, &pos)
			if err != nil {
				return err
			}
			cps = append(cps, cp)
		}
		for _, cp := range cps {
			if err := collectLeafOverflow(paging, cp, colTypes, reached); err != nil {
				return err
			}
		}
		return nil
	default:
		return NewError(DataCorrupted, "expected a B-tree node page")
	}
}

// readSkeleton reads a table's on-disk B-tree (rooted at rootPage) into a demand-paged skeleton:
// interior nodes resident, each leaf left OnDisk. Returns the root node and the total row count. A
// table whose root is itself a single leaf has no interior parent to hold an OnDisk reference, so the
// root leaf is faulted resident (spec/design/pager.md §1/§4).
func readSkeleton(paging *sharedPaging, root uint32, colTypes []ScalarType, reached map[uint32]bool) (*pnode, int, error) {
	c, length, err := readSkeletonNode(paging, root, colTypes, reached)
	if err != nil {
		return nil, 0, err
	}
	if c.node != nil {
		return c.node, length, nil
	}
	node, err := paging.faultLeaf(c.page, colTypes)
	if err != nil {
		return nil, 0, err
	}
	return node, length, nil
}

// readSkeletonNode reads one B-tree node through the pager, once: a leaf becomes an OnDisk childRef
// (its rows counted from the header, then dropped — not retained); an interior node becomes a resident
// childRef with its children resolved recursively. Returns the child reference and the subtree's row
// count.
func readSkeletonNode(paging *sharedPaging, pageIdx uint32, colTypes []ScalarType, reached map[uint32]bool) (childRef, int, error) {
	reached[pageIdx] = true
	block, err := paging.pgr.readBlock(pageIdx)
	if err != nil {
		return childRef{}, 0, err
	}
	pg, err := parsePage(block)
	if err != nil {
		return childRef{}, 0, err
	}
	switch pg.pageType {
	case pageLeaf:
		return onDiskRef(pageIdx), int(pg.itemCount), nil
	case pageInterior:
		n := int(pg.itemCount)
		pos := 0
		children := make([]childRef, 0, n+1)
		total := 0
		for i := 0; i < n+1; i++ {
			cp, err := readU32(pg.payload, &pos)
			if err != nil {
				return childRef{}, 0, err
			}
			child, clen, err := readSkeletonNode(paging, cp, colTypes, reached)
			if err != nil {
				return childRef{}, 0, err
			}
			children = append(children, child)
			total += clen
		}
		keys, vals, weights := make([][]byte, 0, n), make([]Row, 0, n), make([]uint32, 0, n)
		capacity := len(block) - pageHeader
		fetch := func(p uint32) ([]byte, error) { return paging.pgr.readBlock(p) }
		for i := 0; i < n; i++ {
			key, row, ovf, err := decodeRecord(colTypes, pg.payload, &pos, fetch)
			if err != nil {
				return childRef{}, 0, err
			}
			weights = append(weights, uint32(recordSize(colTypes, key, row, capacity)))
			for _, p := range ovf {
				reached[p] = true
			}
			keys = append(keys, key)
			vals = append(vals, row)
		}
		total += n
		return residentRef(&pnode{keys: keys, vals: vals, weights: weights, children: children, page: pageIdx}), total, nil
	default:
		return childRef{}, 0, NewError(DataCorrupted, "expected a B-tree node page")
	}
}

// readTree reads a table's on-disk B-tree (rooted at pageIdx) into an in-memory tree, returning the
// root node and the total row count (spec/fileformat/format.md). An interior node's payload is its
// N+1 child pointers then its N records; we recurse the pointers, then read the separators. Weights
// are recomputed from the value codec (the exact size the writer used), so the loaded tree is ready
// for further size-driven splits.
func readTree(image []byte, ps int, pageIdx uint32, colTypes []ScalarType, reached map[uint32]bool) (*pnode, int, error) {
	reached[pageIdx] = true
	capacity := ps - pageHeader
	pg, err := readPage(image, ps, pageIdx)
	if err != nil {
		return nil, 0, err
	}
	fetch := func(p uint32) ([]byte, error) { return pageBlock(image, ps, p) }
	switch pg.pageType {
	case pageLeaf:
		n := int(pg.itemCount)
		keys, vals, weights := make([][]byte, 0, n), make([]Row, 0, n), make([]uint32, 0, n)
		pos := 0
		for i := 0; i < n; i++ {
			key, row, ovf, err := decodeRecord(colTypes, pg.payload, &pos, fetch)
			if err != nil {
				return nil, 0, err
			}
			weights = append(weights, uint32(recordSize(colTypes, key, row, capacity)))
			for _, p := range ovf {
				reached[p] = true
			}
			keys = append(keys, key)
			vals = append(vals, row)
		}
		return &pnode{keys: keys, vals: vals, weights: weights, page: pageIdx}, n, nil
	case pageInterior:
		n := int(pg.itemCount)
		pos := 0
		children := make([]childRef, 0, n+1)
		total := 0
		for i := 0; i < n+1; i++ {
			cp, err := readU32(pg.payload, &pos)
			if err != nil {
				return nil, 0, err
			}
			child, clen, err := readTree(image, ps, cp, colTypes, reached)
			if err != nil {
				return nil, 0, err
			}
			// The in-memory load is fully resident (no pager to fault from); the demand-paged file
			// load (LoadDatabasePaged) is a separate path that leaves leaf children OnDisk.
			children = append(children, residentRef(child))
			total += clen
		}
		keys, vals, weights := make([][]byte, 0, n), make([]Row, 0, n), make([]uint32, 0, n)
		for i := 0; i < n; i++ {
			key, row, ovf, err := decodeRecord(colTypes, pg.payload, &pos, fetch)
			if err != nil {
				return nil, 0, err
			}
			weights = append(weights, uint32(recordSize(colTypes, key, row, capacity)))
			for _, p := range ovf {
				reached[p] = true
			}
			keys = append(keys, key)
			vals = append(vals, row)
		}
		total += n
		return &pnode{keys: keys, vals: vals, weights: weights, children: children, page: pageIdx}, total, nil
	default:
		return nil, 0, NewError(DataCorrupted, "expected a B-tree node page")
	}
}

// isSpillable reports whether a value of this type can be stored out-of-line (a variable-length
// type). Fixed-width types are tiny and always stay inline (spec/design/large-values.md §12).
func isSpillable(ty ScalarType) bool {
	return ty.IsText() || ty.IsBytea() || ty.IsDecimal()
}

// recordMaxFor is the largest a single record may serialize to and still satisfy the B-tree split
// contract — RECORD_MAX = (C-12)/2 where C = capacity is the page payload (format.md "Why the
// record cap"). The spill planner reduces a record to ≤ this by externalizing values.
func recordMaxFor(capacity int) int {
	m := (capacity - pageHeader) / 2
	if m < 0 {
		m = 0
	}
	return m
}

// valueDisp is a value's planned on-disk disposition (large-values.md §2/§12/§13).
type valueDisp uint8

const (
	dispInline valueDisp = iota
	dispInlineComp
	dispExternal
	dispExternalComp
)

// recordPlan is a record's resolved disposition plan: per-column form, the LZ4 block a
// compressed form carries (so the serializer never re-compresses), the on-disk record size
// (the B-tree split weight), and the value_compress slabs the plan's pass-1 attempts cost.
type recordPlan struct {
	disp          []valueDisp
	comp          [][]byte
	size          int
	compressUnits int
}

// planDispositions decides each column's on-disk disposition (large-values.md §3/§12/§13;
// format.md "Large values"). Spill only when forced: if the all-inline-plain record already fits
// RECORD_MAX, nothing is compressed or spilled. Otherwise two passes, each visiting largest
// encoded size first, ties by ascending column index — deterministic, a §8 contract:
// (1) compress eligible values (payload ≥ sCompress), adopting iff the encoded compressed form is
// strictly smaller (store-smaller); (2) externalize values whose current encoded size still beats
// their pointer, moving the bytes pass 1 chose (compressed → a 0x04 chain of the compressed
// block) until the record fits. Shared by the serializer and recordSize (the B-tree split
// weight): in-memory node boundaries must match the serialized pages.
func planDispositions(colTypes []ScalarType, key []byte, row Row, capacity int) recordPlan {
	inline := make([]int, len(colTypes))
	size := 2 + len(key)
	for i, ty := range colTypes {
		inline[i] = len(encodeValue(ty, row[i]))
		size += inline[i]
	}
	plan := recordPlan{
		disp: make([]valueDisp, len(colTypes)),
		comp: make([][]byte, len(colTypes)),
	}
	cur := append([]int(nil), inline...)
	max := recordMaxFor(capacity)
	if size <= max {
		plan.size = size
		return plan
	}
	// Pass 1 — compress (lz4.md): spillable, non-NULL, payload ≥ sCompress; largest inline-plain
	// encoded size first, ties by ascending index. Every attempt is metered (ceil(raw/capacity)
	// value_compress slabs) whether or not store-smaller adopts it.
	cand := make([]int, 0, len(colTypes))
	for i, ty := range colTypes {
		if isSpillable(ty) && !row[i].IsNull() && len(valuePayload(ty, row[i])) >= sCompress {
			cand = append(cand, i)
		}
	}
	sort.SliceStable(cand, func(a, b int) bool { return inline[cand[a]] > inline[cand[b]] })
	for _, i := range cand {
		if size <= max {
			break
		}
		payload := valuePayload(colTypes[i], row[i])
		plan.compressUnits += (len(payload) + capacity - 1) / capacity
		comp := lz4Compress(payload)
		if inlineCompOverhead+len(comp) < inline[i] {
			size = size - cur[i] + inlineCompOverhead + len(comp)
			cur[i] = inlineCompOverhead + len(comp)
			plan.disp[i] = dispInlineComp
			plan.comp[i] = comp
		}
	}
	if size <= max {
		plan.size = size
		return plan
	}
	// Pass 2 — externalize: anything whose current encoded size beats its pointer, largest
	// current size first, ties by ascending index. (A NULL is 1 byte and never qualifies.)
	cand = cand[:0]
	for i, ty := range colTypes {
		ptr := externalPtrLen
		if plan.disp[i] == dispInlineComp {
			ptr = externalCompPtrLen
		}
		if isSpillable(ty) && cur[i] > ptr {
			cand = append(cand, i)
		}
	}
	sort.SliceStable(cand, func(a, b int) bool { return cur[cand[a]] > cur[cand[b]] })
	for _, i := range cand {
		if size <= max {
			break
		}
		ptr := externalPtrLen
		next := dispExternal
		if plan.disp[i] == dispInlineComp {
			ptr = externalCompPtrLen
			next = dispExternalComp
		}
		plan.disp[i] = next
		size = size - cur[i] + ptr
		cur[i] = ptr
	}
	plan.size = size
	return plan
}

// recordSize is the on-disk size of a record — the weight the page-backed B-tree splits on
// (format.md). Accounts for compression and out-of-line spill: a compressed value contributes its
// compressed inline form, an externalized one its fixed pointer size (large-values.md §12/§13).
// Must equal what the serializer produces, so in-memory node boundaries match serialized pages.
func recordSize(colTypes []ScalarType, key []byte, row Row, capacity int) int {
	return planDispositions(colTypes, key, row, capacity).size
}

// recordScanUnits returns the per-record units a scan's up-front cost block charges beyond the
// B-tree nodes (cost.md §3; large-values.md §8/§12/§14): for every column in the query's TOUCHED
// SET (mask), pages = one page_read per overflow chain page (the chain carries the payload for
// external-plain, the COMPRESSED block for external-compressed) and decompress = ceil(raw/capacity)
// value_decompress slabs per compressed stored value (inline- or external-). Zero/zero for a
// fully-inline-plain record or an untouched column.
func recordScanUnits(colTypes []ScalarType, key []byte, row Row, capacity int, mask []bool) (pages, decompress int) {
	plan := planDispositions(colTypes, key, row, capacity)
	for i, d := range plan.disp {
		if !mask[i] {
			continue // an untouched column's chain/slabs are never read (cost.md §3)
		}
		switch d {
		case dispExternal:
			n := len(valuePayload(colTypes[i], row[i]))
			pages += (n + capacity - 1) / capacity
		case dispInlineComp:
			n := len(valuePayload(colTypes[i], row[i]))
			decompress += (n + capacity - 1) / capacity
		case dispExternalComp:
			pages += (len(plan.comp[i]) + capacity - 1) / capacity
			n := len(valuePayload(colTypes[i], row[i]))
			decompress += (n + capacity - 1) / capacity
		case dispInline:
		}
	}
	return pages, decompress
}

// recordCompressUnits returns the value_compress slabs storing this record costs — one
// ceil(raw/capacity) block per pass-1 compression attempt, adopted or not (cost.md §3;
// large-values.md §13). Charged once per stored row version at the statement's write site,
// never for B-tree re-encodes.
func recordCompressUnits(colTypes []ScalarType, key []byte, row Row, capacity int) int {
	return planDispositions(colTypes, key, row, capacity).compressUnits
}

// valuePayload is a value's content payload P(v) — the bytes stored in the overflow chain when it is
// externalized (large-values.md §12): raw UTF-8 for text / raw bytes for bytea (both in v.Str), the
// decimal body (encoding minus its presence tag) for decimal. Only spillable types reach here.
func valuePayload(ty ScalarType, v Value) []byte {
	switch {
	case ty.IsText(), ty.IsBytea():
		return []byte(v.Str)
	case ty.IsDecimal():
		return encodeValue(ty, v)[1:] // strip the leading presence tag
	default:
		panic("only spillable values are externalized")
	}
}

// valueFromPayload reconstructs a value from the P(v) content gathered from its overflow chain
// (inverse of valuePayload) — large-values.md §12.
func valueFromPayload(ty ScalarType, payload []byte) (Value, error) {
	switch {
	case ty.IsText():
		return TextValue(string(payload)), nil
	case ty.IsBytea():
		return ByteaValue(payload), nil
	case ty.IsDecimal():
		pos := 0
		return decodeDecimalBody(payload, &pos)
	default:
		return Value{}, NewError(DataCorrupted, "a non-spillable type was stored external")
	}
}

// encodeRecord builds one record (key_len(u16) | key | payload), spilling over-large values out-of-
// line per the disposition plan (large-values.md §12). For each externalized value, allocate overflow
// page(s) via take, append them to *ovf, and write a tag|first_page|len pointer instead of the inline
// body. capacity is the page payload (the slab size + the spill-plan input). Shared by the whole-image
// (serializeNode) and incremental (serializeDirty) writers, which differ only in how take allocates.
func encodeRecord(table *Table, key []byte, row Row, capacity int, take func() uint32, ovf *[]overflowPageOut) []byte {
	colTypes := make([]ScalarType, len(table.Columns))
	for i, c := range table.Columns {
		colTypes[i] = c.Type
	}
	plan := planDispositions(colTypes, key, row, capacity)
	out := make([]byte, 0, 2+len(key)+len(row)*2)
	out = appendU16(out, uint16(len(key)))
	out = append(out, key...)
	for i, col := range table.Columns {
		switch plan.disp[i] {
		case dispExternal:
			payload := valuePayload(col.Type, row[i])
			first := writeOverflowChain(payload, capacity, take, ovf)
			out = append(out, tagExternal)
			out = appendU32(out, first)
			out = appendU32(out, uint32(len(payload)))
		case dispInlineComp:
			rawLen := len(valuePayload(col.Type, row[i]))
			comp := plan.comp[i]
			out = append(out, tagInlineComp)
			out = appendU32(out, uint32(rawLen))
			out = appendU16(out, uint16(len(comp)))
			out = append(out, comp...)
		case dispExternalComp:
			// The chain carries the COMPRESSED block (its page count follows comp size).
			rawLen := len(valuePayload(col.Type, row[i]))
			comp := plan.comp[i]
			first := writeOverflowChain(comp, capacity, take, ovf)
			out = append(out, tagExternalComp)
			out = appendU32(out, first)
			out = appendU32(out, uint32(len(comp)))
			out = appendU32(out, uint32(rawLen))
		default:
			out = append(out, encodeValue(col.Type, row[i])...)
		}
	}
	return out
}

// overflowPageOut is one overflow page produced while serializing a record's external value.
type overflowPageOut struct {
	index     uint32
	itemCount uint32
	nextPage  uint32
	payload   []byte
}

// writeOverflowChain writes payload across a chain of overflow pages (capacity-byte slabs, in order),
// allocating each page via take and linking it with nextPage (0 terminates). Returns the first page
// index for the record's pointer. payload is always non-empty (only values larger than the pointer
// spill — planDispositions).
func writeOverflowChain(payload []byte, capacity int, take func() uint32, ovf *[]overflowPageOut) uint32 {
	n := (len(payload) + capacity - 1) / capacity
	indices := make([]uint32, n)
	for i := range indices {
		indices[i] = take()
	}
	for j := 0; j < n; j++ {
		lo := j * capacity
		hi := lo + capacity
		if hi > len(payload) {
			hi = len(payload)
		}
		var next uint32
		if j+1 < n {
			next = indices[j+1]
		}
		*ovf = append(*ovf, overflowPageOut{index: indices[j], itemCount: uint32(hi - lo), nextPage: next, payload: payload[lo:hi]})
	}
	return indices[0]
}

// tableEntryBytes builds one table's catalog entry (format.md).
func tableEntryBytes(table *Table, rootDataPage uint32) []byte {
	var out []byte
	out = appendU16(out, uint16(len(table.Name)))
	out = append(out, table.Name...)
	out = appendU16(out, uint16(len(table.Columns)))
	for _, col := range table.Columns {
		out = appendU16(out, uint16(len(col.Name)))
		out = append(out, col.Name...)
		out = append(out, typeCodeForScalar(col.Type))
		var flags byte
		if col.PrimaryKey {
			flags |= 0b01
		}
		if col.NotNull {
			flags |= 0b10
		}
		if col.Default != nil {
			flags |= 0b100
		}
		out = append(out, flags)
		// A decimal column appends its typmod (precision, scale) — only for type_code 6, so
		// non-decimal entries are byte-unchanged (spec/fileformat/format.md). precision 0 =
		// unconstrained numeric.
		if col.Type.IsDecimal() {
			var precision, scale uint16
			if col.Decimal != nil {
				precision, scale = col.Decimal.Precision, col.Decimal.Scale
			}
			out = appendU16(out, precision)
			out = appendU16(out, scale)
		}
		// A column with a DEFAULT (flags bit2) appends its pre-evaluated default value via the
		// same value codec rows use — AFTER the typmod, presence-gated, so a column without a
		// default is byte-unchanged (spec/fileformat/format.md). A DEFAULT NULL is one 0x01.
		if col.Default != nil {
			out = append(out, encodeValue(col.Type, *col.Default)...)
		}
	}
	out = appendU32(out, rootDataPage)
	return out
}

// pack greedily packs item sizes into pages of capacity cap, returning groups of
// item indices. Empty input yields one empty group. A single item larger than cap
// is unsupported (no overflow pages in step-5b).
func pack(sizes []int, capacity int) ([][]int, error) {
	var groups [][]int
	var cur []int
	used := 0
	for i, sz := range sizes {
		if sz > capacity {
			return nil, NewError(FeatureNotSupported,
				"a record or table entry larger than a page is not supported")
		}
		if len(cur) > 0 && used+sz > capacity {
			groups = append(groups, cur)
			cur = nil
			used = 0
		}
		cur = append(cur, i)
		used += sz
	}
	groups = append(groups, cur)
	return groups, nil
}

// metaPage is one meta slot's full pageSize bytes (the 36-byte header + its CRC, zero-padded): its
// only content. ToImage copies it into both slots; an incremental commit pwrites it to the alternate
// slot (file.go). Single-sources the meta byte layout (spec/fileformat/format.md).
func metaPage(pageSize uint32, txid uint64, root, pageCount uint32) []byte {
	p := make([]byte, pageSize)
	copy(p[0:4], magic[:])
	binary.BigEndian.PutUint16(p[4:], formatVersion)
	binary.BigEndian.PutUint32(p[8:], pageSize)
	binary.BigEndian.PutUint64(p[12:], txid)
	binary.BigEndian.PutUint32(p[20:], root)
	binary.BigEndian.PutUint32(p[24:], pageCount)
	binary.BigEndian.PutUint32(p[32:], crc32IEEE(p[0:32]))
	return p
}

// makePage is a catalog/B-tree page's full pageSize bytes (header + payload, zero-padded). ToImage
// copies it into the image; an incremental commit pwrites it directly (file.go). Single-sources the
// page byte layout.
func makePage(ps int, pageType byte, itemCount, nextPage uint32, payload []byte) []byte {
	p := make([]byte, ps)
	p[0] = pageType
	binary.BigEndian.PutUint32(p[4:], itemCount)
	binary.BigEndian.PutUint32(p[8:], nextPage)
	copy(p[pageHeader:], payload)
	return p
}

// writeMeta writes a meta slot into image (the whole-image path; metaPage is the single source).
func writeMeta(image []byte, ps, slot int, pageSize uint32, txid uint64, root, pageCount uint32) {
	off := slot * ps
	copy(image[off:off+ps], metaPage(pageSize, txid, root, pageCount))
}

// writePage writes a catalog/data page into image (the whole-image path; makePage is the single source).
func writePage(image []byte, ps, index int, pageType byte, itemCount, nextPage uint32, payload []byte) {
	off := index * ps
	copy(image[off:off+ps], makePage(ps, pageType, itemCount, nextPage, payload))
}

// meta holds a validated meta slot's salient fields.
type meta struct {
	txid     uint64
	rootPage uint32
	// pageCount is the on-disk page high-water — the next free page an incremental commit appends at
	// (P6.1 part B).
	pageCount uint32
}

// parseMeta validates a standalone meta block; ok=false if it is not a valid meta. Shared by readMeta
// (whole image) and the demand-paged loader (which reads meta slots 0/1 as individual blocks).
func parseMeta(m []byte) (meta, bool) {
	if len(m) < 36 {
		return meta{}, false
	}
	if !bytes.Equal(m[0:4], magic[:]) {
		return meta{}, false
	}
	if binary.BigEndian.Uint16(m[4:6]) != formatVersion {
		return meta{}, false
	}
	if m[6] != 0 || m[7] != 0 || m[28] != 0 || m[29] != 0 || m[30] != 0 || m[31] != 0 {
		return meta{}, false
	}
	if crc32IEEE(m[0:32]) != binary.BigEndian.Uint32(m[32:36]) {
		return meta{}, false
	}
	return meta{
		txid:      binary.BigEndian.Uint64(m[12:20]),
		rootPage:  binary.BigEndian.Uint32(m[20:24]),
		pageCount: binary.BigEndian.Uint32(m[24:28]),
	}, true
}

// readMeta validates one meta slot of a whole image; ok=false if it is not a valid meta.
func readMeta(image []byte, ps, slot int) (meta, bool) {
	off := slot * ps
	if off+ps > len(image) {
		return meta{}, false
	}
	m := image[off : off+ps]
	if !bytes.Equal(m[0:4], magic[:]) {
		return meta{}, false
	}
	if binary.BigEndian.Uint16(m[4:6]) != formatVersion {
		return meta{}, false
	}
	if m[6] != 0 || m[7] != 0 || m[28] != 0 || m[29] != 0 || m[30] != 0 || m[31] != 0 {
		return meta{}, false
	}
	if crc32IEEE(m[0:32]) != binary.BigEndian.Uint32(m[32:36]) {
		return meta{}, false
	}
	return meta{
		txid:      binary.BigEndian.Uint64(m[12:20]),
		rootPage:  binary.BigEndian.Uint32(m[20:24]),
		pageCount: binary.BigEndian.Uint32(m[24:28]),
	}, true
}

// selectMeta picks the valid slot with the highest txid (tie → slot 0); the lone
// valid slot on a torn write; error if neither is valid (format.md).
func selectMeta(image []byte, ps int) (meta, error) {
	a, aok := readMeta(image, ps, 0)
	b, bok := readMeta(image, ps, 1)
	switch {
	case aok && bok:
		if b.txid > a.txid {
			return b, nil
		}
		return a, nil
	case aok:
		return a, nil
	case bok:
		return b, nil
	default:
		return meta{}, NewError(DataCorrupted, "no valid meta page")
	}
}

// page is a parsed page: header fields + a borrowed payload slice.
type page struct {
	pageType  byte
	itemCount uint32
	nextPage  uint32
	payload   []byte
}

// parsePage parses one standalone page block (header + payload). The single-block reader the
// demand-paged loader and fault path use (a page read through the pager is exactly one block);
// readPage slices it out of a whole image.
func parsePage(block []byte) (page, error) {
	if len(block) < pageHeader {
		return page{}, NewError(DataCorrupted, "page shorter than its header")
	}
	return page{
		pageType:  block[0],
		itemCount: binary.BigEndian.Uint32(block[4:8]),
		nextPage:  binary.BigEndian.Uint32(block[8:12]),
		payload:   block[pageHeader:],
	}, nil
}

func readPage(image []byte, ps int, index uint32) (page, error) {
	off := int(index) * ps
	if off+ps > len(image) {
		return page{}, NewError(DataCorrupted, "page index out of range")
	}
	return parsePage(image[off : off+ps])
}

// pageBlock returns one page's full block, copied out of a whole image — the overflow-chain fetch for
// the in-memory load path (readTree, large-values.md §12).
func pageBlock(image []byte, ps int, index uint32) ([]byte, error) {
	off := int(index) * ps
	if off+ps > len(image) {
		return nil, NewError(DataCorrupted, "page index out of range")
	}
	out := make([]byte, ps)
	copy(out, image[off:off+ps])
	return out, nil
}

// decodeLeafNode decodes a single leaf page block into a resident node, for the demand-paging fault
// path (spec/design/pager.md §4; paging.go faultLeaf). block is one page; page is its page id, stamped
// on the node so a later incremental commit keeps it clean. Weights are recomputed from the value
// codec (the exact size the writer used), so the loaded leaf is ready for further splits.
// fetch reads an overflow page block by index (to materialize external values whose chains live
// outside this leaf — large-values.md §12); the chain pages it visits are discarded here (the
// free-list is reconstructed at open, not on a runtime fault).
func decodeLeafNode(block []byte, pageID uint32, colTypes []ScalarType, fetch func(uint32) ([]byte, error)) (*pnode, error) {
	pg, err := parsePage(block)
	if err != nil {
		return nil, err
	}
	if pg.pageType != pageLeaf {
		return nil, NewError(DataCorrupted, "demand-paged a non-leaf page")
	}
	capacity := len(block) - pageHeader
	n := int(pg.itemCount)
	keys, vals, weights := make([][]byte, 0, n), make([]Row, 0, n), make([]uint32, 0, n)
	pos := 0
	for i := 0; i < n; i++ {
		key, row, _, err := decodeRecord(colTypes, pg.payload, &pos, fetch)
		if err != nil {
			return nil, err
		}
		weights = append(weights, uint32(recordSize(colTypes, key, row, capacity)))
		keys = append(keys, key)
		vals = append(vals, row)
	}
	return &pnode{keys: keys, vals: vals, weights: weights, page: pageID}, nil
}

func decodeTableEntry(buf []byte, pos *int) (*Table, uint32, error) {
	name, err := readString(buf, pos)
	if err != nil {
		return nil, 0, err
	}
	colCount, err := readU16(buf, pos)
	if err != nil {
		return nil, 0, err
	}
	columns := make([]Column, 0, colCount)
	for i := uint16(0); i < colCount; i++ {
		cname, err := readString(buf, pos)
		if err != nil {
			return nil, 0, err
		}
		tc, err := readU8(buf, pos)
		if err != nil {
			return nil, 0, err
		}
		ty, ok := scalarForTypeCode(tc)
		if !ok {
			return nil, 0, NewError(DataCorrupted, "unknown type code")
		}
		flags, err := readU8(buf, pos)
		if err != nil {
			return nil, 0, err
		}
		// A decimal column carries its typmod (precision, scale); precision 0 = unconstrained.
		var decimal *DecimalTypmod
		if ty.IsDecimal() {
			precision, err := readU16(buf, pos)
			if err != nil {
				return nil, 0, err
			}
			scale, err := readU16(buf, pos)
			if err != nil {
				return nil, 0, err
			}
			if precision != 0 {
				decimal = &DecimalTypmod{Precision: precision, Scale: scale}
			}
		}
		// The default value follows the typmod, present iff flags bit2 (same value codec as
		// rows). Absent → no bytes consumed (spec/fileformat/format.md).
		var defaultVal *Value
		if flags&0b100 != 0 {
			// A default is a small evaluated literal — never externalized — so no overflow reader
			// is needed (a 0x02 tag here would be a corrupt catalog).
			var sink []uint32
			dv, err := readValue(ty, buf, pos, nil, &sink)
			if err != nil {
				return nil, 0, err
			}
			defaultVal = &dv
		}
		columns = append(columns, Column{
			Name:       cname,
			Type:       ty,
			Decimal:    decimal,
			PrimaryKey: flags&0b01 != 0,
			NotNull:    flags&0b10 != 0,
			Default:    defaultVal,
		})
	}
	root, err := readU32(buf, pos)
	if err != nil {
		return nil, 0, err
	}
	return &Table{Name: name, Columns: columns}, root, nil
}

// decodeRecord decodes one record (key, row) and the overflow chain pages any external value
// followed (for the free-list reachability walk — large-values.md §12). fetch reads a page block by
// index, used to follow overflow chains; nil is only valid where no value can be external (a default).
func decodeRecord(colTypes []ScalarType, buf []byte, pos *int, fetch func(uint32) ([]byte, error)) ([]byte, Row, []uint32, error) {
	keyLen, err := readU16(buf, pos)
	if err != nil {
		return nil, nil, nil, err
	}
	keySlice, err := take(buf, pos, int(keyLen))
	if err != nil {
		return nil, nil, nil, err
	}
	key := make([]byte, len(keySlice))
	copy(key, keySlice)
	row := make(Row, len(colTypes))
	var ovf []uint32
	for i, ty := range colTypes {
		v, err := readValue(ty, buf, pos, fetch, &ovf)
		if err != nil {
			return nil, nil, nil, err
		}
		row[i] = v
	}
	return key, row, ovf, nil
}

// readValue reads one value via the value codec (inverse of encodeValue). The presence tag is read
// first: 0x00 an inline body, 0x01 NULL, 0x02 an external pointer (u32 first_page + u32 len) whose
// payload is gathered from the overflow chain via fetch and reconstructed by type (large-values.md
// §12). Pages visited while following a chain are appended to *ovfOut for the free-list walk.
func readValue(ty ScalarType, buf []byte, pos *int, fetch func(uint32) ([]byte, error), ovfOut *[]uint32) (Value, error) {
	tag, err := readU8(buf, pos)
	if err != nil {
		return Value{}, err
	}
	switch tag {
	case 0x00:
		return readInlineBody(ty, buf, pos)
	case 0x01:
		return NullValue(), nil
	case tagExternal:
		first, err := readU32(buf, pos)
		if err != nil {
			return Value{}, err
		}
		length, err := readU32(buf, pos)
		if err != nil {
			return Value{}, err
		}
		if fetch == nil {
			return Value{}, NewError(DataCorrupted, "external value with no overflow reader")
		}
		payload, err := readOverflowChain(first, int(length), fetch, ovfOut)
		if err != nil {
			return Value{}, err
		}
		return valueFromPayload(ty, payload)
	case tagInlineComp:
		rawLen, err := readU32(buf, pos)
		if err != nil {
			return Value{}, err
		}
		compLen, err := readU16(buf, pos)
		if err != nil {
			return Value{}, err
		}
		comp, err := take(buf, pos, int(compLen))
		if err != nil {
			return Value{}, err
		}
		payload, err := lz4Decompress(comp, int(rawLen))
		if err != nil {
			return Value{}, err
		}
		return valueFromPayload(ty, payload)
	case tagExternalComp:
		first, err := readU32(buf, pos)
		if err != nil {
			return Value{}, err
		}
		stored, err := readU32(buf, pos)
		if err != nil {
			return Value{}, err
		}
		rawLen, err := readU32(buf, pos)
		if err != nil {
			return Value{}, err
		}
		if fetch == nil {
			return Value{}, NewError(DataCorrupted, "external value with no overflow reader")
		}
		comp, err := readOverflowChain(first, int(stored), fetch, ovfOut)
		if err != nil {
			return Value{}, err
		}
		payload, err := lz4Decompress(comp, int(rawLen))
		if err != nil {
			return Value{}, err
		}
		return valueFromPayload(ty, payload)
	default:
		return Value{}, NewError(DataCorrupted, "invalid value presence tag")
	}
}

// readInlineBody reads the present-value body (after a 0x00 tag): a fixed-width integer, a u16 length
// + UTF-8 bytes for text, a single bool-byte, the decimal body, etc. (format.md *Value codec*).
func readInlineBody(ty ScalarType, buf []byte, pos *int) (Value, error) {
	switch {
	case ty.IsText():
		n, err := readU16(buf, pos)
		if err != nil {
			return Value{}, err
		}
		sb, err := take(buf, pos, int(n))
		if err != nil {
			return Value{}, err
		}
		return TextValue(string(sb)), nil
	case ty.IsBool():
		b, err := readU8(buf, pos)
		if err != nil {
			return Value{}, err
		}
		switch b {
		case 0x00:
			return BoolValue(false), nil
		case 0x01:
			return BoolValue(true), nil
		default:
			return Value{}, NewError(DataCorrupted, "invalid boolean value byte")
		}
	case ty.IsDecimal():
		return decodeDecimalBody(buf, pos)
	case ty.IsBytea():
		n, err := readU16(buf, pos)
		if err != nil {
			return Value{}, err
		}
		bb, err := take(buf, pos, int(n))
		if err != nil {
			return Value{}, err
		}
		// ByteaValue copies the bytes into a string, so the value owns its content.
		return ByteaValue(bb), nil
	case ty.IsUuid():
		// Fixed 16 raw bytes, no length prefix. Must branch before the integer path —
		// DecodeInt would sign-flip and WidthBytes is 16 there too.
		ub, err := take(buf, pos, 16)
		if err != nil {
			return Value{}, err
		}
		return UuidValue(ub), nil
	case ty.IsTimestamp() || ty.IsTimestamptz():
		vb, err := take(buf, pos, ty.WidthBytes())
		if err != nil {
			return Value{}, err
		}
		m := DecodeInt(ty, vb)
		if ty.IsTimestamp() {
			return TimestampValue(m), nil
		}
		return TimestamptzValue(m), nil
	default:
		vb, err := take(buf, pos, ty.WidthBytes())
		if err != nil {
			return Value{}, err
		}
		return IntValue(DecodeInt(ty, vb)), nil
	}
}

// decodeDecimalBody decodes a decimal value's body — flags (sign), u16 scale, u16 ndigits, then that
// many base-10^4 groups (format.md). Shared by the inline path and by external reconstruction (a
// spilled decimal's chain payload is exactly this body — large-values.md §12).
func decodeDecimalBody(buf []byte, pos *int) (Value, error) {
	flags, err := readU8(buf, pos)
	if err != nil {
		return Value{}, err
	}
	scale, err := readU16(buf, pos)
	if err != nil {
		return Value{}, err
	}
	ndigits, err := readU16(buf, pos)
	if err != nil {
		return Value{}, err
	}
	groups := make([]uint16, ndigits)
	for i := range groups {
		g, err := readU16(buf, pos)
		if err != nil {
			return Value{}, err
		}
		groups[i] = g
	}
	return DecimalValue(DecimalFromCodec(flags&1 != 0, uint32(scale), groups)), nil
}

// readOverflowChain gathers length bytes of an external value's payload by following its overflow
// chain from first (large-values.md §12): each page is page_type 4, carries itemCount payload bytes,
// and chains via nextPage (0 terminates). Every visited page is appended to *visited (the free-list
// reachability walk). fetch returns a page's full block by index.
func readOverflowChain(first uint32, length int, fetch func(uint32) ([]byte, error), visited *[]uint32) ([]byte, error) {
	out := make([]byte, 0, length)
	p := first
	for len(out) < length {
		if p == 0 {
			return nil, NewError(DataCorrupted, "overflow chain ended before the value length")
		}
		*visited = append(*visited, p)
		block, err := fetch(p)
		if err != nil {
			return nil, err
		}
		pg, err := parsePage(block)
		if err != nil {
			return nil, err
		}
		if pg.pageType != pageOverflow {
			return nil, NewError(DataCorrupted, "expected an overflow page")
		}
		n := int(pg.itemCount)
		if n == 0 || n > len(pg.payload) || len(out)+n > length {
			return nil, NewError(DataCorrupted, "overflow page slab out of range")
		}
		out = append(out, pg.payload[:n]...)
		p = pg.nextPage
	}
	return out, nil
}

// --- bounds-checked big-endian readers over a payload cursor ---

func take(buf []byte, pos *int, n int) ([]byte, error) {
	if *pos+n > len(buf) {
		return nil, NewError(DataCorrupted, "unexpected end of page data")
	}
	s := buf[*pos : *pos+n]
	*pos += n
	return s, nil
}

func readU8(buf []byte, pos *int) (byte, error) {
	s, err := take(buf, pos, 1)
	if err != nil {
		return 0, err
	}
	return s[0], nil
}

func readU16(buf []byte, pos *int) (uint16, error) {
	s, err := take(buf, pos, 2)
	if err != nil {
		return 0, err
	}
	return binary.BigEndian.Uint16(s), nil
}

func readU32(buf []byte, pos *int) (uint32, error) {
	s, err := take(buf, pos, 4)
	if err != nil {
		return 0, err
	}
	return binary.BigEndian.Uint32(s), nil
}

func readString(buf []byte, pos *int) (string, error) {
	n, err := readU16(buf, pos)
	if err != nil {
		return "", err
	}
	s, err := take(buf, pos, int(n))
	if err != nil {
		return "", err
	}
	if !utf8.Valid(s) {
		return "", NewError(DataCorrupted, "non-UTF-8 name")
	}
	return string(s), nil
}
