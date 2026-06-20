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
	"math"
	"slices"
	"sort"
	"strings"
	"unicode/utf8"
)

// magic — ASCII "JEDB" (the engine is named `jed`).
var magic = [4]byte{'J', 'E', 'D', 'B'}

const (
	formatVersion   uint16 = 17    // on-disk format version (17 = baked collations: a catalog entry_kind 3 collation snapshot — a flags byte is_default/reference + the LZ4-compressed .coll artifact — emitted after sequences and before tables, plus a per-column collation: column flags byte bit6 has_collation + a trailing name, spec/design/collation.md §5. 16 = range columns: type_code 17 + an inline element-type descriptor in the catalog — one scalar code, spec/design/ranges.md §3 — and the compact range value body, a flags byte EMPTY/LB_INF/UB_INF/LB_INC/UB_INC + present bound bodies, §4). 15 = IDENTITY columns: the column-entry flags byte gains bit4 is_identity + bit5 identity_always; an identity column desugars like serial plus those two bits, spec/design/sequences.md §13. 14 = the serial owned-sequence link: the sequence-entry flags byte gains a has_owner bit + a trailing owner table-name/column-ordinal, spec/design/sequences.md §12. 13 = GIN inverted indexes: each catalog index entry gains a one-byte index_kind (0 = ordered B-tree, 1 = GIN) between index_flags and index_root_page, spec/design/gin.md. 12 = sequences: an entry_kind = 2 catalog entry — name + six i64 fields + a flags byte — emitted after composite-type entries and before table entries, spec/design/sequences.md §3, plus the date scalar. 11 = FOREIGN KEY constraints: a per-table catalog foreign-key list after the index list, spec/design/constraints.md §6. 10 = array (T[]) columns: type_code 15 + an element-type descriptor in the catalog, spec/design/array.md §3, and the compact array value body, §4. 9 = composite (row) types; 8 = per-column expression-default flag; 7 = per-page crc32. Each bump is atomic across Rust/Go/TS + the Ruby golden reference (every .jed golden's version byte + CRC changed together).
	pageHeader             = 16    // bytes of the catalog/B-tree/overflow page header (v7: 12-byte v6 header + a 4-byte per-page crc32 at offset 12)
	interiorReserve        = 12    // bytes reserved inside RECORD_MAX for a two-key interior node's 3 child pointers (4·3) — independent of pageHeader (format.md "Why the record cap")
	pageCatalog     byte   = 1     // page_type for a catalog page
	pageLeaf        byte   = 2     // page_type for a B-tree leaf node
	pageInterior    byte   = 3     // page_type for a B-tree interior node
	pageOverflow    byte   = 4     // page_type for an out-of-line value slab (large-values.md §12)
	rootPage        uint32 = 2     // catalog root of a fresh empty db (relocatable thereafter)
	minPageSize            = 256   // smallest valid page size; chosen floor above the structural min pageHeader+36=52 (format.md *Page model*)
	maxPageSize            = 65536 // largest valid page size, 64 KiB (format.md *Page model*; CLAUDE.md §13)

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
	case IntervalType:
		return 11
	case Float64:
		return 12
	case Float32:
		return 13
	case Date:
		return 16
	default:
		return 0
	}
}

// pushArrayElementType appends an array column's element type descriptor (spec/design/array.md §3):
// the element's type code, then (for a composite element) its name. v1 element types are scalars;
// a composite element is handled for forward-compat, a nested array element is rejected
// (multidimensionality is a value property, not array-of-array — §2).
func pushArrayElementType(out []byte, elem Type) []byte {
	if elem.Array != nil {
		panic("nested array element (array-of-array) is not a jed type — array.md §2")
	}
	if elem.Range != nil {
		panic("array-of-range is not a storable type (ranges.md §2)")
	}
	if elem.Comp != nil {
		out = append(out, 14)
		out = appendU16(out, uint16(len(elem.Comp.Name)))
		return append(out, elem.Comp.Name...)
	}
	return append(out, typeCodeForScalar(elem.Scalar))
}

// readArrayElementType decodes an array column's element type descriptor (inverse of
// pushArrayElementType).
func readArrayElementType(buf []byte, pos *int) (Type, error) {
	code, err := readU8(buf, pos)
	if err != nil {
		return Type{}, err
	}
	if code == 14 {
		name, err := readString(buf, pos)
		if err != nil {
			return Type{}, err
		}
		return CompositeT(name), nil
	}
	s, ok := scalarForTypeCode(code)
	if !ok {
		return Type{}, NewError(DataCorrupted, "invalid array element code")
	}
	return ScalarT(s), nil
}

// pushRangeElementType appends a range column's element type descriptor (spec/design/ranges.md §3): a
// single u8 scalar type code. A range element is always one of the six scalar subtypes (i32/i64/
// decimal/timestamp/timestamptz/date) — never composite, array, or nested range — and numrange's
// element is the unconstrained decimal, so no typmod is stored (the type name fully determines the
// element). The element descriptor is self-describing: it identifies which of the six ranges the
// column is.
func pushRangeElementType(out []byte, elem Type) []byte {
	if elem.Comp != nil || elem.Array != nil || elem.Range != nil {
		panic("a range element is always a scalar subtype (ranges.md §2)")
	}
	return append(out, typeCodeForScalar(elem.Scalar))
}

// readRangeElementType decodes a range column's element type descriptor (inverse of
// pushRangeElementType): one scalar code, validated to be one of the six range element subtypes
// (else XX001).
func readRangeElementType(buf []byte, pos *int) (Type, error) {
	code, err := readU8(buf, pos)
	if err != nil {
		return Type{}, err
	}
	s, ok := scalarForTypeCode(code)
	if !ok {
		return Type{}, NewError(DataCorrupted, "invalid range element code")
	}
	if _, ok := rangeForElement(s); !ok {
		return Type{}, NewError(DataCorrupted, "type code is not a valid range element subtype")
	}
	return ScalarT(s), nil
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
	case 11:
		return IntervalType, true
	case 12:
		return Float64, true
	case 13:
		return Float32, true
	case 16:
		return Date, true
	default:
		return 0, false
	}
}

// crc32Update folds data into a running CRC-32/IEEE register (reflected, poly 0xEDB88320)
// WITHOUT the final XOR, so it composes: crc32Update(crc32Update(0xFFFFFFFF, a), b) over a
// split buffer equals folding a‖b. Both crc32IEEE and the split pageCRC build on it.
func crc32Update(crc uint32, data []byte) uint32 {
	for _, b := range data {
		crc ^= uint32(b)
		for i := 0; i < 8; i++ {
			mask := -(crc & 1) // 0xFFFFFFFF if low bit set, else 0
			crc = (crc >> 1) ^ (0xEDB88320 & mask)
		}
	}
	return crc
}

// crc32IEEE is CRC-32/IEEE (reflected, poly 0xEDB88320, init/final 0xFFFFFFFF) — the
// standard zlib CRC32, hand-rolled so no dependency is needed. Pinned by the vector
// crc32("123456789") == 0xCBF43926.
func crc32IEEE(data []byte) uint32 {
	return ^crc32Update(0xFFFFFFFF, data)
}

// pageCRC is the per-page checksum (v7, format.md *Page header*): CRC-32/IEEE over a body page's
// bytes EXCLUDING its own 4-byte crc32 field at [12,16) — i.e. [0,12) then [16,pageSize), covering
// the header, payload, and zero-fill tail. makePage writes it; parsePage re-verifies it (mismatch →
// XX001). page is one full page (pageSize bytes).
func pageCRC(page []byte) uint32 {
	return ^crc32Update(crc32Update(0xFFFFFFFF, page[0:12]), page[pageHeader:])
}

// encodeValue is the value codec (format.md): a 1-byte presence tag (0x01 = NULL), then the type's
// present-value body. A scalar dispatches to encodeScalar; a COMPOSITE (spec/design/composite.md §4)
// is the shared presence tag then a body of `null-bitmap ‖ each present field's value-codec body`
// (no per-field tag — the bitmap carries presence), recursing for nested composites.
func encodeValue(ty ColType, v Value) []byte {
	if ty.Elem != nil {
		// An array column (spec/design/array.md §4): the shared presence tag then the array body.
		if v.Kind == ValNull {
			return []byte{0x01}
		}
		if v.Kind != ValArray {
			panic("BUG: a non-array value in an array column")
		}
		out := []byte{0x00} // present
		return append(out, encodeArrayBody(*ty.Elem, v.Array)...)
	}
	if ty.RangeElem != nil {
		// A range column (spec/design/ranges.md §4): the shared presence tag then the range body.
		if v.Kind == ValNull {
			return []byte{0x01}
		}
		if v.Kind != ValRange {
			panic("BUG: a non-range value in a range column")
		}
		out := []byte{0x00} // present
		return append(out, encodeRangeBody(*ty.RangeElem, v.Range)...)
	}
	if !ty.Composite {
		return encodeScalar(ty.Scalar, v)
	}
	if v.Kind == ValNull {
		return []byte{0x01}
	}
	if v.Kind != ValComposite {
		panic("BUG: a non-composite value in a composite column")
	}
	out := []byte{0x00} // present
	return append(out, encodeCompositeBody(ty.Fields, *v.Comp)...)
}

// encodeArrayBody is an array value's body (after the 0x00 present tag, spec/design/array.md §4):
// ndim u8, flags u8, per-dim (len u32 BE, lb i32 BE), then the optional null bitmap (present iff
// HAS_NULLS) and the present element bodies (row-major). An empty array is ndim 0; otherwise ndim is
// the dimension count and each dimension records its length and lower bound (multidim + custom lower
// bounds — spec/design/array.md §12). The bitmap (MSB-first, like composite) is present iff any
// element is NULL; a NULL element contributes zero body bytes.
func encodeArrayBody(elem ColType, a *ArrayVal) []byte {
	if len(a.Elements) == 0 {
		return []byte{0, 0} // ndim 0, flags 0 (empty array)
	}
	hasNulls := false
	for _, e := range a.Elements {
		if e.Kind == ValNull {
			hasNulls = true
			break
		}
	}
	out := make([]byte, 0, 2+8*a.Ndim()+4*len(a.Elements))
	out = append(out, byte(a.Ndim()))
	if hasNulls {
		out = append(out, 0x01) // flags: bit 0 = HAS_NULLS
	} else {
		out = append(out, 0x00)
	}
	for d := 0; d < a.Ndim(); d++ {
		out = appendU32(out, uint32(a.Dims[d]))    // dim length
		out = appendU32(out, uint32(a.Lbounds[d])) // lower bound (i32 BE)
	}
	if hasNulls {
		nbytes := (len(a.Elements) + 7) / 8
		bitmap := make([]byte, nbytes)
		for i, e := range a.Elements {
			if e.Kind == ValNull {
				bitmap[i/8] |= 0x80 >> uint(i%8)
			}
		}
		out = append(out, bitmap...)
	}
	for _, e := range a.Elements {
		if e.Kind != ValNull {
			out = append(out, encodeValue(elem, e)[1:]...) // body only (no presence tag)
		}
	}
	return out
}

// encodeRangeBody is a range value's body (after the 0x00 present tag, spec/design/ranges.md §4): a
// single flags u8 then the present bound bodies. The flags bits are EMPTY (0), LB_INF (1), UB_INF (2),
// LB_INC (3), UB_INC (4); bits 5-7 are reserved 0. An empty range is the lone flags byte 0x01 (no
// bounds follow). Otherwise a finite lower bound (!LB_INF) then a finite upper bound (!UB_INF) each
// contribute the element's value-codec body MINUS the presence tag (the same tag-byte+body split
// array/composite use). The stored value is canonical (§4) — canonicalization happens at parse/cast,
// not here.
func encodeRangeBody(elem ColType, rv *RangeVal) []byte {
	if rv.Empty {
		return []byte{0x01} // RANGE_EMPTY
	}
	var flags byte
	if rv.Lower == nil {
		flags |= 0x02 // LB_INF
	}
	if rv.Upper == nil {
		flags |= 0x04 // UB_INF
	}
	if rv.LowerInc {
		flags |= 0x08 // LB_INC
	}
	if rv.UpperInc {
		flags |= 0x10 // UB_INC
	}
	out := []byte{flags}
	if rv.Lower != nil {
		out = append(out, encodeValue(elem, *rv.Lower)[1:]...) // body only (no presence tag)
	}
	if rv.Upper != nil {
		out = append(out, encodeValue(elem, *rv.Upper)[1:]...)
	}
	return out
}

// encodeCompositeBody is a composite value's body (after the 0x00 present tag,
// spec/design/composite.md §4): a null bitmap of ceil(field_count/8) bytes (MSB-first — field i is
// bit 0x80 >> (i%8) of byte i/8; a set bit = NULL) followed by each PRESENT field's value-codec body
// in declaration order. A NULL field contributes zero body bytes; a present field's body is its
// encodeValue minus the leading presence tag (a nested composite recurses).
func encodeCompositeBody(fields []ColField, vals []Value) []byte {
	nbytes := (len(fields) + 7) / 8
	bitmap := make([]byte, nbytes)
	var bodies []byte
	for i := range fields {
		if vals[i].Kind == ValNull {
			bitmap[i/8] |= 0x80 >> uint(i%8)
		} else {
			bodies = append(bodies, encodeValue(fields[i].Type, vals[i])[1:]...)
		}
	}
	return append(bitmap, bodies...)
}

// encodeScalar is the scalar value codec (the body of encodeValue for a scalar ColType — format.md):
// a 1-byte presence tag (0x01 = NULL), then the type's present-value body. Integers reuse the
// order-preserving key encoding; text is where the seam diverges — a stored text value needs no
// ordering, so it is a compact u16 byte-length + UTF-8 bytes (collation C, verbatim). A text value
// whose UTF-8 length exceeds uint16's max is unsupported; in practice it also exceeds a page and is
// caught by the oversized-item rule in pack (0A000), so the cast here is sound for every supported
// page size (spec/fileformat/format.md). boolean is a single bool-byte body — 0x00 false, 0x01 true
// (types.md §9).
func encodeScalar(ty ScalarType, v Value) []byte {
	switch v.Kind {
	case ValNull:
		return EncodeNullable(ty, nil)
	case ValUnfetched:
		// An unfetched reference is resolved before any encode/plan (the scan layer for reads,
		// the mutation path for stores, resolveForEncode at commit — large-values.md §14).
		panic("BUG: encoding an unfetched large value")
	case ValComposite:
		// A composite value is encoded by encodeValue's composite arm, never here.
		panic("BUG: a composite value reached the scalar codec")
	case ValArray:
		// An array value is encoded by encodeValue's array arm, never here.
		panic("BUG: an array value reached the scalar codec")
	case ValRange:
		// A range value is encoded by encodeValue's range arm, never here.
		panic("BUG: a range value reached the scalar codec")
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
	case ValInterval:
		// Fixed 16-byte body: i32 months, i32 days, i64 micros — big-endian two's-complement,
		// no sign-flip (a value codec, not an order-preserving key) — spec/fileformat/format.md.
		out := make([]byte, 0, 1+16)
		out = append(out, 0x00) // present
		out = appendU32(out, uint32(v.Iv.Months))
		out = appendU32(out, uint32(v.Iv.Days))
		m := uint64(v.Iv.Micros)
		out = append(out, byte(m>>56), byte(m>>48), byte(m>>40), byte(m>>32),
			byte(m>>24), byte(m>>16), byte(m>>8), byte(m))
		return out
	case ValFloat64:
		// Fixed 8-byte body, NO length prefix: the IEEE bits big-endian (format.md code 12). VERBATIM
		// for every value EXCEPT NaN: a -0.0 keeps its sign bit and ±Inf/finite keep theirs, but a
		// NaN is canonicalized to the single quiet pattern 0x7FF8000000000000. A NaN's payload is
		// core-specific (math.NaN() is …001, hardware Inf-Inf is the negative 0xFFF8…), so the codec
		// re-canonicalizes it to keep a stored NaN cross-core byte-identical (spec/design/float.md §10,
		// determinism.md §4). The -0→+0 collapse is a comparison/key concern only, NOT applied here.
		bits := uint64(v.Int)
		if math.IsNaN(math.Float64frombits(bits)) {
			bits = 0x7FF8000000000000
		}
		out := make([]byte, 0, 1+8)
		out = append(out, 0x00) // present
		return append(out, byte(bits>>56), byte(bits>>48), byte(bits>>40), byte(bits>>32),
			byte(bits>>24), byte(bits>>16), byte(bits>>8), byte(bits))
	case ValFloat32:
		// Fixed 4-byte body, NO length prefix: the IEEE bits big-endian (format.md code 13). VERBATIM
		// except a NaN is canonicalized to the single quiet pattern 0x7FC00000 (see ValFloat64).
		bits := uint32(v.Int)
		if math.IsNaN(float64(math.Float32frombits(bits))) {
			bits = 0x7FC00000
		}
		out := make([]byte, 0, 1+4)
		out = append(out, 0x00) // present
		return appendU32(out, bits)
	default:
		n := v.Int
		return EncodeNullable(ty, &n)
	}
}

func appendU16(b []byte, v uint16) []byte { return append(b, byte(v>>8), byte(v)) }
func appendU32(b []byte, v uint32) []byte {
	return append(b, byte(v>>24), byte(v>>16), byte(v>>8), byte(v))
}

// appendI64 appends v as 8 big-endian two's-complement bytes (the sequence-entry field encoding).
func appendI64(b []byte, v int64) []byte {
	var s [8]byte
	binary.BigEndian.PutUint64(s[:], uint64(v))
	return append(b, s[:]...)
}

// ToImage serializes the whole committed state to one on-disk image (format.md). A thin wrapper
// over Snapshot.ToImage for the committed snapshot — txid is written into both meta slots. (The
// writer's working snapshot is serialized directly via Snapshot.ToImage at commit; this serves
// callers/tests holding a *Database.)
func (db *Database) ToImage(pageSize uint32, txid uint64) ([]byte, error) {
	return db.committed.ToImage(pageSize, txid)
}

// pageSizeValid reports whether ps is a legal page size: a power of two within
// [minPageSize, maxPageSize] (format.md *Page model* — the nine values {256, 512, … 65536}).
// Power-of-two keeps every page boundary sector-aligned (the SSD target, CLAUDE.md §9) and shrinks
// the legal set; the ps != 0 guard also keeps the pager's pageSize divisor non-zero.
func pageSizeValid(ps int) bool {
	return ps != 0 && ps&(ps-1) == 0 && ps >= minPageSize && ps <= maxPageSize
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
	if ps&(ps-1) != 0 {
		return nil, NewError(FeatureNotSupported, "page size must be a power of two")
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
	indexRoots := make([][]uint32, len(keys))
	// Index trees have no value columns — encode against an empty colTypes.
	var indexColTypes []ColType
	nextIndex := rootPage
	for ti, k := range keys {
		if root := s.stores[k].treeRoot(); root != nil {
			rp, np, err := serializeNode(root, s.stores[k].colTypes, capacity, nextIndex, &body)
			if err != nil {
				return nil, err
			}
			rootDataPage[ti] = rp
			nextIndex = np
		}
		// The table's index trees follow its data tree, in catalog (name) order
		// (spec/fileformat/format.md "From-scratch image").
		for _, idx := range s.tables[k].Indexes {
			var r uint32
			if root := s.indexStores[strings.ToLower(idx.Name)].treeRoot(); root != nil {
				rp, np, err := serializeNode(root, indexColTypes, capacity, nextIndex, &body)
				if err != nil {
					return nil, err
				}
				r = rp
				nextIndex = np
			}
			indexRoots[ti] = append(indexRoots[ti], r)
		}
	}

	// The catalog chain follows the data; its head is the relocatable root_page. Each entry is
	// kind-tagged (v9): composite-type entries (kind 1) first in lowercased-name order, then table
	// entries (kind 0) — spec/fileformat/format.md.
	catRoot := nextIndex
	var catEntries [][]byte
	for _, ct := range s.compositeTypesSorted() {
		catEntries = append(catEntries, append([]byte{1}, compositeTypeEntryBytes(ct)...))
	}
	for _, sq := range s.sequencesSorted() {
		catEntries = append(catEntries, append([]byte{2}, sequenceEntryBytes(sq)...))
	}
	// Collation snapshots (kind 3, v17) — after sequences, before tables (spec/design/collation.md §5).
	for _, c := range s.collationsSorted() {
		catEntries = append(catEntries, append([]byte{3}, collationEntryBytes(c, s.defaultCollation == c.Name)...))
	}
	for ti, k := range keys {
		catEntries = append(catEntries, append([]byte{0}, tableEntryBytes(s.tables[k], rootDataPage[ti], indexRoots[ti])...))
	}
	entrySizes := make([]int, len(catEntries))
	for i, e := range catEntries {
		entrySizes[i] = len(e)
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
		for _, ei := range group {
			payload = append(payload, catEntries[ei]...)
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
func serializeNode(n *pnode, colTypes []ColType, capacity int, nextIndex uint32, body *[]bodyPage) (uint32, uint32, error) {
	childPages := make([]uint32, len(n.children))
	for i, c := range n.children {
		// Whole-image serialize renumbers pages from scratch and runs only on a fully-resident
		// in-memory database (create's empty image, the golden generator) — a paged file commits
		// incrementally via serializeDirty. An OnDisk child would carry a page id from a different
		// layout, so it must not appear here.
		if c.node == nil {
			panic("whole-image serialize hit an OnDisk leaf")
		}
		cp, np, err := serializeNode(c.node, colTypes, capacity, nextIndex, body)
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
		payload = append(payload, encodeRecord(colTypes, n.keys[i], n.vals[i], capacity, take, &ovf)...)
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
func (s *Snapshot) incrementalImage(pageSize, startPage uint32, free []uint32, paging *sharedPaging) (incrementalWrite, error) {
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
	indexRoots := make([][]uint32, len(keys))
	var indexColTypes []ColType
	for ti, k := range keys {
		if root := s.stores[k].treeRoot(); root != nil {
			rp, err := serializeDirty(root, s.stores[k].colTypes, capacity, ps, alloc, &pages, paging)
			if err != nil {
				return incrementalWrite{}, err
			}
			rootDataPage[ti] = rp
		}
		// The table's index trees follow its data tree, in catalog (name) order — only
		// their dirty nodes are written, like any tree (spec/fileformat/format.md
		// "Allocation & incremental commit").
		for _, idx := range s.tables[k].Indexes {
			var r uint32
			if root := s.indexStores[strings.ToLower(idx.Name)].treeRoot(); root != nil {
				rp, err := serializeDirty(root, indexColTypes, capacity, ps, alloc, &pages, paging)
				if err != nil {
					return incrementalWrite{}, err
				}
				r = rp
			}
			indexRoots[ti] = append(indexRoots[ti], r)
		}
	}

	// The catalog chain is rewritten to fresh pages every commit (table roots move). Allocate its
	// page indices up front — they may be reused free pages, hence not contiguous — so each page can
	// point at the next (pack always returns ≥ 1 group, so catPages is non-empty). Entries are
	// kind-tagged (v9): composite-type entries (kind 1, name order) then table entries (kind 0) —
	// spec/fileformat/format.md.
	var catEntries [][]byte
	for _, ct := range s.compositeTypesSorted() {
		catEntries = append(catEntries, append([]byte{1}, compositeTypeEntryBytes(ct)...))
	}
	for _, sq := range s.sequencesSorted() {
		catEntries = append(catEntries, append([]byte{2}, sequenceEntryBytes(sq)...))
	}
	// Collation snapshots (kind 3, v17) — after sequences, before tables (spec/design/collation.md §5).
	for _, c := range s.collationsSorted() {
		catEntries = append(catEntries, append([]byte{3}, collationEntryBytes(c, s.defaultCollation == c.Name)...))
	}
	for ti, k := range keys {
		catEntries = append(catEntries, append([]byte{0}, tableEntryBytes(s.tables[k], rootDataPage[ti], indexRoots[ti])...))
	}
	entrySizes := make([]int, len(catEntries))
	for i, e := range catEntries {
		entrySizes[i] = len(e)
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
		for _, ei := range group {
			payload = append(payload, catEntries[ei]...)
		}
		pages = append(pages, dirtyPage{index: catPages[gi], bytes: makePage(ps, pageCatalog, uint32(len(group)), nextPage, payload)})
	}

	return incrementalWrite{pages: pages, rootPage: catRoot, pageCount: alloc.next, freeRemaining: alloc.free[alloc.cursor:]}, nil
}

// resolveForEncode materializes any unfetched values in row for re-encoding at commit
// (spec/design/large-values.md §14): a dirty leaf may carry rows the lazy load left as
// references; the serializer needs their bytes to re-plan and rewrite the record. Unmetered,
// like all commit work. Returns the row unchanged when nothing is unfetched (the common case);
// resolution builds a fresh copy, never mutating the shared tree's row.
func resolveForEncode(row Row, colTypes []ColType, paging *sharedPaging) (Row, error) {
	needs := false
	for _, v := range row {
		if v.Kind == ValUnfetched {
			needs = true
			break
		}
	}
	if !needs {
		return row, nil
	}
	if paging == nil {
		return nil, NewError(DataCorrupted, "unfetched large value with no pager at commit")
	}
	fetch := func(p uint32) ([]byte, error) { return paging.readBlock(p) }
	out := make(Row, len(row))
	copy(out, row)
	for i := range out {
		if out[i].Kind == ValUnfetched {
			v, err := resolveUnfetched(colTypes[i], out[i].Unf, fetch)
			if err != nil {
				return nil, err
			}
			out[i] = v
		}
	}
	return out, nil
}

// serializeDirty assigns a page to one dirty node (and its dirty descendants) post-order, appending
// each as a full pageSize page to *pages, and returns this node's page index. A clean node (already
// persisted, page != 0) short-circuits: its whole subtree is on disk unchanged (copy-on-write only
// rebuilds the modified path), so nothing is written and its existing page is returned. The node's
// set-once page id is stored here — safe, as the working tree is owned by the single writer at commit.
// Page indices come from the allocator (free-list first, then the high-water). Mirrors serializeNode
// for the byte layout.
func serializeDirty(n *pnode, colTypes []ColType, capacity, ps int, alloc *pageAlloc, pages *[]dirtyPage, paging *sharedPaging) (uint32, error) {
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
		cp, err := serializeDirty(c.node, colTypes, capacity, ps, alloc, pages, paging)
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
	// (free-list first, then high-water — large-values.md §12). A dirty node may carry rows the
	// lazy load left unfetched (a sibling row's mutation dirtied them): resolve those through the
	// pager first — unmetered commit work, large-values.md §14 — so the re-encode re-plans the
	// resident row exactly as an eager writer would (chains are rewritten fresh; sharing an
	// unchanged chain is the deferred byte-layout follow-on).
	var ovf []overflowPageOut
	for i := range n.keys {
		row, err := resolveForEncode(n.vals[i], colTypes, paging)
		if err != nil {
			return 0, err
		}
		payload = append(payload, encodeRecord(colTypes, n.keys[i], row, capacity, alloc.take, &ovf)...)
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
	if !pageSizeValid(pageSize) || len(image) < pageSize*2 {
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
			// Each catalog entry is kind-tagged (v9): 1 = a composite-type entry (registered now;
			// its nested refs are validated after the full walk), 0 = a table entry.
			kind, err := readU8(pg.payload, &pos)
			if err != nil {
				return nil, err
			}
			if kind == 1 {
				ct, err := decodeCompositeTypeEntry(pg.payload, &pos)
				if err != nil {
					return nil, err
				}
				snap.putType(ct)
				continue
			}
			if kind == 2 {
				// A sequence entry (v12): self-contained, registered directly (no two-pass).
				sq, err := decodeSequenceEntry(pg.payload, &pos)
				if err != nil {
					return nil, err
				}
				snap.putSequence(sq)
				continue
			}
			if kind == 3 {
				// A collation snapshot (v17): the baked .coll artifact + an is_default flag
				// (spec/design/collation.md §5). The default restores the per-database default.
				coll, isDefault, err := decodeCollationEntry(pg.payload, &pos)
				if err != nil {
					return nil, err
				}
				if isDefault {
					snap.defaultCollation = coll.Name
				}
				snap.collations[coll.Name] = coll
				continue
			}
			if kind != 0 {
				return nil, NewError(DataCorrupted, "unknown catalog entry kind")
			}
			table, tableRoot, indexRoots, err := decodeTableEntry(pg.payload, &pos)
			if err != nil {
				return nil, err
			}
			name := table.Name
			hasPK := len(table.PKIndices()) > 0
			snap.putTable(table, uint32(pageSize))
			// The store resolved each column's ColType from the (types-first) catalog at putTable; the
			// codec reads it back rather than re-walking the type catalog (spec/design/composite.md §3).
			colTypes := snap.stores[strings.ToLower(name)].colTypes
			if tableRoot != 0 {
				root, length, err := readTree(image, pageSize, tableRoot, colTypes, reached)
				if err != nil {
					return nil, err
				}
				store := snap.stores[strings.ToLower(name)]
				store.setTree(root, length)
				// No-PK keys are synthetic i64 rowids — advance the counter past the largest (the
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
			// The table's index trees (v5): zero-column stores of entry keys
			// (spec/design/indexes.md §3), reachable pages included in the walk.
			for k, idx := range table.Indexes {
				istore := NewTableStore(pageSize-pageHeader, nil)
				if indexRoots[k] != 0 {
					root, length, err := readTree(image, pageSize, indexRoots[k], nil, reached)
					if err != nil {
						return nil, err
					}
					istore.setTree(root, length)
				}
				snap.putIndexStore(strings.ToLower(idx.Name), istore)
			}
		}
		catPage = pg.nextPage
	}
	// Two-pass: validate the composite-type catalog (existence + acyclicity) now that every type
	// entry has been read (spec/design/composite.md §3); a bad reference is XX001.
	if err := snap.validateCompositeTypes(); err != nil {
		return nil, err
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
	if !pageSizeValid(pageSize) {
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
			// Each catalog entry is kind-tagged (v9): 1 = a composite-type entry (registered now;
			// its nested refs are validated after the full walk), 0 = a table entry.
			kind, err := readU8(pg.payload, &pos)
			if err != nil {
				return nil, err
			}
			if kind == 1 {
				ct, err := decodeCompositeTypeEntry(pg.payload, &pos)
				if err != nil {
					return nil, err
				}
				snap.putType(ct)
				continue
			}
			if kind == 2 {
				// A sequence entry (v12): self-contained, registered directly (no two-pass).
				sq, err := decodeSequenceEntry(pg.payload, &pos)
				if err != nil {
					return nil, err
				}
				snap.putSequence(sq)
				continue
			}
			if kind == 3 {
				// A collation snapshot (v17): the baked .coll artifact + an is_default flag
				// (spec/design/collation.md §5). The default restores the per-database default.
				coll, isDefault, err := decodeCollationEntry(pg.payload, &pos)
				if err != nil {
					return nil, err
				}
				if isDefault {
					snap.defaultCollation = coll.Name
				}
				snap.collations[coll.Name] = coll
				continue
			}
			if kind != 0 {
				return nil, NewError(DataCorrupted, "unknown catalog entry kind")
			}
			table, tableRoot, indexRoots, err := decodeTableEntry(pg.payload, &pos)
			if err != nil {
				return nil, err
			}
			name := strings.ToLower(table.Name)
			hasPK := len(table.PKIndices()) > 0
			snap.putTable(table, uint32(pageSize))
			store := snap.stores[name]
			store.attachPaging(paging)
			// The store resolved each column's ColType from the (types-first) catalog at putTable
			// (spec/design/composite.md §3).
			colTypes := store.colTypes
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
			// The table's index trees (v5): zero-column demand-paged stores of entry keys
			// (spec/design/indexes.md §3); no spillable columns, so no overflow collection
			// is ever needed.
			for k, idx := range table.Indexes {
				istore := NewTableStore(pageSize-pageHeader, nil)
				istore.attachPaging(paging)
				if indexRoots[k] != 0 {
					root, length, err := readSkeleton(paging, indexRoots[k], nil, reached)
					if err != nil {
						return nil, err
					}
					istore.setTree(root, length)
				}
				snap.putIndexStore(strings.ToLower(idx.Name), istore)
			}
		}
		catPage = pg.nextPage
	}

	// Two-pass: validate the composite-type catalog (existence + acyclicity) — XX001 on a bad
	// reference (spec/design/composite.md §3).
	if err := snap.validateCompositeTypes(); err != nil {
		return nil, err
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
func anySpillableMasked(colTypes []ColType, mask []bool) bool {
	for i, ty := range colTypes {
		if mask[i] && isSpillable(ty) {
			return true
		}
	}
	return false
}

// anySpillable reports whether any column type can spill out-of-line (large-values.md §12).
func anySpillable(colTypes []ColType) bool {
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
// the paged-open free-list reconstruction; it decodes each leaf lazily and follows its chains by
// HEADERS only (chainPages — large-values.md §14), so opening a file never materializes or
// decompresses a large value.
func collectLeafOverflow(paging *sharedPaging, pageIdx uint32, colTypes []ColType, reached map[uint32]bool) error {
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
			_, row, _, err := decodeRecordLazy(colTypes, pg.payload, &pos)
			if err != nil {
				return err
			}
			if err := markChains(row, fetch, reached); err != nil {
				return err
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
func readSkeleton(paging *sharedPaging, root uint32, colTypes []ColType, reached map[uint32]bool) (*pnode, int, error) {
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
func readSkeletonNode(paging *sharedPaging, pageIdx uint32, colTypes []ColType, reached map[uint32]bool) (childRef, int, error) {
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
		// Separators decode lazily like leaves (large-values.md §14): an external value stays an
		// unfetched reference; its chain is marked reachable by headers only.
		fetch := func(p uint32) ([]byte, error) { return paging.pgr.readBlock(p) }
		for i := 0; i < n; i++ {
			key, row, w, err := decodeRecordLazy(colTypes, pg.payload, &pos)
			if err != nil {
				return childRef{}, 0, err
			}
			weights = append(weights, uint32(w))
			if err := markChains(row, fetch, reached); err != nil {
				return childRef{}, 0, err
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
func readTree(image []byte, ps int, pageIdx uint32, colTypes []ColType, reached map[uint32]bool) (*pnode, int, error) {
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
// type). Fixed-width scalars (int*/boolean/uuid/timestamp*) are tiny and always stay inline
// (spec/design/large-values.md §12). A COMPOSITE is treated as spillable — its opaque inline body
// spills via the same overflow + LZ4 path when a record exceeds RECORD_MAX (spec/design/composite.md
// §4); a small composite is never actually chosen by the plan.
func isSpillable(ty ColType) bool {
	if ty.Composite || ty.Elem != nil || ty.RangeElem != nil {
		// An array's opaque inline body spills via the same overflow + LZ4 path (array.md §4). A
		// range's body is its flags byte + bound bodies; a numrange over huge decimals could exceed
		// RECORD_MAX, so it rides the same path (a discrete range is tiny — never actually chosen by
		// the plan, spec/design/ranges.md §4).
		return true
	}
	return ty.Scalar.IsText() || ty.Scalar.IsBytea() || ty.Scalar.IsDecimal()
}

// recordMaxFor is the largest a single record may serialize to and still satisfy the B-tree split
// contract — RECORD_MAX = (C-12)/2 where C = capacity is the page payload (format.md "Why the
// record cap"). The spill planner reduces a record to ≤ this by externalizing values.
func recordMaxFor(capacity int) int {
	m := (capacity - interiorReserve) / 2
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
func planDispositions(colTypes []ColType, key []byte, row Row, capacity int) recordPlan {
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
func recordSize(colTypes []ColType, key []byte, row Row, capacity int) int {
	return planDispositions(colTypes, key, row, capacity).size
}

// recordScanUnits returns the per-record units a scan's up-front cost block charges beyond the
// B-tree nodes (cost.md §3; large-values.md §8/§12/§14): for every column in the query's TOUCHED
// SET (mask), pages = one page_read per overflow chain page (the chain carries the payload for
// external-plain, the COMPRESSED block for external-compressed) and decompress = ceil(raw/capacity)
// value_decompress slabs per compressed stored value (inline- or external-). Zero/zero for a
// fully-inline-plain record or an untouched column.
func recordScanUnits(colTypes []ColType, key []byte, row Row, capacity int, mask []bool) (pages, decompress int) {
	// A lazily-loaded row carries its on-disk forms as unfetched references (large-values.md
	// §14): read the units straight off them — no disposition re-plan, which would need the
	// unfetched bytes. The numbers equal the resident plan below by construction (the
	// references ARE that plan's stored output), so a paged and an in-memory database charge
	// identically (cost.md §3, logical cost).
	lazy := false
	for _, v := range row {
		if v.Kind == ValUnfetched {
			lazy = true
			break
		}
	}
	if lazy {
		for i, v := range row {
			if !mask[i] || v.Kind != ValUnfetched {
				continue
			}
			switch v.Unf.Form {
			case tagExternal:
				pages += (int(v.Unf.StoredLen) + capacity - 1) / capacity
			case tagInlineComp:
				decompress += (int(v.Unf.RawLen) + capacity - 1) / capacity
			case tagExternalComp:
				pages += (int(v.Unf.StoredLen) + capacity - 1) / capacity
				decompress += (int(v.Unf.RawLen) + capacity - 1) / capacity
			}
		}
		return pages, decompress
	}
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
func recordCompressUnits(colTypes []ColType, key []byte, row Row, capacity int) int {
	return planDispositions(colTypes, key, row, capacity).compressUnits
}

// valuePayload is a value's content payload P(v) — the bytes stored in the overflow chain when it is
// externalized (large-values.md §12): raw UTF-8 for text / raw bytes for bytea (both in v.Str), the
// decimal body (encoding minus its presence tag) for decimal. Only spillable types reach here.
func valuePayload(ty ColType, v Value) []byte {
	if ty.Elem != nil {
		// An array's payload is its body (the ndim/flags/dims header + bitmap + element bodies);
		// a large array spills through the same overflow + LZ4 path (spec/design/array.md §4).
		return encodeArrayBody(*ty.Elem, v.Array)
	}
	if ty.RangeElem != nil {
		// A range's payload is its body (the flags byte + present bound bodies, spec/design/ranges.md §4).
		return encodeRangeBody(*ty.RangeElem, v.Range)
	}
	if ty.Composite {
		// A composite's payload is its body — the encoding minus the leading presence tag, i.e. the
		// null bitmap + present-field bodies (spec/design/composite.md §4).
		return encodeCompositeBody(ty.Fields, *v.Comp)
	}
	switch {
	case ty.Scalar.IsText(), ty.Scalar.IsBytea():
		return []byte(v.Str)
	case ty.Scalar.IsDecimal():
		return encodeScalar(ty.Scalar, v)[1:] // strip the leading presence tag
	default:
		panic("only spillable values are externalized")
	}
}

// valueFromPayload reconstructs a value from the P(v) content gathered from its overflow chain
// (inverse of valuePayload) — large-values.md §12.
func valueFromPayload(ty ColType, payload []byte) (Value, error) {
	if ty.Elem != nil {
		// An array's payload is its body; decode it with a fresh cursor (spec/design/array.md §4).
		pos := 0
		return readArrayBody(ty, payload, &pos)
	}
	if ty.RangeElem != nil {
		// A range's payload is its body; decode it with a fresh cursor (spec/design/ranges.md §4).
		pos := 0
		return readRangeBody(*ty.RangeElem, payload, &pos)
	}
	if ty.Composite {
		// A composite's payload is its body (bitmap + present-field bodies); decode it with a fresh
		// cursor (spec/design/composite.md §4).
		pos := 0
		return readCompositeBody(ty, payload, &pos)
	}
	switch {
	case ty.Scalar.IsText():
		if !utf8.Valid(payload) {
			return Value{}, NewError(DataCorrupted, "non-UTF-8 text value")
		}
		return TextValue(string(payload)), nil
	case ty.Scalar.IsBytea():
		return ByteaValue(payload), nil
	case ty.Scalar.IsDecimal():
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
func encodeRecord(colTypes []ColType, key []byte, row Row, capacity int, take func() uint32, ovf *[]overflowPageOut) []byte {
	plan := planDispositions(colTypes, key, row, capacity)
	out := make([]byte, 0, 2+len(key)+len(row)*2)
	out = appendU16(out, uint16(len(key)))
	out = append(out, key...)
	for i, ty := range colTypes {
		switch plan.disp[i] {
		case dispExternal:
			payload := valuePayload(ty, row[i])
			first := writeOverflowChain(payload, capacity, take, ovf)
			out = append(out, tagExternal)
			out = appendU32(out, first)
			out = appendU32(out, uint32(len(payload)))
		case dispInlineComp:
			rawLen := len(valuePayload(ty, row[i]))
			comp := plan.comp[i]
			out = append(out, tagInlineComp)
			out = appendU32(out, uint32(rawLen))
			out = appendU16(out, uint16(len(comp)))
			out = append(out, comp...)
		case dispExternalComp:
			// The chain carries the COMPRESSED block (its page count follows comp size).
			rawLen := len(valuePayload(ty, row[i]))
			comp := plan.comp[i]
			first := writeOverflowChain(comp, capacity, take, ovf)
			out = append(out, tagExternalComp)
			out = appendU32(out, first)
			out = appendU32(out, uint32(len(comp)))
			out = appendU32(out, uint32(rawLen))
		default:
			out = append(out, encodeValue(ty, row[i])...)
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

// compositeTypeEntryBytes serializes a composite-type catalog entry's BODY (after its
// entry_kind = 1 byte): name, field count, then per field — name, type code, [type name when code
// 14 (nested composite)], flags (bit0 not_null), [decimal typmod when code 6]
// (spec/fileformat/format.md *Composite-type entry*).
func compositeTypeEntryBytes(ct *CompositeType) []byte {
	var out []byte
	out = appendU16(out, uint16(len(ct.Name)))
	out = append(out, ct.Name...)
	out = appendU16(out, uint16(len(ct.Fields)))
	for _, f := range ct.Fields {
		out = appendU16(out, uint16(len(f.Name)))
		out = append(out, f.Name...)
		if f.Type.Comp != nil {
			out = append(out, 14)
			out = appendU16(out, uint16(len(f.Type.Comp.Name)))
			out = append(out, f.Type.Comp.Name...)
		} else if f.Type.Array != nil {
			// An array-typed field (spec/design/array.md §12): type_code 15, then the same inline
			// element-type descriptor an array column uses (§3), before the flags byte — mirroring
			// where a nested-composite field's name sits.
			out = append(out, 15)
			out = pushArrayElementType(out, *f.Type.Array)
		} else {
			out = append(out, typeCodeForScalar(f.Type.ScalarTy()))
		}
		var flags byte
		if f.NotNull {
			flags |= 0b1
		}
		out = append(out, flags)
		if f.Type.Comp == nil && f.Type.IsDecimal() {
			var precision, scale uint16
			if f.Decimal != nil {
				precision, scale = f.Decimal.Precision, f.Decimal.Scale
			}
			out = appendU16(out, precision)
			out = appendU16(out, scale)
		}
	}
	return out
}

// decodeCompositeTypeEntry decodes a composite-type catalog entry's body (inverse of
// compositeTypeEntryBytes); the caller has already consumed the entry_kind byte. Nested composite
// fields hold the referenced type's NAME (resolved/validated after the whole catalog is read — the
// two-pass load).
func decodeCompositeTypeEntry(buf []byte, pos *int) (*CompositeType, error) {
	name, err := readString(buf, pos)
	if err != nil {
		return nil, err
	}
	fieldCount, err := readU16(buf, pos)
	if err != nil {
		return nil, err
	}
	fields := make([]CompositeField, 0, fieldCount)
	for i := uint16(0); i < fieldCount; i++ {
		fname, err := readString(buf, pos)
		if err != nil {
			return nil, err
		}
		tc, err := readU8(buf, pos)
		if err != nil {
			return nil, err
		}
		var fty Type
		isDecimal := false
		if tc == 14 {
			tn, err := readString(buf, pos)
			if err != nil {
				return nil, err
			}
			fty = CompositeT(tn)
		} else if tc == 15 {
			// An array-typed field (spec/design/array.md §12): the element-type descriptor, then
			// (below) the flags byte — the inverse of the array arm in compositeTypeEntryBytes.
			elem, err := readArrayElementType(buf, pos)
			if err != nil {
				return nil, err
			}
			fty = ArrayT(elem)
		} else {
			s, ok := scalarForTypeCode(tc)
			if !ok {
				return nil, NewError(DataCorrupted, "unknown field type code")
			}
			fty = ScalarT(s)
			isDecimal = s.IsDecimal()
		}
		flags, err := readU8(buf, pos)
		if err != nil {
			return nil, err
		}
		if flags&^uint8(0b1) != 0 {
			return nil, NewError(DataCorrupted, "reserved composite field flag set")
		}
		var decimal *DecimalTypmod
		if isDecimal {
			precision, err := readU16(buf, pos)
			if err != nil {
				return nil, err
			}
			scale, err := readU16(buf, pos)
			if err != nil {
				return nil, err
			}
			if precision != 0 {
				decimal = &DecimalTypmod{Precision: precision, Scale: scale}
			}
		}
		fields = append(fields, CompositeField{Name: fname, Type: fty, Decimal: decimal, NotNull: flags&0b1 != 0})
	}
	return &CompositeType{Name: name, Fields: fields}, nil
}

// sequenceEntryBytes serializes a sequence catalog entry's BODY (after its entry_kind = 2 byte):
// name, then the six fixed i64 fields (big-endian two's-complement, no sign-flip) and a flags byte
// — spec/fileformat/format.md *Sequence entry*. Fixed-width, every field present (no presence tags).
func sequenceEntryBytes(s *SequenceDef) []byte {
	var out []byte
	out = appendU16(out, uint16(len(s.Name)))
	out = append(out, s.Name...)
	out = appendI64(out, s.Increment)
	out = appendI64(out, s.MinValue)
	out = appendI64(out, s.MaxValue)
	out = appendI64(out, s.Start)
	out = appendI64(out, s.Cache)
	out = appendI64(out, s.LastValue)
	var flags byte
	if s.Cycle {
		flags |= 0b1
	}
	if s.IsCalled {
		flags |= 0b10
	}
	if s.OwnedBy != nil {
		flags |= 0b100 // bit2 has_owner (v13)
	}
	out = append(out, flags)
	// The OWNED BY tail (v13): only present when has_owner — owner table name + column ordinal
	// (spec/design/sequences.md §12, format.md *Sequence entry*).
	if s.OwnedBy != nil {
		out = appendU16(out, uint16(len(s.OwnedBy.Table)))
		out = append(out, s.OwnedBy.Table...)
		out = appendU16(out, s.OwnedBy.Column)
	}
	return out
}

// decodeSequenceEntry decodes a sequence catalog entry's body (inverse of sequenceEntryBytes); the
// caller has already consumed the entry_kind byte.
func decodeSequenceEntry(buf []byte, pos *int) (*SequenceDef, error) {
	name, err := readString(buf, pos)
	if err != nil {
		return nil, err
	}
	increment, err := readI64(buf, pos)
	if err != nil {
		return nil, err
	}
	minValue, err := readI64(buf, pos)
	if err != nil {
		return nil, err
	}
	maxValue, err := readI64(buf, pos)
	if err != nil {
		return nil, err
	}
	start, err := readI64(buf, pos)
	if err != nil {
		return nil, err
	}
	cache, err := readI64(buf, pos)
	if err != nil {
		return nil, err
	}
	lastValue, err := readI64(buf, pos)
	if err != nil {
		return nil, err
	}
	flags, err := readU8(buf, pos)
	if err != nil {
		return nil, err
	}
	if flags&^uint8(0b111) != 0 {
		return nil, NewError(DataCorrupted, "reserved sequence flag set")
	}
	// The OWNED BY tail (v13): present iff bit2 (has_owner) is set.
	var owner *SeqOwner
	if flags&0b100 != 0 {
		ownerTable, err := readString(buf, pos)
		if err != nil {
			return nil, err
		}
		ownerCol, err := readU16(buf, pos)
		if err != nil {
			return nil, err
		}
		owner = &SeqOwner{Table: ownerTable, Column: ownerCol}
	}
	return &SequenceDef{
		Name:      name,
		Increment: increment,
		MinValue:  minValue,
		MaxValue:  maxValue,
		Start:     start,
		Cache:     cache,
		Cycle:     flags&0b1 != 0,
		LastValue: lastValue,
		IsCalled:  flags&0b10 != 0,
		OwnedBy:   owner,
	}, nil
}

// collationEntryBytes serializes a collation-snapshot catalog entry's BODY (after its entry_kind = 3
// byte, v17): a flags byte (bit0 is_default, bit1 reference — deferred, 0/baked this slice) + the
// baked .coll artifact (u32 length + LZ4-compressed bytes). The artifact is byte-identical to
// db.SaveCollation, so a golden doubles as an artifact fixture (spec/design/collation.md §5).
func collationEntryBytes(c *Collation, isDefault bool) []byte {
	var out []byte
	var flags byte
	if isDefault {
		flags = 0b1
	}
	out = append(out, flags)
	artifact := SaveCollation(c)
	out = appendU32(out, uint32(len(artifact)))
	out = append(out, artifact...)
	return out
}

// decodeCollationEntry decodes a collation-snapshot entry's body (inverse of collationEntryBytes);
// the caller has consumed the entry_kind byte. Returns the loaded collation + whether it is the
// per-database default (the is_default flag bit).
func decodeCollationEntry(buf []byte, pos *int) (*Collation, bool, error) {
	flags, err := readU8(buf, pos)
	if err != nil {
		return nil, false, err
	}
	if flags&^uint8(0b11) != 0 {
		return nil, false, NewError(DataCorrupted, "reserved collation flag set")
	}
	if flags&0b10 != 0 {
		return nil, false, NewError(DataCorrupted, "reference-mode collation snapshots are not supported yet")
	}
	isDefault := flags&0b1 != 0
	n, err := readU32(buf, pos)
	if err != nil {
		return nil, false, err
	}
	if *pos+int(n) > len(buf) {
		return nil, false, NewError(DataCorrupted, "collation artifact truncated")
	}
	artifact := buf[*pos : *pos+int(n)]
	*pos += int(n)
	coll, err := OpenCollation(artifact)
	if err != nil {
		return nil, false, err
	}
	return coll, isDefault, nil
}

// tableEntryBytes builds one table's catalog entry (format.md). indexRoots is each
// index's tree root page, parallel to table.Indexes.
func tableEntryBytes(table *Table, rootDataPage uint32, indexRoots []uint32) []byte {
	var out []byte
	out = appendU16(out, uint16(len(table.Name)))
	out = append(out, table.Name...)
	out = appendU16(out, uint16(len(table.Columns)))
	for _, col := range table.Columns {
		out = appendU16(out, uint16(len(col.Name)))
		out = append(out, col.Name...)
		if col.Type.Comp != nil {
			// A composite column (v9): type_code 14, then flags, then the type name in the typmod
			// slot (spec/fileformat/format.md). Composite columns carry no default this slice, so
			// flags bits 2/3 are 0. Forward-ready — composite columns are not produced this slice
			// (composite.md §12), but the codec emits the code so a later-slice file is symmetric.
			out = append(out, 14)
			var flags byte
			if col.NotNull {
				flags |= 0b10
			}
			out = append(out, flags)
			out = appendU16(out, uint16(len(col.Type.Comp.Name)))
			out = append(out, col.Type.Comp.Name...)
			continue
		}
		if col.Type.Array != nil {
			// An array column (v10): type_code 15, flags, then the element type descriptor
			// (spec/design/array.md §3). Arrays carry no default this slice (flags bits 2/3 = 0).
			out = append(out, 15)
			var flags byte
			if col.NotNull {
				flags |= 0b10
			}
			out = append(out, flags)
			out = pushArrayElementType(out, *col.Type.Array)
			continue
		}
		if col.Type.Range != nil {
			// A range column (v16): type_code 17, flags, then the element type descriptor — one
			// scalar code (spec/design/ranges.md §3). Ranges carry no default this slice (flags bits
			// 2/3 = 0), so the entry is type_code ‖ flags ‖ element_code.
			out = append(out, 17)
			var flags byte
			if col.NotNull {
				flags |= 0b10
			}
			out = append(out, flags)
			out = pushRangeElementType(out, *col.Type.Range)
			continue
		}
		out = append(out, typeCodeForScalar(col.Type.ScalarTy()))
		// bit0 (primary_key through v4) is RETIRED in v5 — the pk ordinal list below is
		// the single authority; the bit is reserved, written 0 (spec/fileformat/format.md).
		var flags byte
		if col.NotNull {
			flags |= 0b10
		}
		if col.Default != nil {
			flags |= 0b100
		}
		if col.DefaultExpr != nil {
			// bit3 default_is_expr (v8) — mutually exclusive with bit2 (a column has at most
			// one of a constant or an expression default — spec/fileformat/format.md).
			flags |= 0b1000
		}
		// bit4 is_identity + bit5 identity_always (v15) — an IDENTITY column also carries not_null
		// (bit1) + the nextval expression default (bit3) — spec/design/sequences.md §13.
		if col.Identity != nil {
			flags |= 0b1_0000
			if *col.Identity == IdentityAlways {
				flags |= 0b10_0000
			}
		}
		// bit6 has_collation (v17) — a text column with a non-C effective collation
		// (spec/design/collation.md §5); the name is appended after the default.
		if col.Collation != "" {
			flags |= 0b100_0000
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
		// A column with a constant DEFAULT (flags bit2) appends its pre-evaluated default value
		// via the same value codec rows use — AFTER the typmod, presence-gated, so a column
		// without a default is byte-unchanged (spec/fileformat/format.md). A DEFAULT NULL is one
		// 0x01. An EXPRESSION default (flags bit3, v8) instead appends its expr-text (u16 length
		// + UTF-8) there, the same token rendering a CHECK uses — bit2/bit3 are exclusive.
		if col.Default != nil {
			// A column DEFAULT is always a scalar value (composite columns carry no default this
			// slice — composite.md §12), so encode the scalar body directly.
			out = append(out, encodeScalar(col.Type.ScalarTy(), *col.Default)...)
		} else if col.DefaultExpr != nil {
			out = appendU16(out, uint16(len(col.DefaultExpr.ExprText)))
			out = append(out, col.DefaultExpr.ExprText...)
		}
		// The effective collation name (v17, flags bit6) — last in the per-column entry, so a
		// non-collated column is byte-unchanged (spec/design/collation.md §5).
		if col.Collation != "" {
			out = appendU16(out, uint16(len(col.Collation)))
			out = append(out, col.Collation...)
		}
	}
	// The primary key (v5): count, then the member column ordinals in KEY order
	// (constraints.md §3 — the list persists an order independent of declaration order).
	out = appendU16(out, uint16(len(table.PK)))
	for _, i := range table.PK {
		out = appendU16(out, uint16(i))
	}
	// CHECK constraints (v4): count, then (name, expression text) per check, in the
	// catalog's evaluation order — the text is written back VERBATIM, so the bytes are
	// stable across create → commit → load → commit (spec/fileformat/format.md
	// "Check-expression text").
	out = appendU16(out, uint16(len(table.Checks)))
	for _, check := range table.Checks {
		out = appendU16(out, uint16(len(check.Name)))
		out = append(out, check.Name...)
		out = appendU16(out, uint16(len(check.ExprText)))
		out = append(out, check.ExprText...)
	}
	// Secondary indexes (v5): count, then per index the name, key-column ordinals
	// (index-key order, duplicates allowed), the v6 flags byte (bit0 unique —
	// spec/design/indexes.md §8), and its tree's root page — in the catalog's ascending
	// lowercased-name order (spec/design/indexes.md §6).
	out = appendU16(out, uint16(len(table.Indexes)))
	for k, idx := range table.Indexes {
		out = appendU16(out, uint16(len(idx.Name)))
		out = append(out, idx.Name...)
		out = appendU16(out, uint16(len(idx.Columns)))
		for _, c := range idx.Columns {
			out = appendU16(out, uint16(c))
		}
		if idx.Unique {
			out = append(out, 1)
		} else {
			out = append(out, 0)
		}
		out = append(out, byte(idx.Kind)) // v12: index_kind byte (0 = btree, 1 = GIN)
		out = appendU32(out, indexRoots[k])
	}
	// Foreign keys (v11): count, then per FK the name, the local-column ordinals (into THIS
	// table, list order), the referenced table name, the referenced-column ordinals (into the
	// PARENT, list order), and the actions byte (bits 0-1 on_delete, bits 2-3 on_update) — in the
	// catalog's ascending lowercased-name order (spec/design/constraints.md §6.9). An FK owns no
	// B-tree (no root page).
	out = appendU16(out, uint16(len(table.ForeignKeys)))
	for _, fk := range table.ForeignKeys {
		out = appendU16(out, uint16(len(fk.Name)))
		out = append(out, fk.Name...)
		out = appendU16(out, uint16(len(fk.Columns)))
		for _, c := range fk.Columns {
			out = appendU16(out, uint16(c))
		}
		out = appendU16(out, uint16(len(fk.RefTable)))
		out = append(out, fk.RefTable...)
		out = appendU16(out, uint16(len(fk.RefColumns)))
		for _, c := range fk.RefColumns {
			out = appendU16(out, uint16(c))
		}
		out = append(out, fkActionCode(fk.OnDelete)|(fkActionCode(fk.OnUpdate)<<2))
	}
	out = appendU32(out, rootDataPage)
	return out
}

// fkActionCode is the 2-bit on-disk code for a referential action (format.md): NO ACTION = 0,
// RESTRICT = 1.
func fkActionCode(a FkAction) byte {
	switch a {
	case FkRestrict:
		return 1
	default:
		return 0
	}
}

// fkActionFromCode decodes a 2-bit referential-action code; an unsupported code (2/3, reserved
// for the deferred write-actions) in an otherwise-valid file is XX001.
func fkActionFromCode(c byte) (FkAction, error) {
	switch c {
	case 0:
		return FkNoAction, nil
	case 1:
		return FkRestrict, nil
	default:
		return 0, NewError(DataCorrupted, "unsupported foreign-key action code")
	}
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
	// The per-page checksum (v7) is computed last, over every byte but its own field at [12,16).
	binary.BigEndian.PutUint32(p[12:], pageCRC(p))
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
	// Verify the per-page checksum (v7) before trusting any header field — a mismatch is silent
	// at-rest corruption (format.md *Page header*; storage.md §6).
	if pageCRC(block) != binary.BigEndian.Uint32(block[12:16]) {
		return page{}, NewError(DataCorrupted, "page checksum mismatch (corrupted page)")
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
// on the node so a later incremental commit keeps it clean. Decoding is LAZY (large-values.md §14):
// an external/compressed value becomes an Unfetched reference — no chain read, no decompression —
// resolved later only for the columns a query touches. Each weight is the bytes the record occupies
// on the page (exactly the writer's recordSize).
func decodeLeafNode(block []byte, pageID uint32, colTypes []ColType) (*pnode, error) {
	pg, err := parsePage(block)
	if err != nil {
		return nil, err
	}
	if pg.pageType != pageLeaf {
		return nil, NewError(DataCorrupted, "demand-paged a non-leaf page")
	}
	n := int(pg.itemCount)
	keys, vals, weights := make([][]byte, 0, n), make([]Row, 0, n), make([]uint32, 0, n)
	pos := 0
	for i := 0; i < n; i++ {
		key, row, w, err := decodeRecordLazy(colTypes, pg.payload, &pos)
		if err != nil {
			return nil, err
		}
		weights = append(weights, uint32(w))
		keys = append(keys, key)
		vals = append(vals, row)
	}
	return &pnode{keys: keys, vals: vals, weights: weights, page: pageID}, nil
}

// decodeTableEntry decodes one catalog table entry: the *Table (its pk list, checks, and
// index definitions included), its root_data_page, and each index's root page (parallel
// to Table.Indexes).
func decodeTableEntry(buf []byte, pos *int) (*Table, uint32, []uint32, error) {
	name, err := readString(buf, pos)
	if err != nil {
		return nil, 0, nil, err
	}
	colCount, err := readU16(buf, pos)
	if err != nil {
		return nil, 0, nil, err
	}
	columns := make([]Column, 0, colCount)
	for i := uint16(0); i < colCount; i++ {
		cname, err := readString(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		tc, err := readU8(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		if tc == 14 {
			// A composite column (v9): flags, then the type name (spec/fileformat/format.md).
			// Forward-ready — composite columns are not produced this slice (composite.md §12),
			// but a reader handles the code so a later-slice file loads cleanly.
			flags, err := readU8(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			if flags&0b01 != 0 {
				return nil, 0, nil, NewError(DataCorrupted, "reserved column flag bit0 set")
			}
			tname, err := readString(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			columns = append(columns, Column{
				Name:    cname,
				Type:    CompositeT(tname),
				NotNull: flags&0b10 != 0,
			})
			continue
		}
		if tc == 15 {
			// An array column (v10): flags, then the element type descriptor (array.md §3).
			flags, err := readU8(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			if flags&0b01 != 0 {
				return nil, 0, nil, NewError(DataCorrupted, "reserved column flag bit0 set")
			}
			elem, err := readArrayElementType(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			columns = append(columns, Column{
				Name:    cname,
				Type:    ArrayT(elem),
				NotNull: flags&0b10 != 0,
			})
			continue
		}
		if tc == 17 {
			// A range column (v16): flags, then the element type descriptor — one scalar code
			// (spec/design/ranges.md §3). Ranges carry no default this slice.
			flags, err := readU8(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			if flags&0b01 != 0 {
				return nil, 0, nil, NewError(DataCorrupted, "reserved column flag bit0 set")
			}
			elem, err := readRangeElementType(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			columns = append(columns, Column{
				Name:    cname,
				Type:    RangeT(elem),
				NotNull: flags&0b10 != 0,
			})
			continue
		}
		ty, ok := scalarForTypeCode(tc)
		if !ok {
			return nil, 0, nil, NewError(DataCorrupted, "unknown type code")
		}
		flags, err := readU8(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		// bit0 was the primary_key flag through v4; v5 retired it (the pk list below is
		// the authority) and reserves it as must-be-zero. bit6 = has_collation (v17); bit7 reserved.
		if flags&0b01 != 0 {
			return nil, 0, nil, NewError(DataCorrupted, "reserved column flag bit0 set")
		}
		if flags&0b1000_0000 != 0 {
			return nil, 0, nil, NewError(DataCorrupted, "reserved column flag bit7 set")
		}
		// bit4 is_identity + bit5 identity_always (v15) — identity_always is meaningful only with
		// is_identity (spec/design/sequences.md §13).
		if flags&0b11_0000 == 0b10_0000 {
			return nil, 0, nil, NewError(DataCorrupted, "identity_always set without is_identity")
		}
		var identity *IdentityKind
		if flags&0b1_0000 != 0 {
			k := IdentityByDefault
			if flags&0b10_0000 != 0 {
				k = IdentityAlways
			}
			identity = &k
		}
		// A decimal column carries its typmod (precision, scale); precision 0 = unconstrained.
		var decimal *DecimalTypmod
		if ty.IsDecimal() {
			precision, err := readU16(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			scale, err := readU16(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			if precision != 0 {
				decimal = &DecimalTypmod{Precision: precision, Scale: scale}
			}
		}
		// The default follows the typmod (spec/fileformat/format.md): a CONSTANT default (flags
		// bit2) is a value via the same value codec rows use — never externalized, so no
		// overflow reader is needed (a 0x02 tag here would be a corrupt catalog). An EXPRESSION
		// default (flags bit3, v8) is instead the expr-text (u16 length + UTF-8), re-parsed with
		// the ordinary expression parser (XX001 if it fails, like a stored check). The two bits
		// are mutually exclusive — both set is a corrupt catalog.
		if flags&0b1100 == 0b1100 {
			return nil, 0, nil, NewError(DataCorrupted, "column has both a constant and an expression default")
		}
		var defaultVal *Value
		if flags&0b100 != 0 {
			var sink []uint32
			// A constant default is a scalar value (this branch is the scalar type path).
			dv, err := readValue(ScalarColType(ty), buf, pos, nil, &sink)
			if err != nil {
				return nil, 0, nil, err
			}
			defaultVal = &dv
		}
		var defaultExpr *DefaultExpr
		if flags&0b1000 != 0 {
			exprText, err := readString(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			expr, err := ParseExpression(exprText)
			if err != nil {
				return nil, 0, nil, NewError(DataCorrupted, "stored default expression does not parse: "+err.Error())
			}
			defaultExpr = &DefaultExpr{ExprText: exprText, Expr: expr}
		}
		// The effective collation (v17, flags bit6) — appended last; a non-collated column has the
		// bit clear and reads nothing (spec/design/collation.md §5).
		collation := ""
		if flags&0b100_0000 != 0 {
			collation, err = readString(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
		}
		columns = append(columns, Column{
			Name:    cname,
			Type:    ScalarT(ty),
			Decimal: decimal,
			// PrimaryKey is set from the pk list below.
			NotNull:     flags&0b10 != 0,
			Default:     defaultVal,
			DefaultExpr: defaultExpr,
			Identity:    identity,
			Collation:   collation,
		})
	}
	// The primary key (v5): member ordinals in KEY order. Each must name a real column,
	// once; membership sets the per-column convenience flag.
	pkCount, err := readU16(buf, pos)
	if err != nil {
		return nil, 0, nil, err
	}
	pk := make([]int, 0, pkCount)
	for i := uint16(0); i < pkCount; i++ {
		ord, err := readU16(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		o := int(ord)
		if o >= len(columns) || slices.Contains(pk, o) {
			return nil, 0, nil, NewError(DataCorrupted, "invalid primary key ordinal")
		}
		columns[o].PrimaryKey = true
		pk = append(pk, o)
	}
	// CHECK constraints (v4): the stored expression text re-parses with the ordinary
	// expression parser — it was written by the token renderer, so this cannot fail for a
	// file the engine wrote; failure means the file lied (XX001, constraints.md §4.5).
	checkCount, err := readU16(buf, pos)
	if err != nil {
		return nil, 0, nil, err
	}
	checks := make([]CheckConstraint, 0, checkCount)
	for i := uint16(0); i < checkCount; i++ {
		checkName, err := readString(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		exprText, err := readString(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		expr, err := ParseExpression(exprText)
		if err != nil {
			return nil, 0, nil, NewError(DataCorrupted,
				"stored check constraint does not parse: "+err.Error())
		}
		checks = append(checks, CheckConstraint{Name: checkName, ExprText: exprText, Expr: expr})
	}
	// Secondary indexes (v5): name + key-column ordinals + the v6 flags byte (bit0
	// unique; the rest reserved-zero) + root page, in the catalog's (lowercased-name
	// ascending) order — a reader trusts the order. Duplicate ordinals within one index
	// are legal (indexes.md §1).
	indexCount, err := readU16(buf, pos)
	if err != nil {
		return nil, 0, nil, err
	}
	indexes := make([]IndexDef, 0, indexCount)
	indexRoots := make([]uint32, 0, indexCount)
	for i := uint16(0); i < indexCount; i++ {
		iname, err := readString(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		kc, err := readU16(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		if kc == 0 {
			return nil, 0, nil, NewError(DataCorrupted, "index with no key columns")
		}
		cols := make([]int, 0, kc)
		for j := uint16(0); j < kc; j++ {
			ord, err := readU16(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			if int(ord) >= len(columns) {
				return nil, 0, nil, NewError(DataCorrupted, "invalid index column ordinal")
			}
			cols = append(cols, int(ord))
		}
		iflags, err := readU8(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		if iflags&^uint8(0b01) != 0 {
			return nil, 0, nil, NewError(DataCorrupted, "reserved index flag set")
		}
		ikind, err := readU8(buf, pos) // v12: index_kind byte (0 = btree, 1 = GIN)
		if err != nil {
			return nil, 0, nil, err
		}
		if ikind > 1 {
			return nil, 0, nil, NewError(DataCorrupted, "unsupported index kind")
		}
		iroot, err := readU32(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		indexes = append(indexes, IndexDef{Name: iname, Columns: cols, Unique: iflags&0b01 != 0, Kind: IndexKind(ikind)})
		indexRoots = append(indexRoots, iroot)
	}
	// Foreign keys (v11): name + local ordinals + referenced table + referenced ordinals + the
	// actions byte, in the catalog's (lowercased-name ascending) order — a reader trusts the
	// order. The local ordinals index THIS table; the referenced ordinals index the PARENT (whose
	// entry may be decoded later, so they are not cross-checked here — the writer keeps them
	// valid; a structurally impossible FK is rejected below).
	fkCount, err := readU16(buf, pos)
	if err != nil {
		return nil, 0, nil, err
	}
	foreignKeys := make([]ForeignKey, 0, fkCount)
	for i := uint16(0); i < fkCount; i++ {
		fname, err := readString(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		lc, err := readU16(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		if lc == 0 {
			return nil, 0, nil, NewError(DataCorrupted, "foreign key with no columns")
		}
		cols := make([]int, 0, lc)
		for j := uint16(0); j < lc; j++ {
			ord, err := readU16(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			if int(ord) >= len(columns) {
				return nil, 0, nil, NewError(DataCorrupted, "invalid foreign-key column ordinal")
			}
			cols = append(cols, int(ord))
		}
		refTable, err := readString(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		rc, err := readU16(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		if rc != lc {
			return nil, 0, nil, NewError(DataCorrupted, "foreign-key referencing/referenced column count mismatch")
		}
		refCols := make([]int, 0, rc)
		for j := uint16(0); j < rc; j++ {
			ord, err := readU16(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			refCols = append(refCols, int(ord))
		}
		actions, err := readU8(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		if actions&^byte(0b1111) != 0 {
			return nil, 0, nil, NewError(DataCorrupted, "reserved foreign-key action bit set")
		}
		onDelete, err := fkActionFromCode(actions & 0b11)
		if err != nil {
			return nil, 0, nil, err
		}
		onUpdate, err := fkActionFromCode((actions >> 2) & 0b11)
		if err != nil {
			return nil, 0, nil, err
		}
		foreignKeys = append(foreignKeys, ForeignKey{
			Name:       fname,
			Columns:    cols,
			RefTable:   refTable,
			RefColumns: refCols,
			OnDelete:   onDelete,
			OnUpdate:   onUpdate,
		})
	}
	root, err := readU32(buf, pos)
	if err != nil {
		return nil, 0, nil, err
	}
	return &Table{Name: name, Columns: columns, PK: pk, Checks: checks, Indexes: indexes, ForeignKeys: foreignKeys}, root, indexRoots, nil
}

// readValueLazy reads one value lazily (spec/design/large-values.md §14): inline-plain and NULL
// decode as today, but an external/compressed form becomes an Unfetched reference holding exactly
// the record's pointer fields — no chain read, no decompression. The scan layer resolves the
// references for the columns a query touches (resolveUnfetched); the commit path resolves the
// rest when a dirty leaf re-encodes (resolveForEncode).
func readValueLazy(ty ColType, buf []byte, pos *int) (Value, error) {
	tag, err := readU8(buf, pos)
	if err != nil {
		return Value{}, err
	}
	switch tag {
	case 0x00:
		// A composite's inline body has no nested overflow pointers (its fields are inline —
		// composite.md §4), so it is read eagerly even in the lazy path.
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
		return Value{Kind: ValUnfetched, Unf: &Unfetched{Form: tagExternal, FirstPage: first, StoredLen: length}}, nil
	case tagInlineComp:
		rawLen, err := readU32(buf, pos)
		if err != nil {
			return Value{}, err
		}
		compLen, err := readU16(buf, pos)
		if err != nil {
			return Value{}, err
		}
		compSlice, err := take(buf, pos, int(compLen))
		if err != nil {
			return Value{}, err
		}
		comp := make([]byte, len(compSlice))
		copy(comp, compSlice)
		return Value{Kind: ValUnfetched, Unf: &Unfetched{Form: tagInlineComp, RawLen: rawLen, Comp: comp}}, nil
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
		return Value{Kind: ValUnfetched, Unf: &Unfetched{Form: tagExternalComp, FirstPage: first, StoredLen: stored, RawLen: rawLen}}, nil
	default:
		return Value{}, NewError(DataCorrupted, "invalid value presence tag")
	}
}

// decodeRecordLazy decodes one record (readValueLazy per column) and returns (key, row, weight),
// where the weight is the bytes the record occupies on the page — exactly the recordSize the
// writer split on, read off the cursor instead of re-planned (a re-plan would need the unfetched
// bytes).
func decodeRecordLazy(colTypes []ColType, buf []byte, pos *int) ([]byte, Row, int, error) {
	start := *pos
	keyLen, err := readU16(buf, pos)
	if err != nil {
		return nil, nil, 0, err
	}
	keySlice, err := take(buf, pos, int(keyLen))
	if err != nil {
		return nil, nil, 0, err
	}
	key := make([]byte, len(keySlice))
	copy(key, keySlice)
	row := make(Row, len(colTypes))
	for i, ty := range colTypes {
		v, err := readValueLazy(ty, buf, pos)
		if err != nil {
			return nil, nil, 0, err
		}
		row[i] = v
	}
	return key, row, *pos - start, nil
}

// resolveUnfetched materializes an unfetched reference into its plain Value
// (spec/design/large-values.md §14): gather the overflow chain through fetch for an external
// form, decompress a compressed one, and reconstruct by column type. Decompression errors are
// data_corrupted, surfaced only when the value is actually touched.
func resolveUnfetched(ty ColType, u *Unfetched, fetch func(uint32) ([]byte, error)) (Value, error) {
	var sink []uint32
	switch u.Form {
	case tagExternal:
		payload, err := readOverflowChain(u.FirstPage, int(u.StoredLen), fetch, &sink)
		if err != nil {
			return Value{}, err
		}
		return valueFromPayload(ty, payload)
	case tagInlineComp:
		payload, err := lz4Decompress(u.Comp, int(u.RawLen))
		if err != nil {
			return Value{}, err
		}
		return valueFromPayload(ty, payload)
	case tagExternalComp:
		comp, err := readOverflowChain(u.FirstPage, int(u.StoredLen), fetch, &sink)
		if err != nil {
			return Value{}, err
		}
		payload, err := lz4Decompress(comp, int(u.RawLen))
		if err != nil {
			return Value{}, err
		}
		return valueFromPayload(ty, payload)
	default:
		return Value{}, NewError(DataCorrupted, "invalid unfetched value form")
	}
}

// chainPages returns the page indices of the overflow chain carrying length payload bytes from
// first, following next_page hops and reading HEADERS only — no payload assembly, no
// decompression (spec/design/large-values.md §14). The open-time reachability walk marks live
// chains with this, so opening a file never materializes its large values.
func chainPages(first uint32, length int, fetch func(uint32) ([]byte, error)) ([]uint32, error) {
	var out []uint32
	gathered := 0
	p := first
	for gathered < length {
		if p == 0 {
			return nil, NewError(DataCorrupted, "overflow chain ended before the value length")
		}
		out = append(out, p)
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
		if n == 0 || n > len(pg.payload) || gathered+n > length {
			return nil, NewError(DataCorrupted, "overflow page slab out of range")
		}
		gathered += n
		p = pg.nextPage
	}
	return out, nil
}

// markChains adds the overflow chain pages a lazily-decoded row references to reached (the
// free-list reachability walk), via the header-only chainPages hop.
func markChains(row Row, fetch func(uint32) ([]byte, error), reached map[uint32]bool) error {
	for _, v := range row {
		if v.Kind != ValUnfetched {
			continue
		}
		switch v.Unf.Form {
		case tagExternal, tagExternalComp:
			pages, err := chainPages(v.Unf.FirstPage, int(v.Unf.StoredLen), fetch)
			if err != nil {
				return err
			}
			for _, p := range pages {
				reached[p] = true
			}
		}
	}
	return nil
}

// decodeRecord decodes one record (key, row) and the overflow chain pages any external value
// followed (for the free-list reachability walk — large-values.md §12). fetch reads a page block by
// index, used to follow overflow chains; nil is only valid where no value can be external (a default).
func decodeRecord(colTypes []ColType, buf []byte, pos *int, fetch func(uint32) ([]byte, error)) ([]byte, Row, []uint32, error) {
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
func readValue(ty ColType, buf []byte, pos *int, fetch func(uint32) ([]byte, error), ovfOut *[]uint32) (Value, error) {
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

// readInlineBody reads the present-value body (after a 0x00 tag) for any ColType: a scalar via
// readInlineScalar, or a composite via readCompositeBody (spec/design/composite.md §4).
func readInlineBody(ty ColType, buf []byte, pos *int) (Value, error) {
	if ty.Elem != nil {
		return readArrayBody(ty, buf, pos)
	}
	if ty.RangeElem != nil {
		return readRangeBody(*ty.RangeElem, buf, pos)
	}
	if ty.Composite {
		return readCompositeBody(ty, buf, pos)
	}
	return readInlineScalar(ty.Scalar, buf, pos)
}

// readRangeBody reads a range value's present body (after the 0x00 tag): inverse of encodeRangeBody
// (spec/design/ranges.md §4). Reads the flags byte; an EMPTY range stops there. Otherwise the finite
// lower bound (!LB_INF) then the finite upper bound (!UB_INF) are each read as the element's
// value-codec body (no presence tag). A reserved flag bit set is XX001.
func readRangeBody(elem ColType, buf []byte, pos *int) (Value, error) {
	flags, err := readU8(buf, pos)
	if err != nil {
		return Value{}, err
	}
	if flags&^0x1f != 0 {
		return Value{}, NewError(DataCorrupted, "range flags has a reserved bit set")
	}
	if flags&0x01 != 0 {
		return RangeValue(EmptyRangeVal()), nil
	}
	lbInf := flags&0x02 != 0
	ubInf := flags&0x04 != 0
	var lower, upper *Value
	if !lbInf {
		v, err := readInlineBody(elem, buf, pos)
		if err != nil {
			return Value{}, err
		}
		lower = &v
	}
	if !ubInf {
		v, err := readInlineBody(elem, buf, pos)
		if err != nil {
			return Value{}, err
		}
		upper = &v
	}
	return RangeValue(&RangeVal{
		Empty:    false,
		Lower:    lower,
		Upper:    upper,
		LowerInc: flags&0x08 != 0,
		UpperInc: flags&0x10 != 0,
	}), nil
}

// readArrayBody reads an array value's present body (after the 0x00 tag): inverse of
// encodeArrayBody (spec/design/array.md §4). Reads ndim/flags/per-dim (len, lb), then the optional
// null bitmap and the present element bodies (row-major). Accepts ndim 0 (empty) through 6 (MAXDIM);
// a higher ndim or an element-count overflow is XX001.
func readArrayBody(ty ColType, buf []byte, pos *int) (Value, error) {
	if ty.Elem == nil {
		return Value{}, NewError(DataCorrupted, "readArrayBody on a non-array type")
	}
	ndim, err := readU8(buf, pos)
	if err != nil {
		return Value{}, err
	}
	flags, err := readU8(buf, pos)
	if err != nil {
		return Value{}, err
	}
	if flags&^0x01 != 0 {
		return Value{}, NewError(DataCorrupted, "array flags has a reserved bit set")
	}
	if ndim == 0 {
		// An empty array (ndim 0) — all-empty slices.
		return ArrayValueOf(EmptyArray()), nil
	}
	if ndim > 6 {
		return Value{}, NewError(DataCorrupted, "array ndim exceeds the maximum of 6")
	}
	dims := make([]int, ndim)
	lbounds := make([]int32, ndim)
	n := 1
	for d := 0; d < int(ndim); d++ {
		ln, err := readU32(buf, pos)
		if err != nil {
			return Value{}, err
		}
		lb, err := readU32(buf, pos) // lower bound (i32 two's-complement)
		if err != nil {
			return Value{}, err
		}
		dims[d] = int(ln)
		lbounds[d] = int32(lb)
		n *= int(ln)
		if n < 0 || n > (1<<31) {
			return Value{}, NewError(DataCorrupted, "array element count overflow")
		}
	}
	hasNulls := flags&0x01 != 0
	var bitmap []byte
	if hasNulls {
		bitmap, err = take(buf, pos, (n+7)/8)
		if err != nil {
			return Value{}, err
		}
	}
	elems := make([]Value, n)
	for i := 0; i < n; i++ {
		if hasNulls && bitmap[i/8]&(0x80>>uint(i%8)) != 0 {
			elems[i] = NullValue()
		} else {
			v, err := readInlineBody(*ty.Elem, buf, pos)
			if err != nil {
				return Value{}, err
			}
			elems[i] = v
		}
	}
	return ArrayValueOf(&ArrayVal{Dims: dims, Lbounds: lbounds, Elements: elems}), nil
}

// readCompositeBody reads a composite value's present body (after the 0x00 tag): the null bitmap then
// each present field's body in declaration order (inverse of encodeCompositeBody,
// spec/design/composite.md §4). A field whose bitmap bit is set is NULL and consumes no body bytes;
// otherwise its body is read recursively (no per-field presence tag).
func readCompositeBody(ty ColType, buf []byte, pos *int) (Value, error) {
	if !ty.Composite {
		return Value{}, NewError(DataCorrupted, "readCompositeBody on a non-composite type")
	}
	nbytes := (len(ty.Fields) + 7) / 8
	bitmap, err := take(buf, pos, nbytes)
	if err != nil {
		return Value{}, err
	}
	vals := make([]Value, len(ty.Fields))
	for i := range ty.Fields {
		if bitmap[i/8]&(0x80>>uint(i%8)) != 0 {
			vals[i] = NullValue()
		} else {
			v, err := readInlineBody(ty.Fields[i].Type, buf, pos)
			if err != nil {
				return Value{}, err
			}
			vals[i] = v
		}
	}
	return CompositeValue(vals), nil
}

// readInlineScalar reads the present-value body of a SCALAR (after a 0x00 tag): a fixed-width integer,
// a u16 length + UTF-8 bytes for text, a single bool-byte, the decimal body, etc. (format.md *Value
// codec*).
func readInlineScalar(ty ScalarType, buf []byte, pos *int) (Value, error) {
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
		if !utf8.Valid(sb) {
			return Value{}, NewError(DataCorrupted, "non-UTF-8 text value")
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
	case ty.IsFloat64():
		// Fixed 8 raw IEEE bytes big-endian, no length prefix. Branch before the integer path —
		// DecodeInt would sign-flip. Bits are reconstructed verbatim (spec/design/float.md §10).
		vb, err := take(buf, pos, 8)
		if err != nil {
			return Value{}, err
		}
		bits := uint64(vb[0])<<56 | uint64(vb[1])<<48 | uint64(vb[2])<<40 | uint64(vb[3])<<32 |
			uint64(vb[4])<<24 | uint64(vb[5])<<16 | uint64(vb[6])<<8 | uint64(vb[7])
		return Value{Kind: ValFloat64, Int: int64(bits)}, nil
	case ty.IsFloat32():
		vb, err := take(buf, pos, 4)
		if err != nil {
			return Value{}, err
		}
		bits := uint32(vb[0])<<24 | uint32(vb[1])<<16 | uint32(vb[2])<<8 | uint32(vb[3])
		return Value{Kind: ValFloat32, Int: int64(bits)}, nil
	case ty.IsDate():
		// 4-byte i32 day count, same order-preserving codec as i32 (spec/design/date.md).
		vb, err := take(buf, pos, ty.WidthBytes())
		if err != nil {
			return Value{}, err
		}
		return DateValue(int32(DecodeInt(ty, vb))), nil
	case ty.IsInterval():
		// Fixed 16-byte body: i32 months + i32 days + i64 micros, big-endian (no sign-flip).
		mb, err := take(buf, pos, 4)
		if err != nil {
			return Value{}, err
		}
		db, err := take(buf, pos, 4)
		if err != nil {
			return Value{}, err
		}
		ub, err := take(buf, pos, 8)
		if err != nil {
			return Value{}, err
		}
		months := int32(uint32(mb[0])<<24 | uint32(mb[1])<<16 | uint32(mb[2])<<8 | uint32(mb[3]))
		days := int32(uint32(db[0])<<24 | uint32(db[1])<<16 | uint32(db[2])<<8 | uint32(db[3]))
		micros := int64(uint64(ub[0])<<56 | uint64(ub[1])<<48 | uint64(ub[2])<<40 | uint64(ub[3])<<32 |
			uint64(ub[4])<<24 | uint64(ub[5])<<16 | uint64(ub[6])<<8 | uint64(ub[7]))
		return IntervalValue(Interval{Months: months, Days: days, Micros: micros}), nil
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

// readI64 reads an 8-byte big-endian two's-complement i64 (the sequence-entry field encoding).
func readI64(buf []byte, pos *int) (int64, error) {
	s, err := take(buf, pos, 8)
	if err != nil {
		return 0, err
	}
	return int64(binary.BigEndian.Uint64(s)), nil
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
