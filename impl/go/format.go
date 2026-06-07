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
	formatVersion uint16 = 2  // on-disk format version (2 = page-backed CoW B-tree, P6.1)
	pageHeader           = 12 // bytes of the catalog/B-tree page header
	pageCatalog   byte   = 1  // page_type for a catalog page
	pageLeaf      byte   = 2  // page_type for a B-tree leaf node
	pageInterior  byte   = 3  // page_type for a B-tree interior node
	rootPage      uint32 = 2  // catalog root of a fresh empty db (relocatable thereafter)
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
	if ps < pageHeader+36 {
		return nil, NewError(FeatureNotSupported, "page size too small for the format")
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

	// B-tree node pages.
	for _, bp := range body {
		writePage(image, ps, int(bp.index), bp.pageType, bp.itemCount, 0, bp.payload)
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

// bodyPage is one serialized B-tree node awaiting write: its assigned index, type, key count, payload.
type bodyPage struct {
	index     uint32
	pageType  byte
	itemCount uint32
	payload   []byte
}

// serializeNode serializes one node and its subtree post-order, appending each to *body, and returns
// this node's assigned page index and the next free index. A leaf's payload is its records; an
// interior's is its N+1 child pointers (big-endian u32) then its N records (format.md). A node whose
// payload would exceed the page is an oversized record (over RECORD_MAX) — feature_not_supported.
func serializeNode(n *pnode, table *Table, capacity int, nextIndex uint32, body *[]bodyPage) (uint32, uint32, error) {
	childPages := make([]uint32, len(n.children))
	for i, c := range n.children {
		cp, np, err := serializeNode(c, table, capacity, nextIndex, body)
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
	for i := range n.keys {
		payload = append(payload, encodeRecord(table, n.keys[i], n.vals[i])...)
	}
	if len(payload) > capacity {
		return 0, 0, NewError(FeatureNotSupported, "a record larger than the per-row limit is not supported")
	}
	*body = append(*body, bodyPage{index: index, pageType: pageType, itemCount: uint32(len(n.keys)), payload: payload})
	return index, nextIndex, nil
}

// LoadDatabase reconstructs a database from an on-disk image (inverse of ToImage).
// Returns a structured data_corrupted (XX001) error for malformed input.
func LoadDatabase(image []byte) (*Database, error) {
	if len(image) < 12 {
		return nil, NewError(DataCorrupted, "image smaller than a meta header")
	}
	pageSize := int(binary.BigEndian.Uint32(image[8:12]))
	if pageSize < pageHeader+36 || len(image) < pageSize*2 {
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
	catPage := mt.rootPage
	for catPage != 0 {
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
				root, length, err := readTree(image, pageSize, tableRoot, colTypes)
				if err != nil {
					return nil, err
				}
				store := snap.stores[strings.ToLower(name)]
				store.setTree(root, length)
				// No-PK keys are synthetic int64 rowids — advance the counter past the largest (the
				// last entry in key order) so future inserts don't collide.
				if !hasPK && length > 0 {
					keys, _ := store.rows.inorder()
					store.BumpRowidTo(DecodeInt(Int64, keys[len(keys)-1]) + 1)
				}
			}
		}
		catPage = pg.nextPage
	}
	db := NewDatabase()
	db.pageSize = uint32(pageSize)
	db.committed = snap
	return db, nil
}

// readTree reads a table's on-disk B-tree (rooted at pageIdx) into an in-memory tree, returning the
// root node and the total row count (spec/fileformat/format.md). An interior node's payload is its
// N+1 child pointers then its N records; we recurse the pointers, then read the separators. Weights
// are recomputed from the value codec (the exact size the writer used), so the loaded tree is ready
// for further size-driven splits.
func readTree(image []byte, ps int, pageIdx uint32, colTypes []ScalarType) (*pnode, int, error) {
	pg, err := readPage(image, ps, pageIdx)
	if err != nil {
		return nil, 0, err
	}
	switch pg.pageType {
	case pageLeaf:
		n := int(pg.itemCount)
		keys, vals, weights := make([][]byte, 0, n), make([]Row, 0, n), make([]uint32, 0, n)
		pos := 0
		for i := 0; i < n; i++ {
			key, row, err := decodeRecord(colTypes, pg.payload, &pos)
			if err != nil {
				return nil, 0, err
			}
			weights = append(weights, uint32(recordSize(colTypes, key, row)))
			keys = append(keys, key)
			vals = append(vals, row)
		}
		return &pnode{keys: keys, vals: vals, weights: weights, page: pageIdx}, n, nil
	case pageInterior:
		n := int(pg.itemCount)
		pos := 0
		children := make([]*pnode, 0, n+1)
		total := 0
		for i := 0; i < n+1; i++ {
			cp, err := readU32(pg.payload, &pos)
			if err != nil {
				return nil, 0, err
			}
			child, clen, err := readTree(image, ps, cp, colTypes)
			if err != nil {
				return nil, 0, err
			}
			children = append(children, child)
			total += clen
		}
		keys, vals, weights := make([][]byte, 0, n), make([]Row, 0, n), make([]uint32, 0, n)
		for i := 0; i < n; i++ {
			key, row, err := decodeRecord(colTypes, pg.payload, &pos)
			if err != nil {
				return nil, 0, err
			}
			weights = append(weights, uint32(recordSize(colTypes, key, row)))
			keys = append(keys, key)
			vals = append(vals, row)
		}
		total += n
		return &pnode{keys: keys, vals: vals, weights: weights, children: children, page: pageIdx}, total, nil
	default:
		return nil, 0, NewError(DataCorrupted, "expected a B-tree node page")
	}
}

// recordSize is the on-disk size of a record (key_len(u16) | key | each column value) — the weight
// the page-backed B-tree splits on (format.md). It must equal len(encodeRecord ...), so in-memory
// node boundaries match serialized page boundaries; computed from the value codec to stay in lockstep.
func recordSize(colTypes []ScalarType, key []byte, row Row) int {
	n := 2 + len(key)
	for i, ty := range colTypes {
		n += len(encodeValue(ty, row[i]))
	}
	return n
}

// encodeRecord builds one record: key_len(u16) | key | payload(each column value).
func encodeRecord(table *Table, key []byte, row Row) []byte {
	out := make([]byte, 0, 2+len(key)+len(row)*2)
	out = appendU16(out, uint16(len(key)))
	out = append(out, key...)
	for i, col := range table.Columns {
		out = append(out, encodeValue(col.Type, row[i])...)
	}
	return out
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

// writeMeta writes a meta slot's bytes (and its CRC) into image.
func writeMeta(image []byte, ps, slot int, pageSize uint32, txid uint64, root, pageCount uint32) {
	off := slot * ps
	copy(image[off:off+4], magic[:])
	binary.BigEndian.PutUint16(image[off+4:], formatVersion)
	binary.BigEndian.PutUint32(image[off+8:], pageSize)
	binary.BigEndian.PutUint64(image[off+12:], txid)
	binary.BigEndian.PutUint32(image[off+20:], root)
	binary.BigEndian.PutUint32(image[off+24:], pageCount)
	crc := crc32IEEE(image[off : off+32])
	binary.BigEndian.PutUint32(image[off+32:], crc)
}

// writePage writes a catalog/data page's header and payload into image.
func writePage(image []byte, ps, index int, pageType byte, itemCount, nextPage uint32, payload []byte) {
	off := index * ps
	image[off] = pageType
	binary.BigEndian.PutUint32(image[off+4:], itemCount)
	binary.BigEndian.PutUint32(image[off+8:], nextPage)
	copy(image[off+pageHeader:], payload)
}

// meta holds a validated meta slot's salient fields.
type meta struct {
	txid     uint64
	rootPage uint32
}

// readMeta validates one meta slot; ok=false if it is not a valid meta.
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
		txid:     binary.BigEndian.Uint64(m[12:20]),
		rootPage: binary.BigEndian.Uint32(m[20:24]),
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

func readPage(image []byte, ps int, index uint32) (page, error) {
	off := int(index) * ps
	if off+ps > len(image) {
		return page{}, NewError(DataCorrupted, "page index out of range")
	}
	p := image[off : off+ps]
	return page{
		pageType:  p[0],
		itemCount: binary.BigEndian.Uint32(p[4:8]),
		nextPage:  binary.BigEndian.Uint32(p[8:12]),
		payload:   p[pageHeader:],
	}, nil
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
			dv, err := readValue(ty, buf, pos)
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

func decodeRecord(colTypes []ScalarType, buf []byte, pos *int) ([]byte, Row, error) {
	keyLen, err := readU16(buf, pos)
	if err != nil {
		return nil, nil, err
	}
	keySlice, err := take(buf, pos, int(keyLen))
	if err != nil {
		return nil, nil, err
	}
	key := make([]byte, len(keySlice))
	copy(key, keySlice)
	row := make(Row, len(colTypes))
	for i, ty := range colTypes {
		v, err := readValue(ty, buf, pos)
		if err != nil {
			return nil, nil, err
		}
		row[i] = v
	}
	return key, row, nil
}

// readValue reads one value via the value codec (inverse of encodeValue).
func readValue(ty ScalarType, buf []byte, pos *int) (Value, error) {
	tag, err := readU8(buf, pos)
	if err != nil {
		return Value{}, err
	}
	switch tag {
	case 0x00:
		if ty.IsText() {
			n, err := readU16(buf, pos)
			if err != nil {
				return Value{}, err
			}
			sb, err := take(buf, pos, int(n))
			if err != nil {
				return Value{}, err
			}
			return TextValue(string(sb)), nil
		}
		if ty.IsBool() {
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
		}
		if ty.IsDecimal() {
			// flags (sign), u16 scale, u16 ndigits, then that many base-10^4 groups.
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
		if ty.IsBytea() {
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
		}
		if ty.IsUuid() {
			// Fixed 16 raw bytes, no length prefix. Must branch before the integer path —
			// DecodeInt would sign-flip and WidthBytes is 16 there too.
			ub, err := take(buf, pos, 16)
			if err != nil {
				return Value{}, err
			}
			return UuidValue(ub), nil
		}
		if ty.IsTimestamp() || ty.IsTimestamptz() {
			vb, err := take(buf, pos, ty.WidthBytes())
			if err != nil {
				return Value{}, err
			}
			m := DecodeInt(ty, vb)
			if ty.IsTimestamp() {
				return TimestampValue(m), nil
			}
			return TimestamptzValue(m), nil
		}
		vb, err := take(buf, pos, ty.WidthBytes())
		if err != nil {
			return Value{}, err
		}
		return IntValue(DecodeInt(ty, vb)), nil
	case 0x01:
		return NullValue(), nil
	default:
		return Value{}, NewError(DataCorrupted, "invalid value presence tag")
	}
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
