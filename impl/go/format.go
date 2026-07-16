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
	"fmt"
	"hash/crc32"
	"math"
	"slices"
	"sort"
	"strings"
	"unicode/utf8"
)

// magic — ASCII "JEDB" (the engine is named `jed`).
var magic = [4]byte{'J', 'E', 'D', 'B'}

const (
	formatVersion    uint16 = 29    // 29 = deterministic per-column statistics (kind 4; spec/design/statistics.md); 28 = exact table row count: each table catalog entry appends a nonnegative i64 row_count after root_data_page, with (root_data_page == 0) == (row_count == 0);  on-disk format version (27 = partial-index predicates (spec/design/indexes.md §9): the per-index index_flags byte gains bit1 has_predicate, and (only when set) a u16 length + the canonical predicate text (the Check-expression text form) follows index_root_page; on load a partial predicate re-parses that text (XX001 on failure, like a stored CHECK) and a non-btree index with bit1 set is data_corrupted. B-tree only. A non-partial index is byte-identical to v26, so a file with no partial index moves to v27 only by its version byte + meta CRC. 26 = expression index keys (spec/design/indexes.md §1/§6): a per-index key element is a u16 column ordinal OR the 0xFFFF sentinel + a u16 length + the canonical expression text (the Check-expression text form); on load an expression element re-parses that text (XX001 on failure, like a stored CHECK), and a GIN/GiST index with a non-column key is data_corrupted. Only the index-list changes — a plain column index is byte-identical to v6. 25 = on-disk free-list persistence (spec/fileformat/format.md; storage.md §6): meta offset 28 becomes free_list_head (0 = empty), and a page_type 7 free-list page persists the unconsumed free-list so open reads it directly instead of reconstructing it by walking every leaf; paired with continuous within-session reclamation. A from-scratch image (create/goldens) has an EMPTY free-list, so free_list_head = 0 and no page_type 7 page: every golden's only v25 change is its version byte + meta CRC. 24 = the B+tree reshape (spec/design/bplus-reshape.md, slice B1; spec/fileformat/format.md "The per-table data B+tree"): records live ONLY in leaves — an INTERIOR page (page_type 3) is a record-free routing skeleton, N+1 child pointers ‖ an N-entry end-offset separator directory ‖ the separator key blob (a separator is a COPY of a boundary key; leaf splits copy up, interior splits push up, leaf merges remove the parent separator, interior merges pull it down). A LEAF page's column regions each lead with a reserved flags byte (0 — the string-dictionary door) and take a class-determined shape: a FIXED-WIDTH column is a null bitmap (ceil(N/8), MSB-first, set = NULL) + N×width dense UNTAGGED slots (a NULL slot zero-filled); a VARIABLE-WIDTH column is an N-entry end-offset value directory + the tagged v23 codec bytes with NULL a ZERO-LENGTH SPAN (no 0x01 tag inside a leaf; the single-value codec elsewhere is unchanged). All directories become N-entry END offsets (the redundant leading 0 of the v23 N+1 prefix sums is dropped). record_size is restated as key_len + Σ value_size (fixed → its width always, variable → 0 when NULL else the tagged encoded size; the v23 phantom 2+ is dropped); RECORD_MAX keeps its v23 value (C − max(12, 12+16K))/2, re-derived leaf-only. Catalog/overflow/GiST pages are byte-identical to v23. 23 = PAX leaf layout (spec/fileformat/format.md "Leaf node"): a B-tree LEAF page (page_type 2) stores its records COLUMN-MAJOR — key directory (N+1 u32 prefix-sum) ‖ key blob ‖ column directory (K+1 u32 region offsets, colStart[K] = payload end) ‖ per column a value directory (N+1 u32 prefix-sum) then that column's N value bodies. The value codec is byte-unchanged (same 1-byte tag + body); interior pages (page_type 3) stay row-major (child pointers ‖ records). 22 = varchar(n) length limits (spec/design/types.md §15): a text column entry appends a u32 varchar_max_len in the typmod slot (type_code 4) — 0 = unbounded, 1…10485760 = the varchar(n)/string(n) limit; a composite text field carries the same u32. The value codec is unchanged (a value is checked/truncated before encoding). A file whose every text column is unbounded still moves to v22 by its version byte + a 0 on each text column/field. 21 = EXCLUDE constraints (spec/design/gist.md §7/§8, GX3): a per-table exclusion list after the foreign-key list — each entry the constraint name, its backing GiST index name, and a (column ordinal u16, operator strategy u8) element vector (&& = 0, = 1). The backing GiST index is stored like any GiST index — the index list now admits MULTI-COLUMN GiST indexes whose leaf/interior bound is the per-column component bounds concatenated (single-column GX1/GX2 bytes unchanged). A table with no exclusion still moves to v21 by its version byte + the zero count. 20 = GiST indexes (spec/design/gist.md, GX1): a per-index index_kind = 2 selects the GiST access method, and the index's on-disk form is a persisted R-tree of bounding-predicate nodes — two new page types 5 (GiST leaf) / 6 (GiST interior). A leaf entry is bound_len(u16) ‖ encode_range_body(bound) ‖ skey_len(u16) ‖ skey; an interior entry is bound_len(u16) ‖ encode_range_body(union) ‖ child_page(u32). The catalog index entry is unchanged (index_root_page points at the R-tree root, 0 for empty); a file with no GiST index still moves to v20 only by its version byte. 19 = storable json/jsonb columns (spec/design/json.md, J1/J1b): a column type can be json (type_code 18) or jsonb (type_code 19) — plain scalar catalog entries with no extra descriptor (the has_jsonb_dict door §3.2 stays clear, zero bytes). A json value's body is the verbatim text, length-prefixed like text (§4); a jsonb value's body is the self-delimiting tagged-node tree (§2 — node tags + LEB128 varint counts, numbers as the decimal body), riding the large-value overflow + LZ4 path. No catalog-shape change, so a file with no json/jsonb column still moves to v19 only by its version byte. 18 = reference-only collations: the catalog entry_kind 3 collation entry is metadata ONLY — a flags byte bit0 is_default, then name + unicode_version + cldr_version + description (each u16-len + UTF-8) — emitted after sequences and before tables; the compiled table is NOT in the file, it is vendored into the binary and resolved by name on open, spec/design/collation.md §2/§5/§9. This supersedes v17's baked snapshot (the LZ4-compressed .coll artifact is gone). The per-column collation is unchanged (column flags byte bit6 has_collation + a trailing name). 17 = baked collations (superseded). 16 = range columns: type_code 17 + an inline element-type descriptor in the catalog — one scalar code, spec/design/ranges.md §3 — and the compact range value body, a flags byte EMPTY/LB_INF/UB_INF/LB_INC/UB_INC + present bound bodies, §4). 15 = IDENTITY columns: the column-entry flags byte gains bit4 is_identity + bit5 identity_always; an identity column desugars like serial plus those two bits, spec/design/sequences.md §13. 14 = the serial owned-sequence link: the sequence-entry flags byte gains a has_owner bit + a trailing owner table-name/column-ordinal, spec/design/sequences.md §12. 13 = GIN inverted indexes: each catalog index entry gains a one-byte index_kind (0 = ordered B-tree, 1 = GIN) between index_flags and index_root_page, spec/design/gin.md. 12 = sequences: an entry_kind = 2 catalog entry — name + six i64 fields + a flags byte — emitted after composite-type entries and before table entries, spec/design/sequences.md §3, plus the date scalar. 11 = FOREIGN KEY constraints: a per-table catalog foreign-key list after the index list, spec/design/constraints.md §6. 10 = array (T[]) columns: type_code 15 + an element-type descriptor in the catalog, spec/design/array.md §3, and the compact array value body, §4. 9 = composite (row) types; 8 = per-column expression-default flag; 7 = per-page crc32. Each bump is atomic across Rust/Go/TS + the Ruby golden reference (every .jed golden's version byte + CRC changed together).
	pageHeader              = 16    // bytes of the catalog/B-tree/overflow page header (v7: 12-byte v6 header + a 4-byte per-page crc32 at offset 12)
	recordMaxReserve        = 12    // bytes reserved inside RECORD_MAX beyond the per-column term — independent of pageHeader (format.md "Why the record cap"). Historically the two-key interior node's 3 child pointers (4·3); since v24 the value is kept as the K = 0 floor of the leaf-only re-derivation (a two-record index leaf is exactly 2·(C−12)/2 + 4·2 + 4 = C)
	pageCatalog      byte   = 1     // page_type for a catalog page
	pageLeaf         byte   = 2     // page_type for a B-tree leaf node
	pageInterior     byte   = 3     // page_type for a B-tree interior node
	pageOverflow     byte   = 4     // page_type for an out-of-line value slab (large-values.md §12)
	pageFreelist     byte   = 7     // page_type for a persisted free-list page (v25 — item_count u32 free page indices, chained by next_page; spec/fileformat/format.md *Free-list page*)
	rootPage         uint32 = 2     // catalog root of a fresh empty db (relocatable thereafter)
	minPageSize             = 256   // smallest valid page size; chosen floor above the structural min pageHeader+36=52 (format.md *Page model*)
	maxPageSize             = 65536 // largest valid page size, 64 KiB (format.md *Page model*; CLAUDE.md §13)

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
func typeCodeForScalar(ty scalarType) byte {
	switch ty {
	case scalarInt16:
		return 1
	case scalarInt32:
		return 2
	case scalarInt64:
		return 3
	case scalarText:
		return 4
	case scalarBool:
		return 5
	case scalarDecimal:
		return 6
	case scalarBytea:
		return 7
	case scalarUuid:
		return 8
	case scalarTimestamp:
		return 9
	case scalarTimestamptz:
		return 10
	case scalarInterval:
		return 11
	case scalarFloat64:
		return 12
	case scalarFloat32:
		return 13
	case scalarDate:
		return 16
	// 14 (composite) / 15 (array) / 17 (range) are container element-type codes, not scalars.
	case scalarJson:
		return 18
	case scalarJsonb:
		return 19
	// jsonpath reserves type code 20, but is literal-only this slice (no storable column), so this
	// code is never written to disk yet — a storable jsonpath column is a P1a follow-on.
	case scalarJsonPath:
		return 20
	default:
		return 0
	}
}

// pushArrayElementType appends an array column's element type descriptor (spec/design/array.md §3):
// the element's type code, then (for a composite element) its name. v1 element types are scalars;
// a composite element is handled for forward-compat, a nested array element is rejected
// (multidimensionality is a value property, not array-of-array — §2).
func pushArrayElementType(out []byte, elem dataType) []byte {
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
func readArrayElementType(buf []byte, pos *int) (dataType, error) {
	code, err := readU8(buf, pos)
	if err != nil {
		return dataType{}, err
	}
	if code == 14 {
		name, err := readString(buf, pos)
		if err != nil {
			return dataType{}, err
		}
		return compositeT(name), nil
	}
	s, ok := scalarForTypeCode(code)
	if !ok {
		return dataType{}, newError(DataCorrupted, "invalid array element code")
	}
	return scalarT(s), nil
}

// pushRangeElementType appends a range column's element type descriptor (spec/design/ranges.md §3): a
// single u8 scalar type code. A range element is always one of the six scalar subtypes (i32/i64/
// decimal/timestamp/timestamptz/date) — never composite, array, or nested range — and numrange's
// element is the unconstrained decimal, so no typmod is stored (the type name fully determines the
// element). The element descriptor is self-describing: it identifies which of the six ranges the
// column is.
func pushRangeElementType(out []byte, elem dataType) []byte {
	if elem.Comp != nil || elem.Array != nil || elem.Range != nil {
		panic("a range element is always a scalar subtype (ranges.md §2)")
	}
	return append(out, typeCodeForScalar(elem.Scalar))
}

// readRangeElementType decodes a range column's element type descriptor (inverse of
// pushRangeElementType): one scalar code, validated to be one of the six range element subtypes
// (else XX001).
func readRangeElementType(buf []byte, pos *int) (dataType, error) {
	code, err := readU8(buf, pos)
	if err != nil {
		return dataType{}, err
	}
	s, ok := scalarForTypeCode(code)
	if !ok {
		return dataType{}, newError(DataCorrupted, "invalid range element code")
	}
	if _, ok := rangeForElement(s); !ok {
		return dataType{}, newError(DataCorrupted, "type code is not a valid range element subtype")
	}
	return scalarT(s), nil
}

// scalarForTypeCode is the inverse of typeCodeForScalar; ok=false for an unknown code.
func scalarForTypeCode(code byte) (scalarType, bool) {
	switch code {
	case 1:
		return scalarInt16, true
	case 2:
		return scalarInt32, true
	case 3:
		return scalarInt64, true
	case 4:
		return scalarText, true
	case 5:
		return scalarBool, true
	case 6:
		return scalarDecimal, true
	case 7:
		return scalarBytea, true
	case 8:
		return scalarUuid, true
	case 9:
		return scalarTimestamp, true
	case 10:
		return scalarTimestamptz, true
	case 11:
		return scalarInterval, true
	case 12:
		return scalarFloat64, true
	case 13:
		return scalarFloat32, true
	case 16:
		return scalarDate, true
	case 18:
		return scalarJson, true
	case 19:
		return scalarJsonb, true
	// jsonpath reserves code 20 (non-storable this slice, so never actually decoded off disk).
	case 20:
		return scalarJsonPath, true
	default:
		return 0, false
	}
}

// crc32IEEE is CRC-32/IEEE (reflected, poly 0xEDB88320, init/final 0xFFFFFFFF) — the
// standard zlib CRC32. The standard library runtime-dispatches to accelerated implementations where
// available and retains a portable fallback. Pinned by crc32("123456789") == 0xCBF43926.
func crc32IEEE(data []byte) uint32 {
	return crc32.ChecksumIEEE(data)
}

// pageCRC is the per-page checksum (v7, format.md *Page header*): CRC-32/IEEE over a body page's
// bytes EXCLUDING its own 4-byte crc32 field at [12,16) — i.e. [0,12) then [16,pageSize), covering
// the header, payload, and zero-fill tail. makePage writes it; parsePage re-verifies it (mismatch →
// XX001). page is one full page (pageSize bytes).
func pageCRC(page []byte) uint32 {
	checksum := crc32.Update(0, crc32.IEEETable, page[0:12])
	return crc32.Update(checksum, crc32.IEEETable, page[pageHeader:])
}

// encodeValue is the value codec (format.md): a 1-byte presence tag (0x01 = NULL), then the type's
// present-value body. A scalar dispatches to encodeScalar; a COMPOSITE (spec/design/composite.md §4)
// is the shared presence tag then a body of `null-bitmap ‖ each present field's value-codec body`
// (no per-field tag — the bitmap carries presence), recursing for nested composites.
func encodeValue(ty colType, v Value) []byte {
	if ty.Elem != nil {
		// An array column (spec/design/array.md §4): the shared presence tag then the array body.
		if v.Kind == ValNull {
			return []byte{0x01}
		}
		if v.Kind != ValArray {
			panic("BUG: a non-array value in an array column")
		}
		out := []byte{0x00} // present
		return append(out, encodeArrayBody(*ty.Elem, v.arrayVal())...)
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
		return append(out, encodeRangeBody(*ty.RangeElem, v.rangeVal())...)
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
	return append(out, encodeCompositeBody(ty.Fields, *v.composite())...)
}

// encodeArrayBody is an array value's body (after the 0x00 present tag, spec/design/array.md §4):
// ndim u8, flags u8, per-dim (len u32 BE, lb i32 BE), then the optional null bitmap (present iff
// HAS_NULLS) and the present element bodies (row-major). An empty array is ndim 0; otherwise ndim is
// the dimension count and each dimension records its length and lower bound (multidim + custom lower
// bounds — spec/design/array.md §12). The bitmap (MSB-first, like composite) is present iff any
// element is NULL; a NULL element contributes zero body bytes.
func encodeArrayBody(elem colType, a *ArrayVal) []byte {
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
func encodeRangeBody(elem colType, rv *RangeVal) []byte {
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
func encodeCompositeBody(fields []colField, vals []Value) []byte {
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
// --- jsonb value codec (the tagged-node tree, spec/design/json.md §2) -------------------------
//
// A jsonb value's BODY (after the 0x00 present tag) is a self-delimiting depth-first serialization
// of the canonical node tree: every node leads with a one-byte tag (low nibble = kind, high nibble
// = flags, reserved 0). Like array/range, there is NO outer length prefix — the tree walks itself,
// so a large jsonb body rides the large-value overflow + LZ4 path opaquely (§2). Counts / string
// lengths are an unsigned LEB128 varint (§2.1). A json value's body is the text VERBATIM,
// length-prefixed exactly like text (§4).
const (
	ntagNull       byte = 0x0
	ntagFalse      byte = 0x1
	ntagTrue       byte = 0x2
	ntagNumber     byte = 0x3
	ntagString     byte = 0x4
	ntagStringDict byte = 0x5 // reserved — the dictionary door (§3); a reader rejects it XX001
	ntagArray      byte = 0x6
	ntagObject     byte = 0x7
)

// writeUvarint appends an unsigned LEB128 varint (7 bits/byte, high bit = continuation) — the
// count/length codec for the jsonb node bodies (spec/design/json.md §2.1).
func writeUvarint(out []byte, v uint64) []byte {
	for {
		b := byte(v & 0x7f)
		v >>= 7
		if v == 0 {
			return append(out, b)
		}
		out = append(out, b|0x80)
	}
}

// readUvarint reads an unsigned LEB128 varint (inverse of writeUvarint). XX001 on a truncated or
// over-64-bit value.
func readUvarint(buf []byte, pos *int) (uint64, error) {
	var result uint64
	var shift uint32
	for {
		b, err := readU8(buf, pos)
		if err != nil {
			return 0, err
		}
		if shift >= 64 || (shift == 63 && b > 1) {
			return 0, newError(DataCorrupted, "jsonb varint overflows u64")
		}
		result |= uint64(b&0x7f) << shift
		if b&0x80 == 0 {
			return result, nil
		}
		shift += 7
	}
}

// encodeDecimalBody appends a decimal value's BODY (no presence tag): flags(sign) ‖ u16 scale ‖
// u16 ndigits ‖ groups (base-10^4, MS-first) — the ntagNumber payload and the inverse of
// decodeDecimalBody.
func encodeDecimalBody(d Decimal, out []byte) []byte {
	neg, scale, groups := d.ToCodec()
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
}

// encodeJsonbBody serializes a jsonb node tree into out (the body bytes — spec/design/json.md §2.1).
// Object members are already in canonical key order (the canonicalizer's invariant); each member's
// key is itself a string node (ntagString), so the dictionary door covers keys and values uniformly.
func encodeJsonbBody(node *JsonNode, out []byte) []byte {
	switch node.Kind {
	case JNull:
		return append(out, ntagNull)
	case JBool:
		if node.B {
			return append(out, ntagTrue)
		}
		return append(out, ntagFalse)
	case JNumber:
		out = append(out, ntagNumber)
		return encodeDecimalBody(node.Num, out)
	case JString:
		out = append(out, ntagString)
		out = writeUvarint(out, uint64(len(node.S)))
		return append(out, node.S...)
	case JArray:
		out = append(out, ntagArray)
		out = writeUvarint(out, uint64(len(node.Arr)))
		for i := range node.Arr {
			out = encodeJsonbBody(&node.Arr[i], out)
		}
		return out
	case JObject:
		out = append(out, ntagObject)
		out = writeUvarint(out, uint64(len(node.Obj)))
		for i := range node.Obj {
			out = append(out, ntagString)
			out = writeUvarint(out, uint64(len(node.Obj[i].Key)))
			out = append(out, node.Obj[i].Key...)
			out = encodeJsonbBody(&node.Obj[i].Val, out)
		}
		return out
	default:
		panic("BUG: unknown jsonb node kind in encodeJsonbBody")
	}
}

// decodeJsonbBody deserializes a jsonb node from buf at pos (inverse of encodeJsonbBody). A nonzero
// flag nibble, the reserved ntagStringDict (no dictionary slice yet), or an unknown kind is XX001
// data_corrupted (spec/design/json.md §3.1/§6.3).
func decodeJsonbBody(buf []byte, pos *int, mode decodeMode) (JsonNode, error) {
	tag, err := readU8(buf, pos)
	if err != nil {
		return JsonNode{}, err
	}
	if tag&0xf0 != 0 {
		return JsonNode{}, newError(DataCorrupted, "jsonb node tag has a reserved flag bit set")
	}
	switch tag & 0x0f {
	case ntagNull:
		return JsonNode{Kind: JNull}, nil
	case ntagFalse:
		return JsonNode{Kind: JBool, B: false}, nil
	case ntagTrue:
		return JsonNode{Kind: JBool, B: true}, nil
	case ntagNumber:
		dv, err := decodeDecimalBody(buf, pos, mode)
		if err != nil {
			return JsonNode{}, err
		}
		if !mode.constructs() {
			return JsonNode{}, nil // skip-mode placeholder (decimal body advanced, not built)
		}
		return JsonNode{Kind: JNumber, Num: *dv.decimal()}, nil
	case ntagString:
		s, err := decodeJsonbString(buf, pos, mode)
		if err != nil {
			return JsonNode{}, err
		}
		if !mode.constructs() {
			return JsonNode{}, nil // skip-mode placeholder
		}
		return JsonNode{Kind: JString, S: s}, nil
	case ntagStringDict:
		return JsonNode{}, newError(DataCorrupted, "jsonb string-dictionary reference before the dictionary slice")
	case ntagArray:
		count, err := readUvarint(buf, pos)
		if err != nil {
			return JsonNode{}, err
		}
		var elems []JsonNode
		if mode.constructs() {
			elems = make([]JsonNode, 0, minCap(count))
		}
		for i := uint64(0); i < count; i++ {
			child, err := decodeJsonbBody(buf, pos, mode)
			if err != nil {
				return JsonNode{}, err
			}
			if mode.constructs() {
				elems = append(elems, child)
			}
		}
		return JsonNode{Kind: JArray, Arr: elems}, nil
	case ntagObject:
		count, err := readUvarint(buf, pos)
		if err != nil {
			return JsonNode{}, err
		}
		var members []JsonMember
		if mode.constructs() {
			members = make([]JsonMember, 0, minCap(count))
		}
		for i := uint64(0); i < count; i++ {
			// Each member's key is a string node (ntagString / reserved ntagStringDict).
			ktag, err := readU8(buf, pos)
			if err != nil {
				return JsonNode{}, err
			}
			if ktag&0xf0 != 0 {
				return JsonNode{}, newError(DataCorrupted, "jsonb object key tag has a reserved flag bit set")
			}
			switch ktag & 0x0f {
			case ntagString:
				// fall through to read the key payload below
			case ntagStringDict:
				return JsonNode{}, newError(DataCorrupted, "jsonb string-dictionary reference before the dictionary slice")
			default:
				return JsonNode{}, newError(DataCorrupted, "jsonb object key is not a string node")
			}
			key, err := decodeJsonbString(buf, pos, mode)
			if err != nil {
				return JsonNode{}, err
			}
			val, err := decodeJsonbBody(buf, pos, mode)
			if err != nil {
				return JsonNode{}, err
			}
			if mode.constructs() {
				members = append(members, JsonMember{Key: key, Val: val})
			}
		}
		return JsonNode{Kind: JObject, Obj: members}, nil
	default:
		return JsonNode{}, newError(DataCorrupted, "unknown jsonb node tag")
	}
}

// minCap bounds a decoded count's preallocation (the Rust .min(1024) guard) so a corrupt huge count
// cannot force a giant allocation before the bytes are read.
func minCap(count uint64) int {
	if count > 1024 {
		return 1024
	}
	return int(count)
}

// decodeJsonbString reads a ntagString payload (varint len ‖ UTF-8 bytes) after its tag has been
// consumed.
func decodeJsonbString(buf []byte, pos *int, mode decodeMode) (string, error) {
	n, err := readUvarint(buf, pos)
	if err != nil {
		return "", err
	}
	sb, err := take(buf, pos, int(n))
	if err != nil {
		return "", err
	}
	if !mode.constructs() {
		return "", nil // skip: advance past the bytes, no copy / no UTF-8 validation
	}
	if !utf8.Valid(sb) {
		return "", newError(DataCorrupted, "non-UTF-8 jsonb string")
	}
	return string(sb), nil
}

func encodeScalar(ty scalarType, v Value) []byte {
	switch v.Kind {
	case ValNull:
		return encodeNullable(ty, nil)
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
	case ValJsonPath:
		// jsonpath is literal-only (non-storable), so a value never reaches the codec.
		panic("BUG: a jsonpath value reached the scalar codec")
	case ValJson:
		// json: the verbatim text body, length-prefixed exactly like text (spec/design/json.md §4).
		out := make([]byte, 0, 3+len(v.str()))
		out = append(out, 0x00) // present
		out = appendU16(out, uint16(len(v.str())))
		return append(out, v.str()...)
	case ValJsonb:
		// jsonb: present tag, then the self-delimiting tagged-node tree (spec/design/json.md §2).
		out := []byte{0x00} // present
		return encodeJsonbBody(v.jsonb(), out)
	case ValText, ValBytea:
		// text (UTF-8) and bytea (raw bytes) share the compact length-prefixed body; both
		// hold their bytes in Str, so the on-disk form is identical.
		out := make([]byte, 0, 3+len(v.str()))
		out = append(out, 0x00) // present
		out = appendU16(out, uint16(len(v.str())))
		return append(out, v.str()...)
	case ValUuid:
		// Fixed 16-byte body, NO length prefix (the first fixed-width non-integer value) —
		// spec/fileformat/format.md. The 16 raw bytes live in Str.
		out := make([]byte, 0, 1+16)
		out = append(out, 0x00) // present
		return append(out, v.str()...)
	case ValBool:
		b := byte(0x00)
		if v.boolVal() {
			b = 0x01
		}
		return []byte{0x00, b} // present tag + bool-byte (0x00 false, 0x01 true)
	case ValDecimal:
		// Decimal value codec (spec/fileformat/format.md): tag, flags (sign), u16 scale, u16
		// ndigits, then that many big-endian base-10^4 coefficient groups (MS-first).
		neg, scale, groups := v.decimal().ToCodec()
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
		out = appendU32(out, uint32(v.interval().Months))
		out = appendU32(out, uint32(v.interval().Days))
		m := uint64(v.interval().Micros)
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
		return encodeNullable(ty, &n)
	}
}

func appendU16(b []byte, v uint16) []byte { return append(b, byte(v>>8), byte(v)) }

// appendString writes a u16-length-prefixed UTF-8 string (the catalog's name/string encoding).
func appendString(b []byte, s string) []byte {
	b = appendU16(b, uint16(len(s)))
	return append(b, s...)
}

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
// callers/tests holding a *Engine.)
func (db *engine) ToImage(pageSize uint32, txid uint64) ([]byte, error) {
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
func (s *snapshot) ToImage(pageSize uint32, txid uint64) ([]byte, error) {
	ps := int(pageSize)
	if ps < minPageSize {
		return nil, newError(FeatureNotSupported, "page size too small for the format")
	}
	if ps > maxPageSize {
		return nil, newError(FeatureNotSupported, "page size too large for the format")
	}
	if ps&(ps-1) != 0 {
		return nil, newError(FeatureNotSupported, "page size must be a power of two")
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
	nextIndex := rootPage
	for ti, k := range keys {
		if root := s.stores[k].treeRoot(); root != nil {
			rp, np, err := serializeNode(root, s.stores[k], capacity, nextIndex, &body)
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
			if idx.Kind == indexGist {
				// GiST: the on-disk form is the R-tree (pages 5/6), not the flat leaf store (gist.md
				// §4.1). Serialize the canonical tree, allocating from the same counter.
				gpages, root, err := serializeGistIndex(s, s.tables[k], idx, func() uint32 { p := nextIndex; nextIndex++; return p })
				if err != nil {
					return nil, err
				}
				for _, p := range gpages {
					body = append(body, bodyPage{index: p.pageNo, pageType: p.pageType, itemCount: p.itemCount, nextPage: 0, payload: p.payload})
				}
				r = root
			} else if istore := s.indexStores[strings.ToLower(idx.Name)]; istore.treeRoot() != nil {
				rp, np, err := serializeNode(istore.treeRoot(), istore, capacity, nextIndex, &body)
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
	// Collation reference entries (kind 3, v18) — after sequences, before tables, so a collated table
	// entry is read after the entry it references. Reference-only: emit one metadata entry per
	// collation the SCHEMA references (columns + default), not an imported set
	// (spec/design/collation.md §2/§5).
	refColls, err := s.referencedCollations()
	if err != nil {
		return nil, err
	}
	for _, c := range refColls {
		catEntries = append(catEntries, append([]byte{3}, collationEntryBytes(c, s.defaultCollation == c.Name)...))
	}
	for ti, k := range keys {
		rowCount, known := s.stores[k].Count()
		if !known {
			panic("table stores always carry an exact row count")
		}
		catEntries = append(catEntries, append([]byte{0}, tableEntryBytes(s.tables[k], rootDataPage[ti], indexRoots[ti], rowCount)...))
	}
	catEntries = append(catEntries, statisticsCatalogEntries(s)...)
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

// serializeGistIndex builds a GiST index's canonical R-tree from its leaf-key store and serializes it
// to node pages (spec/design/gist.md §3/§4.1). The on-disk form of a GiST index is the R-tree (page
// types 5/6), NOT the flat leaf-key B-tree the in-memory index store holds — so the index store is
// never serialized for a GiST index; this is. The tree is rebuilt CANONICALLY from the leaf set, so
// its bytes are a pure function of the set — content-deterministic and cross-core identical (§3).
// alloc hands out page numbers (a counter for the whole image, the free-list allocator for an
// incremental commit). Returns the node pages + the root; an empty index returns no pages and root 0.
func serializeGistIndex(s *snapshot, table *catTable, idx indexDef, alloc func() uint32) ([]gistPage, uint32, error) {
	// One opclass per indexed column (gist.md §7): single for a GX1/GX2 index, one per WITH column
	// for an EXCLUDE backing index. A GiST index is always plain-column (columnOrdinals non-nil).
	ops := gistOpclassesFor(idx.columnOrdinals(), table.Columns)
	var keys [][]byte
	if istore := s.indexStores[strings.ToLower(idx.Name)]; istore != nil {
		entries, err := istore.EntriesInKeyOrder()
		if err != nil {
			return nil, 0, err
		}
		keys = make([][]byte, len(entries))
		for i, e := range entries {
			keys[i] = e.Key
		}
	}
	if len(keys) == 0 {
		return nil, 0, nil
	}
	tree, err := buildGistFromLeafKeys(ops, keys)
	if err != nil {
		return nil, 0, err
	}
	pages, root := serializeGistTree(tree, ops, alloc)
	return pages, root, nil
}

// serializeNode serializes one node and its subtree post-order, appending each to *body, and returns
// this node's assigned page index and the next free index. A leaf's payload is its records; an
// interior's is its N+1 child pointers (big-endian u32) then its N records (format.md). A node whose
// payload would exceed the page is an oversized record (over RECORD_MAX) — feature_not_supported.
func serializeNode(n *pnode, store *tableStore, capacity int, nextIndex uint32, body *[]bodyPage) (uint32, uint32, error) {
	colTypes := store.colTypes
	childPages := make([]uint32, len(n.children))
	for i, c := range n.children {
		// Whole-image serialize renumbers pages from scratch. Under B3 (bplus-reshape.md) every
		// database — in-memory included — is demand-paged, so a clean leaf may be an OnDisk
		// reference into the source store: fault it through the store's pool for the duration of
		// its own serialization (whole-image serialize is not a hot path).
		child := c.node
		if child == nil {
			faulted, err := store.faultLeaf(c.page)
			if err != nil {
				return 0, 0, err
			}
			child = faulted
		}
		cp, np, err := serializeNode(child, store, capacity, nextIndex, body)
		if err != nil {
			return 0, 0, err
		}
		childPages[i] = cp
		nextIndex = np
	}
	index := nextIndex
	nextIndex++

	// Encode a leaf's records, spilling over-large values to overflow pages allocated after this
	// node's index (post-order traversal + record-then-column order → deterministic, golden-pinnable
	// layout; only LEAVES allocate chains in v24). A LEAF is column-major (PAX — encodeLeafPAX); an
	// INTERIOR node is the record-free keys+children skeleton (encodeInterior), format.md.
	var ovf []overflowPageOut
	take := func() uint32 { p := nextIndex; nextIndex++; return p }
	var payload []byte
	pageType := pageLeaf
	if len(n.children) > 0 {
		pageType = pageInterior
		payload = encodeInterior(n.keys, childPages)
	} else {
		// A leaf may be Packed here: a demand-paged load keeps leaves as page blocks and toImage
		// re-serializes them. Materialize through the seam (decodedRows reconstructs a Packed leaf,
		// clones a Decoded one), then resolve any lazily-deferred large values through the store's
		// pager (large-values.md §14) so encode sees resident bytes. Whole-image serialize is not a
		// hot path (create's empty image / golden generator / toImage canonical), so the clones are
		// acceptable (packed-leaf.md §7).
		rows, err := n.decodedRows()
		if err != nil {
			return 0, 0, err
		}
		for ri := range rows {
			resolved, err := store.resolveAll(rows[ri])
			if err != nil {
				return 0, 0, err
			}
			rows[ri] = resolved
		}
		payload = encodeLeafPAX(colTypes, n.keyViews(), rows, capacity, take, &ovf)
	}
	if len(payload) > capacity {
		return 0, 0, newError(FeatureNotSupported, "a record larger than the per-row limit is not supported")
	}
	*body = append(*body, bodyPage{index: index, pageType: pageType, itemCount: uint32(n.keyLen()), payload: payload})
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
	// freeRemaining is the free-list entries this commit did not consume by its tree/catalog pages —
	// all pages dead at the fallback (prior) snapshot, so safe to overwrite this commit. The durable
	// path draws its persisted page_type 7 free-list pages from these (never the high-water) and
	// reclaims this commit's fresh orphans into the persisted list too (serializeFreeList / planFreeList).
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
	// reuse gates whether this commit draws from the free-list. false ⇒ allocate high-water only,
	// leaving the whole free-list unconsumed (cursor stays 0, so freeRemaining carries it all through
	// for persistence): the reader-liveness watermark defers reusing a page a still-open reader on an
	// older snapshot could observe (transactions.md §8 — the free-list generation gate). Reconstruct-on-open
	// and the single-handle case leave it true (oldest_live == committed ⇒ no page is still observed),
	// so the on-disk byte layout is unchanged whenever no reader pins an older version.
	reuse bool
}

func (a *pageAlloc) take() uint32 {
	if a.reuse && a.cursor < len(a.free) {
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
func (s *snapshot) incrementalImage(pageSize, startPage uint32, free []uint32, reuse bool, paging *sharedPaging) (incrementalWrite, error) {
	ps := int(pageSize)
	capacity := ps - pageHeader

	keys := make([]string, 0, len(s.tables))
	for k := range s.tables {
		keys = append(keys, k)
	}
	sort.Strings(keys)

	// Allocate from the free-list first (reclaiming dead pages), then extend the file — unless the
	// watermark defers reuse (reuse false), in which case only the high-water is drawn and the whole
	// free-list carries through unconsumed for persistence (pageAlloc.reuse, transactions.md §8).
	alloc := &pageAlloc{free: free, next: startPage, reuse: reuse}

	var pages []dirtyPage
	rootDataPage := make([]uint32, len(keys))
	indexRoots := make([][]uint32, len(keys))
	var indexColTypes []colType
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
			if idx.Kind == indexGist {
				// GiST rewrites its WHOLE R-tree every commit (gist.md §4.1(b)): fresh pages from the
				// allocator (free-list first), the old tree's pages reclaimed on the next open.
				gpages, root, err := serializeGistIndex(s, s.tables[k], idx, alloc.take)
				if err != nil {
					return incrementalWrite{}, err
				}
				for _, p := range gpages {
					pages = append(pages, dirtyPage{index: p.pageNo, bytes: makePage(ps, p.pageType, p.itemCount, 0, p.payload)})
				}
				r = root
			} else if root := s.indexStores[strings.ToLower(idx.Name)].treeRoot(); root != nil {
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
	// Collation reference entries (kind 3, v18) — after sequences, before tables, so a collated table
	// entry is read after the entry it references. Reference-only: emit one metadata entry per
	// collation the SCHEMA references (columns + default), not an imported set
	// (spec/design/collation.md §2/§5).
	refColls, err := s.referencedCollations()
	if err != nil {
		return incrementalWrite{}, err
	}
	for _, c := range refColls {
		catEntries = append(catEntries, append([]byte{3}, collationEntryBytes(c, s.defaultCollation == c.Name)...))
	}
	for ti, k := range keys {
		rowCount, known := s.stores[k].Count()
		if !known {
			panic("table stores always carry an exact row count")
		}
		catEntries = append(catEntries, append([]byte{0}, tableEntryBytes(s.tables[k], rootDataPage[ti], indexRoots[ti], rowCount)...))
	}
	catEntries = append(catEntries, statisticsCatalogEntries(s)...)
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
func resolveForEncode(row storedRow, colTypes []colType, paging *sharedPaging) (storedRow, error) {
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
		return nil, newError(DataCorrupted, "unfetched large value with no pager at commit")
	}
	fetch := func(p uint32) ([]byte, error) { return paging.readBlock(p) }
	out := make(storedRow, len(row))
	copy(out, row)
	for i := range out {
		if out[i].Kind == ValUnfetched {
			v, err := resolveUnfetched(colTypes[i], out[i].unfetched(), fetch)
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
func serializeDirty(n *pnode, colTypes []colType, capacity, ps int, alloc *pageAlloc, pages *[]dirtyPage, paging *sharedPaging) (uint32, error) {
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
	// Encode a leaf's records, spilling over-large values to overflow pages drawn from the same
	// allocator (free-list first, then high-water — large-values.md §12). A dirty leaf may carry
	// rows the lazy load left unfetched (a sibling row's mutation dirtied them): resolve those
	// through the pager first — unmetered commit work, large-values.md §14 — so the re-encode
	// re-plans the resident row exactly as an eager writer would (chains are rewritten fresh;
	// sharing an unchanged chain is the deferred byte-layout follow-on). An INTERIOR node is the
	// record-free keys+children skeleton (v24) — no values, no chains.
	var ovf []overflowPageOut
	var payload []byte
	pageType := pageLeaf
	if len(n.children) > 0 {
		pageType = pageInterior
		payload = encodeInterior(n.keys, childPages)
	} else {
		rows := make([]storedRow, len(n.keys))
		for i := range n.keys {
			row, err := resolveForEncode(n.vals[i], colTypes, paging)
			if err != nil {
				return 0, err
			}
			rows[i] = row
		}
		payload = encodeLeafPAX(colTypes, n.keys, rows, capacity, alloc.take, &ovf)
	}
	if len(payload) > capacity {
		return 0, newError(FeatureNotSupported, "a record larger than the per-row limit is not supported")
	}
	index := alloc.take()
	n.page = index
	*pages = append(*pages, dirtyPage{index: index, bytes: makePage(ps, pageType, uint32(len(n.keys)), 0, payload)})
	for _, o := range ovf {
		*pages = append(*pages, dirtyPage{index: o.index, bytes: makePage(ps, pageOverflow, o.itemCount, o.nextPage, o.payload)})
	}
	return index, nil
}

// newTempStorage builds a fresh per-domain storage identity for a TEMP snapshot (temp-tables.md §6,
// bplus-reshape.md): a private in-RAM memoryBlockStore read/written through the SAME pager + packed-leaf
// path as an in-memory database, with a PINNED (unbounded) pool — a temp domain is resident by
// definition (§5) — and within-session compaction ON, so its copy-on-write orphans are reclaimed rather
// than leaked (a temp store is never reopened, so reconstruct-on-open never runs). It seeds the store
// with the empty from-scratch image exactly as an in-memory database does (newInMemoryWithPageSize), so
// pagerFromStore reads the page size and pageCount starts past the meta slots. Zero file writes: this
// byte store is entirely separate from the main database file.
func newTempStorage(pageSize uint32) *storage {
	image, err := newSnapshot().ToImage(pageSize, 0)
	if err != nil {
		panic("a fresh temp image always serializes: " + err.Error())
	}
	p, err := pagerFromStore(newMemoryBlockStore(image))
	if err != nil {
		panic("a fresh temp image always opens: " + err.Error())
	}
	mt, _ := parseMeta(image[:pageSize])
	return &storage{
		pageSize:             pageSize,
		pageCount:            mt.pageCount,
		paging:               newSharedPaging(p, math.MaxInt), // pinned/unbounded, mirroring an in-memory database
		reclaimWithinSession: true,
	}
}

// newAttachedStorage builds a fresh, empty in-RAM storage identity for a host-attached DATABASE-scoped
// in-memory database (spec/design/attached-databases.md §6) — the same recipe as newTempStorage (a
// memoryBlockStore seeded with the empty from-scratch image, a pinned/unbounded pool, within-session
// compaction on), differing only in that its root is DATABASE-scoped (published into roots.attached and
// pinned by the cross-session watermark) rather than session-private. In Slice 1b every attachment is
// in-memory; a file-backed attachment (Slice 2) would open a fileBlockStore here instead.
func newAttachedStorage(pageSize uint32) *storage {
	return newTempStorage(pageSize)
}

// LoadEngine reconstructs a database from an on-disk image (inverse of ToImage). Returns a
// structured data_corrupted (XX001) error for malformed input.
//
// B3 (bplus-reshape.md): the image becomes the engine's byte store — a memoryBlockStore read
// through the SAME demand-paged loader, pager, and Packed leaf path as a file (one read path; the
// eager whole-image readTree loader is gone). The pool is pinned (unbounded): an in-memory
// database is resident by definition (§5), so CacheBytes bounds only file-backed eviction and the
// observable default is unchanged. Basic image validation stays up front so a malformed image is
// XX001 before any store is built.
func loadEngine(image []byte) (*engine, error) {
	if len(image) < 12 {
		return nil, newError(DataCorrupted, "image smaller than a meta header")
	}
	pageSize := int(binary.BigEndian.Uint32(image[8:12]))
	if !pageSizeValid(pageSize) || len(image) < pageSize*2 {
		return nil, newError(DataCorrupted, "invalid page size")
	}
	p, err := pagerFromStore(newMemoryBlockStore(image))
	if err != nil {
		return nil, err
	}
	return loadEnginePaged(p, math.MaxInt)
}

// LoadEnginePaged opens a file-backed database demand-paged (spec/design/pager.md, P6.4b): it loads
// only the interior B-tree skeleton resident, leaving each leaf an OnDisk page faulted through the
// bounded buffer pool on access — so the resident set is bounded by the pool, not the file size. The
// inverse of an incremental commit, reading pages through pgr instead of a whole image.
//
// This slice reads every leaf page once (to count its rows for length and mark it reachable for the
// free-list), then discards it — memory stays bounded (only the skeleton is retained), but open is
// O(pages). Making open O(skeleton) needs a per-subtree row count in the format (a deferred follow-on,
// pager.md §6); the residency win — a bounded resident set — already holds.
func loadEnginePaged(pgr *pager, capacity int) (*engine, error) {
	pageSize := int(pgr.pageSize)
	if !pageSizeValid(pageSize) {
		return nil, newError(DataCorrupted, "invalid page size")
	}
	paging := newSharedPaging(pgr, capacity)
	return loadEngineSharedPaging(paging)
}

// loadEngineSharedPaging reloads the newest committed root through an existing pager/pool while a
// shared-file participant holds commit SH. Co-resident body pages are append-only, so cached leaves
// remain valid and only the catalog/interior skeleton is rebuilt.
func loadEngineSharedPaging(paging *sharedPaging) (*engine, error) {
	pgr := paging.pgr
	pageSize := int(pgr.pageSize)
	if !pageSizeValid(pageSize) {
		return nil, newError(DataCorrupted, "invalid page size")
	}

	// Select the live meta from slots 0 and 1 (highest valid txid; the lone valid slot on a torn
	// write), read as individual blocks through the pager.
	b0, err := paging.readBlock(0)
	if err != nil {
		return nil, err
	}
	b1, err := paging.readBlock(1)
	if err != nil {
		return nil, err
	}
	mt, ok := parseMeta(b0)
	if mb, okb := parseMeta(b1); okb && (!ok || mb.txid > mt.txid) {
		mt, ok = mb, true
	}
	if !ok {
		return nil, newError(DataCorrupted, "no valid meta page")
	}

	snap := newSnapshot()
	snap.txid = mt.txid
	statisticsExpected := make(map[string][2]int)
	// v25: the free-list is read from the persisted chain (below), not reconstructed by a reachability
	// walk — so the catalog + skeleton load no longer tracks a reached set.
	catPage := mt.rootPage
	for catPage != 0 {
		block, err := paging.readBlock(catPage)
		if err != nil {
			return nil, err
		}
		pg, err := parsePage(block)
		if err != nil {
			return nil, err
		}
		if pg.pageType != pageCatalog {
			return nil, newError(DataCorrupted, "expected a catalog page")
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
			if kind == 4 {
				if err := decodeStatisticsEntry(pg.payload, &pos, snap, statisticsExpected); err != nil {
					return nil, err
				}
				continue
			}
			if kind != 0 {
				return nil, newError(DataCorrupted, "unknown catalog entry kind")
			}
			var rowCount int64
			table, tableRoot, indexRoots, err := decodeTableEntry(pg.payload, &pos, &rowCount)
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
				// Reads only the interior spine — leaves stay OnDisk; the exact row count was restored
				// from the v28 catalog entry (spec/design/storage.md §6).
				root, err := readSkeleton(paging, tableRoot, colTypes)
				if err != nil {
					return nil, err
				}
				store.setSkeleton(root, rowCount, true)
				if !hasPK {
					// No-PK rowid reconstruction faults the leaves to find the largest key; only for
					// keyless tables (most have a PK), bounded by the pool. tableRoot != 0 ⇒ non-empty.
					keys, _, err := store.rows.inorder(store.leafSrc())
					if err != nil {
						return nil, err
					}
					store.BumpRowidTo(decodeInt(scalarInt64, keys[len(keys)-1]) + 1)
				}
			}
			// The table's index trees (v5): zero-column demand-paged stores of entry keys
			// (spec/design/indexes.md §3); no spillable columns, so no overflow collection
			// is ever needed.
			for k, idx := range table.Indexes {
				istore := newTableStore(pageSize-pageHeader, nil)
				if indexRoots[k] != 0 && idx.Kind == indexGist {
					// GiST is EAGER-loaded, not demand-paged (gist.md §4.1(a)): read the whole R-tree,
					// recover its leaf keys into a fully-resident leaf store.
					var keys [][]byte
					read := func(p uint32) (byte, uint32, []byte, error) {
						block, err := paging.readBlock(p)
						if err != nil {
							return 0, 0, nil, err
						}
						pg, err := parsePage(block)
						if err != nil {
							return 0, 0, nil, err
						}
						return pg.pageType, pg.itemCount, pg.payload, nil
					}
					if err := readGistLeafKeys(read, indexRoots[k], &keys); err != nil {
						return nil, err
					}
					for _, key := range keys {
						if _, err := istore.Insert(key, nil); err != nil {
							return nil, err
						}
					}
				} else {
					istore.attachPaging(paging)
					if indexRoots[k] != 0 {
						root, err := readSkeleton(paging, indexRoots[k], nil)
						if err != nil {
							return nil, err
						}
						istore.setSkeleton(root, 0, false)
					}
				}
				snap.putIndexStore(strings.ToLower(idx.Name), istore)
			}
		}
		catPage = pg.nextPage
	}
	for groupKey, counts := range statisticsExpected {
		separator := strings.LastIndexByte(groupKey, 0)
		if separator < 0 {
			return nil, newError(DataCorrupted, "invalid statistics group key")
		}
		tableKey := groupKey[:separator]
		var column int
		if _, err := fmt.Sscanf(groupKey[separator+1:], "%d", &column); err != nil {
			return nil, newError(DataCorrupted, "invalid statistics group key")
		}
		statistics := snap.columnStatistics(tableKey, column)
		if statistics == nil || len(statistics.MCV) != counts[0] || len(statistics.Histogram) != counts[1] {
			return nil, newError(DataCorrupted, "incomplete statistics entry group")
		}
	}

	// Two-pass: validate the composite-type catalog (existence + acyclicity) — XX001 on a bad
	// reference (spec/design/composite.md §3).
	if err := snap.validateCompositeTypes(); err != nil {
		return nil, err
	}
	// Build each GiST index's resident R-tree from its eager-loaded leaf store (gist.md §4.1).
	if err := snap.rebuildGistTrees(); err != nil {
		return nil, err
	}
	db := newEngine()
	db.pageSize = uint32(pageSize)
	db.pageCount = mt.pageCount
	// v25: load the free-list directly from the persisted chain (meta offset 28) — no reachability
	// walk (spec/fileformat/format.md *Reclamation*).
	free, err := readFreeList(paging, mt.freeListHead)
	if err != nil {
		return nil, err
	}
	db.freePages = free
	// Every persisted free page is dead at the committed version (the free-list is "as of" mt.txid), so
	// its reuse generation is mt.txid: at open oldest_live == committed and any later reader pins ≥ the
	// committed version, so reuse is safe (transactions.md §8, the free-list generation gate).
	db.freeGenTxid = mt.txid
	// Seed the within-session compaction trigger with the live estimate (page_count minus the
	// free-list), so the first commit after open does not compact spuriously (planFreeList).
	if live := int(mt.pageCount) - len(db.freePages); live > 0 {
		db.liveAtCompaction = uint32(live)
	}
	db.committed = snap
	db.paging = paging
	// Stores created in a LATER session bind this same pager at creation (snapshot.storePaging), so
	// they join the post-commit residency flip like the loaded stores attached above.
	snap.storePaging = paging
	return db, nil
}

// anySpillableMasked is anySpillable restricted to the columns a query's touched set selects —
// the gate for the masked scan-units walk (cost.md §3 "The touched set"): if no TOUCHED column
// can spill, the whole walk yields zero and is skipped.
func anySpillableMasked(colTypes []colType, mask []bool) bool {
	for i, ty := range colTypes {
		if mask[i] && isSpillable(ty) {
			return true
		}
	}
	return false
}

// anySpillable reports whether any column type can spill out-of-line (large-values.md §12).
func anySpillable(colTypes []colType) bool {
	for _, ty := range colTypes {
		if isSpillable(ty) {
			return true
		}
	}
	return false
}

// collectLeafOverflow walks a table's on-disk B+tree, reading each leaf and adding the overflow chain
// pages its records reference to reached (large-values.md §12). Only leaves own chains in v24 (an
// interior node is record-free) — an interior page contributes just its child pointers to the walk.
// Used only for tables with spillable columns during the paged-open free-list reconstruction; it
// decodes each non-null variable-width span lazily and follows its chains by HEADERS only
// (chainPages — large-values.md §14), so opening a file never materializes or decompresses a large
// value.
func collectLeafOverflow(paging *sharedPaging, pageIdx uint32, colTypes []colType, reached map[uint32]bool) error {
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
		n := int(pg.itemCount)
		leaf, err := parsePaxLeaf(pg.payload, n, colTypes)
		if err != nil {
			return err
		}
		// Only variable-width (spillable) columns can own a chain; fixed-width regions are
		// skipped entirely, and a NULL (zero-length span) has no bytes to decode.
		for c, ty := range colTypes {
			if _, fixed := fixedValueWidth(ty); fixed {
				continue
			}
			for i := 0; i < n; i++ {
				if leaf.isNull(c, i) {
					continue
				}
				vb, err := leaf.value(c, i)
				if err != nil {
					return err
				}
				p := 0
				// The value is discarded right after markChains (only its chain pages matter), so
				// the paging resolution handle is deliberately dead (nil).
				v, err := readValueLazy(colTypes, c, vb, &p, nil)
				if err != nil {
					return err
				}
				if err := markChains(storedRow{v}, fetch, reached); err != nil {
					return err
				}
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
		return newError(DataCorrupted, "expected a B-tree node page")
	}
}

// reachablePages collects every page reachable from the committed snapshot whose catalog head is
// catRoot: the catalog chain, every table/index B+tree node, and (for spillable columns) the live
// overflow chains. It is the basis of within-session compaction (storage.maybeCompact / planFreeList):
// node page ids come from the in-memory tree walk (no pager reads), and only the catalog chain and
// spillable-leaf overflow are read through the pager. It does NOT cover a GiST index's on-disk R-tree
// pages (the resident GiST store holds only the leaf-key set, no on-disk page ids) nor the current
// persisted free-list pages — both are handled by the caller unioning the pages this commit just wrote
// into the reached set (a GiST index rewrites its whole R-tree every commit — gist.md §4.1(b) — so all
// live GiST pages are in that write set), so a rebuild never frees a live GiST or free-list page.
func (s *snapshot) reachablePages(paging *sharedPaging, catRoot uint32) (map[uint32]bool, error) {
	reached := make(map[uint32]bool)
	// The catalog chain (rewritten to fresh pages every commit; its predecessor pages are the bulk of
	// what compaction reclaims).
	for p := catRoot; p != 0; {
		reached[p] = true
		block, err := paging.pgr.readBlock(p)
		if err != nil {
			return nil, err
		}
		pg, err := parsePage(block)
		if err != nil {
			return nil, err
		}
		if pg.pageType != pageCatalog {
			return nil, newError(DataCorrupted, "expected a catalog page")
		}
		p = pg.nextPage
	}
	// Table data trees + their live overflow chains.
	for _, st := range s.stores {
		root := st.treeRoot()
		collectTreePages(root, reached)
		if root != nil && root.page != 0 && anySpillable(st.colTypes) {
			if err := collectLeafOverflow(paging, root.page, st.colTypes, reached); err != nil {
				return nil, err
			}
		}
	}
	// Secondary/unique index trees (empty-payload, never spillable).
	for _, ist := range s.indexStores {
		collectTreePages(ist.treeRoot(), reached)
	}
	return reached, nil
}

// collectTreePages adds every node page of a resident B+tree to reached: an interior/leaf node's own
// set-once page, and each OnDisk child leaf's page (walked without faulting it — the page id is on the
// childRef). A page-0 node is a dirty node not yet persisted (never on a committed tree at compaction
// time); it is skipped so page 0 (a meta slot) is never marked.
func collectTreePages(n *pnode, reached map[uint32]bool) {
	if n == nil {
		return
	}
	if n.page != 0 {
		reached[n.page] = true
	}
	for _, c := range n.children {
		if c.node == nil {
			reached[c.page] = true // an OnDisk leaf: page id known without a fault
		} else {
			collectTreePages(c.node, reached)
		}
	}
}

// readFreeList reads a persisted free-list (v25) by following the page_type 7 chain from head (meta
// offset 28) through the pager, collecting every free page index. head == 0 is an empty free-list.
// The inverse of the serialization in serializeFreeList; replaces the v24 reconstruct-on-open
// reachability walk (spec/fileformat/format.md *Reclamation*).
func readFreeList(paging *sharedPaging, head uint32) ([]uint32, error) {
	var free []uint32
	for p := head; p != 0; {
		block, err := paging.pgr.readBlock(p)
		if err != nil {
			return nil, err
		}
		pg, err := parsePage(block)
		if err != nil {
			return nil, err
		}
		if pg.pageType != pageFreelist {
			return nil, newError(DataCorrupted, "expected a free-list page")
		}
		pos := 0
		for i := uint32(0); i < pg.itemCount; i++ {
			e, err := readU32(pg.payload, &pos)
			if err != nil {
				return nil, err
			}
			free = append(free, e)
		}
		p = pg.nextPage
	}
	return free, nil
}

// serializeFreeList serializes the full free-list persist (ascending — the pages dead at the new
// committed snapshot, including this commit's fresh orphans) into a page_type 7 chain (v25 —
// spec/fileformat/format.md *Free-list page*), and returns the chain pages, its head (0 when empty),
// the list actually persisted (persist minus the pages the chain occupies), and the new high-water.
//
// The chain's own pages are drawn from safe — the subset of the free-list that is dead at the FALLBACK
// (prior) snapshot too (freeRemaining), so overwriting them this commit is torn-write-safe — NOT from
// the high-water, so persisting the free-list does not grow the file. The high-water is extended only
// if safe is exhausted (rare: a large delete on an otherwise-tight file). A free-list page is consumed
// here, so it never appears in the list it carries; this commit's fresh orphans (in persist, not in
// safe) are persisted but not reused until the next commit (the watermark — still reachable from the
// fallback).
func serializeFreeList(persist, safe []uint32, cap, ps int, next uint32) ([]dirtyPage, uint32, []uint32, uint32) {
	// Nothing worth persisting when it would take the whole list to hold itself (empty, or a lone
	// page): leave the residue in RAM, reclaimed at the next compaction (a bounded transient leak).
	if len(persist) < 2 {
		return nil, 0, append([]uint32(nil), persist...), next
	}
	per := cap / 4
	if per < 1 {
		per = 1
	}
	// Draw free-list pages (from safe, then the high-water) until they hold every entry that then
	// remains. Each page drawn from safe also removes itself from what must be held (it is in persist),
	// so the loop converges; a high-water page adds a slot without shrinking the content.
	var flIDs []uint32
	si := 0
	hw := next
	safeDrawn := 0
	for {
		content := len(persist) - safeDrawn
		if (content+per-1)/per <= len(flIDs) {
			break
		}
		if si < len(safe) {
			flIDs = append(flIDs, safe[si])
			si++
			safeDrawn++
		} else {
			flIDs = append(flIDs, hw)
			hw++
		}
	}
	// The persisted list is persist minus the pages drawn from it (the first safeDrawn flIDs).
	drawn := make(map[uint32]bool, safeDrawn)
	for _, id := range flIDs[:safeDrawn] {
		drawn[id] = true
	}
	var persisted []uint32
	for _, p := range persist {
		if !drawn[p] {
			persisted = append(persisted, p)
		}
	}
	pages := make([]dirtyPage, 0, len(flIDs))
	for ci, pageNo := range flIDs {
		lo := ci * per
		if lo > len(persisted) {
			lo = len(persisted)
		}
		hi := (ci + 1) * per
		if hi > len(persisted) {
			hi = len(persisted)
		}
		chunk := persisted[lo:hi]
		var nextPage uint32
		if ci+1 < len(flIDs) {
			nextPage = flIDs[ci+1]
		}
		payload := make([]byte, 0, len(chunk)*4)
		for _, e := range chunk {
			var b [4]byte
			binary.BigEndian.PutUint32(b[:], e)
			payload = append(payload, b[:]...)
		}
		pages = append(pages, dirtyPage{index: pageNo, bytes: makePage(ps, pageFreelist, uint32(len(chunk)), nextPage, payload)})
	}
	return pages, flIDs[0], persisted, hw
}

// planFreeList is the v25 durable-commit free-list plan, shared by the file commit paths
// (shared.commitFile and file.persist). It runs IN-COMMIT (after the tree + catalog are written to the
// pager, before the meta), so the list it persists includes THIS commit's fresh orphans — without
// that, a short open→commit→close session would leak them forever (open no longer reconstructs the
// free-list, v25). COMPACT (periodic — the high-water has grown past ~2× the live count at the last
// compaction, and no reader pins an older version): the persisted list is [2, pageCount) − reached
// (written unioned in so a wholesale-rewritten GiST R-tree is never freed; the catalog + new overflow
// are covered by reading the just-written pages back). CARRY (otherwise): the persisted list is
// freeRemaining (this commit's orphans wait for the next compaction). Either way the chain pages come
// from freeRemaining. Returns the chain pages, head, the new free-list, the new high-water, the
// live count to remember (unchanged when not compacting), and the free-list GENERATION txid — the
// version the persisted free-list is "as of". On a COMPACT the generation becomes snap.txid (the pages
// are proven dead at snap.txid); on a CARRY it is unchanged (the list only shrank). The generation gates
// reuse (transactions.md §8): the pages are dead at their generation, so reusing them is safe only once
// no reader pins a version older than it — commitDurable checks oldest_live ≥ generation before reuse.
func planFreeList(snap *snapshot, paging *sharedPaging, catRoot uint32, written []dirtyPage, freeRemaining []uint32, pageCount, liveAtCompaction uint32, genTxid uint64, cap, ps int, canReclaim, canReuse bool) ([]dirtyPage, uint32, []uint32, uint32, uint32, uint64, error) {
	const minCompactPages = 16 // don't churn a tiny store
	// liveAtCompaction==0 is the shared-file handoff sentinel: reconstruct on the first proven-alone
	// commit even when the file is still below the ordinary amortization threshold.
	compact := canReclaim && (liveAtCompaction == 0 || (pageCount > minCompactPages && uint64(pageCount) > 2*uint64(liveAtCompaction)))
	persistList := freeRemaining
	newLive := liveAtCompaction
	newGen := genTxid
	if compact {
		reached, err := snap.reachablePages(paging, catRoot)
		if err != nil {
			return nil, 0, nil, 0, 0, 0, err
		}
		for _, w := range written {
			reached[w.index] = true
		}
		var free []uint32
		for p := rootPage; p < pageCount; p++ {
			if !reached[p] {
				free = append(free, p)
			}
		}
		persistList = free
		newLive = uint32(len(reached))
		newGen = snap.txid // the recomputed list is proven dead at snap.txid (the generation gate)
	}
	// The free-list CHAIN pages overwrite in place, so they may only land on pages no live reader can
	// observe. freeRemaining is dead at the FALLBACK snapshot (torn-write-safe), but a reader pinned OLDER
	// than the free-list generation may still reference one of those pages (transactions.md §8) — the same
	// hazard as data-page reuse. When the watermark defers reuse (canReuse false) the chain must therefore
	// grow the high-water instead (safe empty), exactly as the data allocator does; when reuse is allowed,
	// freeRemaining is reader-safe (oldest_live ≥ generation) and reused as before.
	safe := freeRemaining
	if !canReuse {
		safe = nil
	}
	pages, head, persisted, newPC := serializeFreeList(persistList, safe, cap, ps, pageCount)
	return pages, head, persisted, newPC, newLive, newGen, nil
}

// readSkeleton reads a table's on-disk B+tree (rooted at rootPage) into a demand-paged skeleton:
// interior nodes resident, every leaf left OnDisk (faulted on first access). Returns the root node
// only — it does not compute a row count because the caller installs the exact v28 catalog count
// alongside the skeleton (spec/design/storage.md §6). A table whose root is itself a single leaf
// has no interior parent to hold an OnDisk reference, so the root leaf is faulted resident
// (spec/design/pager.md §1/§4).
func readSkeleton(paging *sharedPaging, root uint32, colTypes []colType) (*pnode, error) {
	c, err := readSkeletonNode(paging, root, colTypes)
	if err != nil {
		return nil, err
	}
	if c.node != nil {
		return c.node, nil
	}
	return paging.faultLeaf(c.page, colTypes)
}

// readSkeletonNode resolves one B+tree node into a childRef WITHOUT reading the leaf level. A leaf
// page yields an OnDisk childRef — its bytes are not read here at all; the parent hands down the page
// id and the leaf faults on first access. An interior page yields a resident childRef — the
// record-free separators + children skeleton (v24) — with its children resolved.
//
// The open-speed trick (spec/design/storage.md §6, v28 catalog count): an interior's children
// are homogeneous — a B+tree keeps every leaf at one depth, so an interior's children are either all
// leaves or all interiors. We resolve only the first child to learn which; if it came back OnDisk (a
// leaf), every sibling is a leaf too and becomes an OnDisk reference WITHOUT a block read. Only
// interior pages are read, so open is O(interior spine) rather than O(leaves) — the second and last
// reason open used to touch every leaf (after v25 dropped the free-list reachability walk) is gone.
// The cost: the first child of each bottom-level interior is still read (to classify the level), i.e.
// ~leaves/fanout leaf reads, negligible beside the former per-leaf walk. A corrupt leaf is now
// surfaced at fault rather than at open (still XX001, never wrong rows — spec/design/storage.md §7);
// the interior spine is still CRC-validated here at open.
func readSkeletonNode(paging *sharedPaging, pageIdx uint32, colTypes []colType) (childRef, error) {
	block, err := paging.pgr.readBlock(pageIdx)
	if err != nil {
		return childRef{}, err
	}
	pg, err := parsePage(block)
	if err != nil {
		return childRef{}, err
	}
	switch pg.pageType {
	case pageLeaf:
		return onDiskRef(pageIdx), nil
	case pageInterior:
		n := int(pg.itemCount)
		pos := 0
		// Child pointers precede the separator directory (format.md "Interior node").
		childPtrs := make([]uint32, 0, n+1)
		for i := 0; i < n+1; i++ {
			cp, err := readU32(pg.payload, &pos)
			if err != nil {
				return childRef{}, err
			}
			childPtrs = append(childPtrs, cp)
		}
		// v24: the record-free routing skeleton — an end-offset separator directory + key blob.
		// Separators carry no values, so no lazy decode and no chains to mark.
		keys, err := readSeparators(pg.payload, &pos, n)
		if err != nil {
			return childRef{}, err
		}
		// Resolve the first child to classify the level, then avoid reading leaf siblings.
		children := make([]childRef, 0, n+1)
		first, err := readSkeletonNode(paging, childPtrs[0], colTypes)
		if err != nil {
			return childRef{}, err
		}
		childrenAreLeaves := first.node == nil
		children = append(children, first)
		for _, cp := range childPtrs[1:] {
			if childrenAreLeaves {
				children = append(children, onDiskRef(cp)) // a leaf sibling — not read at open
				continue
			}
			child, err := readSkeletonNode(paging, cp, colTypes)
			if err != nil {
				return childRef{}, err
			}
			children = append(children, child)
		}
		return residentRef(&pnode{keys: keys, children: children, page: pageIdx}), nil
	default:
		return childRef{}, newError(DataCorrupted, "expected a B-tree node page")
	}
}

// readSeparators reads a v24 interior node's separator keys: the N-entry end-offset directory then
// the key blob, pos at the directory's first byte (spec/fileformat/format.md "Interior node").
func readSeparators(payload []byte, pos *int, n int) ([][]byte, error) {
	ends := make([]uint32, n)
	for i := range ends {
		e, err := readU32(payload, pos)
		if err != nil {
			return nil, err
		}
		ends[i] = e
	}
	blob := *pos
	keys := make([][]byte, 0, n)
	prev := uint32(0)
	for _, e := range ends {
		if e < prev || blob+int(e) > len(payload) {
			return nil, newError(DataCorrupted, "interior separator directory out of range")
		}
		key := make([]byte, e-prev)
		copy(key, payload[blob+int(prev):blob+int(e)])
		keys = append(keys, key)
		prev = e
	}
	*pos = blob + int(prev)
	return keys, nil
}

// isSpillable reports whether a value of this type can be stored out-of-line (a variable-length
// type). Fixed-width scalars (int*/boolean/uuid/timestamp*) are tiny and always stay inline
// (spec/design/large-values.md §12). A COMPOSITE is treated as spillable — its opaque inline body
// spills via the same overflow + LZ4 path when a record exceeds RECORD_MAX (spec/design/composite.md
// §4); a small composite is never actually chosen by the plan.
func isSpillable(ty colType) bool {
	if ty.Composite || ty.Elem != nil || ty.RangeElem != nil {
		// An array's opaque inline body spills via the same overflow + LZ4 path (array.md §4). A
		// range's body is its flags byte + bound bodies; a numrange over huge decimals could exceed
		// RECORD_MAX, so it rides the same path (a discrete range is tiny — never actually chosen by
		// the plan, spec/design/ranges.md §4).
		return true
	}
	// json/jsonb are variable-length document bodies that ride the same overflow + LZ4 path as
	// text/bytea when a record exceeds RECORD_MAX (spec/design/json.md §2/§4).
	return ty.Scalar.IsText() || ty.Scalar.IsBytea() || ty.Scalar.IsDecimal() ||
		ty.Scalar.IsJson() || ty.Scalar.IsJsonb()
}

// pagePayload is the page payload capacity C = pageSize − pageHeader — the bytes a single page has
// for body content (the B-tree split threshold and the overflow-chain slab size). The in-memory
// store, the whole-image serializer, and the cost meter must all use this one value, or the split
// decision diverges from the serialized layout (the `−12` drift that pageHeader's v7 growth to 16
// silently introduced — format.md §7).
func pagePayload(pageSize uint32) int {
	return int(pageSize) - pageHeader
}

// recordMaxFor is the largest a single LEAF record may serialize to and still satisfy the B+tree
// split contract — RECORD_MAX(C,K) = (C − max(12, 12+16·K))/2 where C = capacity is the page
// payload and K the value-column count (format.md "Why the record cap"). The value is deliberately
// KEPT from v23 (bplus-reshape.md §4.2), re-derived leaf-only: the worst-case (all-variable)
// two-record leaf overhead is 12 + 13·K ≤ 12 + 16·K, so a two-record leaf never overflows. The
// spill planner reduces a record to ≤ this by externalizing values.
func recordMaxFor(capacity, k int) int {
	reserve := recordMaxReserve + 16*k // = max(12, 12+16K) since k ≥ 0
	m := (capacity - reserve) / 2
	if m < 0 {
		m = 0
	}
	return m
}

// fixedValueWidth is the storage width of a FIXED-WIDTH column's value body (the dense leaf slot
// stride — spec/fileformat/format.md v24 "Leaf node"), or (0, false) for a VARIABLE-WIDTH column
// (text / bytea / decimal / json / jsonb / composite / array / range — exactly the spillable set).
// The class decides the column's leaf region shape: fixed-width regions are a null bitmap + dense
// untagged slots; variable regions are a value directory + tagged codec bytes (NULL = a
// zero-length span). MUST stay the exact complement of isSpillable.
func fixedValueWidth(ty colType) (int, bool) {
	if ty.Composite || ty.Elem != nil || ty.RangeElem != nil {
		return 0, false
	}
	switch ty.Scalar {
	case scalarInt16:
		return 2, true
	case scalarInt32:
		return 4, true
	case scalarInt64:
		return 8, true
	case scalarBool:
		return 1, true
	case scalarUuid:
		return 16, true
	case scalarTimestamp, scalarTimestamptz:
		return 8, true
	case scalarDate:
		return 4, true
	case scalarInterval:
		return 16, true
	case scalarFloat64:
		return 8, true
	case scalarFloat32:
		return 4, true
	default:
		// text/decimal/bytea/json/jsonb are variable-width; jsonpath is not storable as a column
		// (type code 20 is reserved), classed variable defensively.
		return 0, false
	}
}

// leafShape is a leaf's column-class shape — the two counts leafOverhead needs beyond N (the fixed
// and variable column counts, fixed + variable = K). Computed once per store from its column types
// and threaded through the B+tree's size arithmetic (pmap), which never sees the types themselves.
type leafShape struct {
	fixed    int
	variable int
}

func (s leafShape) k() int { return s.fixed + s.variable }

// leafShapeFor is the shape of a leaf for a table with these value-column types (an index tree —
// empty colTypes — is {0, 0}).
func leafShapeFor(colTypes []colType) leafShape {
	fixed := 0
	for _, ty := range colTypes {
		if _, ok := fixedValueWidth(ty); ok {
			fixed++
		}
	}
	return leafShape{fixed: fixed, variable: len(colTypes) - fixed}
}

// leafOverhead is the bytes a v24 leaf's payload carries BEYOND Σ recordSize (format.md "Leaf
// node"): the key directory (4·N), the column directory (4·(K+1)), and per region a flags byte
// plus — fixed-width — the null bitmap (ceil(N/8)) or — variable-width — the value directory
// (4·N). Interior nodes do not use this (their payload is 8·N + 4 + Σ sep_len):
//
//	leafOverhead(N, cols) = 4·N + 4·(K+1) + F·(1 + ceil(N/8)) + V·(1 + 4·N)
func leafOverhead(n int, shape leafShape) int {
	return 4*n + 4*(shape.k()+1) + shape.fixed*(1+(n+7)/8) + shape.variable*(1+4*n)
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
func planDispositions(colTypes []colType, key []byte, row storedRow, capacity int) recordPlan {
	// Each column's inline-plain contribution to recordSize (the v24 basis — format.md "Record"):
	// a fixed-width column always its width (a NULL occupies a zero-filled slot); a variable-width
	// column 0 when NULL (a zero-length span) else its tagged inline encoding.
	inline := make([]int, len(colTypes))
	size := len(key)
	for i, ty := range colTypes {
		if w, ok := fixedValueWidth(ty); ok {
			inline[i] = w
		} else if row[i].IsNull() {
			inline[i] = 0
		} else {
			inline[i] = len(encodeValue(ty, row[i]))
		}
		size += inline[i]
	}
	plan := recordPlan{
		disp: make([]valueDisp, len(colTypes)),
		comp: make([][]byte, len(colTypes)),
	}
	cur := append([]int(nil), inline...)
	max := recordMaxFor(capacity, len(colTypes))
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

// recordSize is the on-disk size of a record — the weight the page-backed B+tree splits on
// (format.md "Record": key_len + Σ value_size — a fixed-width column always its width, a NULL
// variable-width value 0). Accounts for compression and out-of-line spill: a compressed value
// contributes its compressed inline form, an externalized one its fixed pointer size
// (large-values.md §12/§13). Must equal what the serializer produces, so in-memory node
// boundaries match serialized pages.
func recordSize(colTypes []colType, key []byte, row storedRow, capacity int) int {
	return planDispositions(colTypes, key, row, capacity).size
}

// recordScanUnits returns the per-record units a scan's up-front cost block charges beyond the
// B-tree nodes (cost.md §3; large-values.md §8/§12/§14): for every column in the query's TOUCHED
// SET (mask), pages = one page_read per overflow chain page (the chain carries the payload for
// external-plain, the COMPRESSED block for external-compressed) and decompress = ceil(raw/capacity)
// value_decompress slabs per compressed stored value (inline- or external-). Zero/zero for a
// fully-inline-plain record or an untouched column.
func recordScanUnits(colTypes []colType, key []byte, row storedRow, capacity int, mask []bool) (pages, decompress int) {
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
			switch v.unfetched().Form {
			case 0x00:
				// Inline-deferred values live in the record — no chain page, no decompress slab
				// (lazy-record.md §8: cost is invariant; matches the resident plan's dispInline).
			case tagExternal:
				pages += (int(v.unfetched().StoredLen) + capacity - 1) / capacity
			case tagInlineComp:
				decompress += (int(v.unfetched().RawLen) + capacity - 1) / capacity
			case tagExternalComp:
				pages += (int(v.unfetched().StoredLen) + capacity - 1) / capacity
				decompress += (int(v.unfetched().RawLen) + capacity - 1) / capacity
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
func recordCompressUnits(colTypes []colType, key []byte, row storedRow, capacity int) int {
	return planDispositions(colTypes, key, row, capacity).compressUnits
}

// valuePayload is a value's content payload P(v) — the bytes stored in the overflow chain when it is
// externalized (large-values.md §12): raw UTF-8 for text / raw bytes for bytea (both in v.Str), the
// decimal body (encoding minus its presence tag) for decimal. Only spillable types reach here.
func valuePayload(ty colType, v Value) []byte {
	if ty.Elem != nil {
		// An array's payload is its body (the ndim/flags/dims header + bitmap + element bodies);
		// a large array spills through the same overflow + LZ4 path (spec/design/array.md §4).
		return encodeArrayBody(*ty.Elem, v.arrayVal())
	}
	if ty.RangeElem != nil {
		// A range's payload is its body (the flags byte + present bound bodies, spec/design/ranges.md §4).
		return encodeRangeBody(*ty.RangeElem, v.rangeVal())
	}
	if ty.Composite {
		// A composite's payload is its body — the encoding minus the leading presence tag, i.e. the
		// null bitmap + present-field bodies (spec/design/composite.md §4).
		return encodeCompositeBody(ty.Fields, *v.composite())
	}
	switch {
	case ty.Scalar.IsText(), ty.Scalar.IsBytea():
		return []byte(v.str())
	// json's payload is the verbatim UTF-8 (no length prefix — the chain tracks its own length,
	// exactly like text); jsonb's payload is the tagged-node tree body (spec/design/json.md §4/§2).
	case ty.Scalar.IsJson():
		return []byte(v.str())
	case ty.Scalar.IsJsonb():
		return encodeJsonbBody(v.jsonb(), nil)
	case ty.Scalar.IsDecimal():
		return encodeScalar(ty.Scalar, v)[1:] // strip the leading presence tag
	default:
		panic("only spillable values are externalized")
	}
}

// valueFromPayload reconstructs a value from the P(v) content gathered from its overflow chain
// (inverse of valuePayload) — large-values.md §12.
func valueFromPayload(ty colType, payload []byte) (Value, error) {
	if ty.Elem != nil {
		// An array's payload is its body; decode it with a fresh cursor (spec/design/array.md §4).
		pos := 0
		return readArrayBody(ty, payload, &pos, decodeConstruct)
	}
	if ty.RangeElem != nil {
		// A range's payload is its body; decode it with a fresh cursor (spec/design/ranges.md §4).
		pos := 0
		return readRangeBody(*ty.RangeElem, payload, &pos, decodeConstruct)
	}
	if ty.Composite {
		// A composite's payload is its body (bitmap + present-field bodies); decode it with a fresh
		// cursor (spec/design/composite.md §4).
		pos := 0
		return readCompositeBody(ty, payload, &pos, decodeConstruct)
	}
	switch {
	case ty.Scalar.IsText():
		if !utf8.Valid(payload) {
			return Value{}, newError(DataCorrupted, "non-UTF-8 text value")
		}
		return TextValue(string(payload)), nil
	case ty.Scalar.IsBytea():
		return ByteaValue(payload), nil
	case ty.Scalar.IsJson():
		if !utf8.Valid(payload) {
			return Value{}, newError(DataCorrupted, "non-UTF-8 json value")
		}
		return JsonValue(string(payload)), nil
	case ty.Scalar.IsJsonb():
		pos := 0
		n, err := decodeJsonbBody(payload, &pos, decodeConstruct)
		if err != nil {
			return Value{}, err
		}
		return JsonbValue(n), nil
	case ty.Scalar.IsDecimal():
		pos := 0
		return decodeDecimalBody(payload, &pos, decodeConstruct)
	default:
		return Value{}, newError(DataCorrupted, "a non-spillable type was stored external")
	}
}

// encodeInterior builds a v24 INTERIOR node payload (spec/fileformat/format.md "Interior node"):
// N+1 child pointers ‖ an N-entry end-offset separator directory ‖ the separator key blob.
// Record-free — no value codec, no overflow chains; a separator is raw order-preserving key bytes.
func encodeInterior(seps [][]byte, childPages []uint32) []byte {
	var out []byte
	for _, cp := range childPages {
		out = appendU32(out, cp)
	}
	off := uint32(0)
	for _, s := range seps {
		off += uint32(len(s))
		out = appendU32(out, off)
	}
	for _, s := range seps {
		out = append(out, s...)
	}
	return out
}

// encodeDisposedValue encodes one value's on-disk body given its resolved disposition (the value
// codec, unchanged across the row-major and PAX leaf layouts): an inline-plain value is encodeValue;
// the large-value forms carry a pointer / inline-compressed block and allocate overflow chains via
// take (large-values.md §12/§13). comp is the LZ4 block planDispositions already produced for a
// compressed form (nil otherwise), so the serializer never re-compresses.
func encodeDisposedValue(ty colType, v Value, disp valueDisp, comp []byte, capacity int, take func() uint32, ovf *[]overflowPageOut) []byte {
	switch disp {
	case dispExternal:
		payload := valuePayload(ty, v)
		first := writeOverflowChain(payload, capacity, take, ovf)
		out := []byte{tagExternal}
		out = appendU32(out, first)
		out = appendU32(out, uint32(len(payload)))
		return out
	case dispInlineComp:
		rawLen := len(valuePayload(ty, v))
		out := []byte{tagInlineComp}
		out = appendU32(out, uint32(rawLen))
		out = appendU16(out, uint16(len(comp)))
		out = append(out, comp...)
		return out
	case dispExternalComp:
		// The chain carries the COMPRESSED block (its page count follows comp size).
		rawLen := len(valuePayload(ty, v))
		first := writeOverflowChain(comp, capacity, take, ovf)
		out := []byte{tagExternalComp}
		out = appendU32(out, first)
		out = appendU32(out, uint32(len(comp)))
		out = appendU32(out, uint32(rawLen))
		return out
	default:
		return encodeValue(ty, v)
	}
}

// encodeLeafPAX builds a v24 PAX (column-major) leaf payload from records in ascending key order
// (format.md "Leaf node"). Values are encoded in (record, column) order — so each external value's
// overflow chain is allocated via take in exactly that order (a node's own page is allocated by
// the caller first; then chains in record-then-column order), keeping the overflow page indices
// golden-pinned. The bytes are then assembled column-major:
//
//	key dir : keyEnd[0..N)   N u32 END offsets into the key blob (leading 0 implicit)
//	key blob: N keys concatenated (ascending)
//	col dir : colStart[0..K] (K+1) u32 absolute payload offset of each column region; colStart[K]=end
//	col c   : a flags byte (0), then — fixed-width — the null bitmap (ceil(N/8), MSB-first, set =
//	          NULL) + N×width dense UNTAGGED slots (a NULL slot zero-filled), or — variable-width —
//	          an N-entry u32 end-offset value directory + the tagged value bodies (NULL = a
//	          zero-length span)
func encodeLeafPAX(colTypes []colType, keys [][]byte, rows []storedRow, capacity int, take func() uint32, ovf *[]overflowPageOut) []byte {
	n := len(keys)
	k := len(colTypes)
	// Encode each value in (record, column) order; overflow chains allocate here. A fixed-width
	// column's slot is the untagged inline body (encodeValue minus its 0x00 tag; zeros for NULL);
	// a variable column's bytes are the tagged disposed form (empty for NULL).
	valBytes := make([][][]byte, k) // valBytes[c][i]
	nulls := make([][]bool, k)      // nulls[c][i]
	for c := range valBytes {
		valBytes[c] = make([][]byte, n)
		nulls[c] = make([]bool, n)
	}
	for i := 0; i < n; i++ {
		plan := planDispositions(colTypes, keys[i], rows[i], capacity)
		for c, ty := range colTypes {
			isNull := rows[i][c].IsNull()
			nulls[c][i] = isNull
			w, fixed := fixedValueWidth(ty)
			switch {
			case fixed && isNull:
				valBytes[c][i] = make([]byte, w) // a zero-filled slot, never read
			case fixed:
				valBytes[c][i] = encodeValue(ty, rows[i][c])[1:] // the untagged body
			case isNull:
				valBytes[c][i] = nil // a zero-length span
			default:
				valBytes[c][i] = encodeDisposedValue(ty, rows[i][c], plan.disp[c], plan.comp[c], capacity, take, ovf)
			}
		}
	}
	// key directory (N end offsets) + key blob.
	out := []byte{}
	off := 0
	for i := 0; i < n; i++ {
		off += len(keys[i])
		out = appendU32(out, uint32(off))
	}
	for i := 0; i < n; i++ {
		out = append(out, keys[i]...)
	}
	// column directory: absolute payload offset of each column region.
	baseAfterColDir := len(out) + 4*(k+1)
	colStart := make([]int, k+1)
	cur := baseAfterColDir
	for c := 0; c < k; c++ {
		colStart[c] = cur
		bodies := 0
		for i := 0; i < n; i++ {
			bodies += len(valBytes[c][i])
		}
		if _, fixed := fixedValueWidth(colTypes[c]); fixed {
			cur += 1 + (n+7)/8 + bodies
		} else {
			cur += 1 + 4*n + bodies
		}
	}
	colStart[k] = cur // payload end
	for c := 0; c <= k; c++ {
		out = appendU32(out, uint32(colStart[c]))
	}
	// each column region: flags byte, then bitmap + dense slots (fixed) or value directory +
	// tagged bodies (variable).
	for c := 0; c < k; c++ {
		out = append(out, 0) // region flags — reserved (the dictionary door)
		if _, fixed := fixedValueWidth(colTypes[c]); fixed {
			bitmap := make([]byte, (n+7)/8)
			for i, isNull := range nulls[c] {
				if isNull {
					bitmap[i/8] |= 0x80 >> (i % 8)
				}
			}
			out = append(out, bitmap...)
		} else {
			voff := 0
			for i := 0; i < n; i++ {
				voff += len(valBytes[c][i])
				out = appendU32(out, uint32(voff))
			}
		}
		for i := 0; i < n; i++ {
			out = append(out, valBytes[c][i]...)
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
func compositeTypeEntryBytes(ct *compositeType) []byte {
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
		// A text field appends its varchar(n) max length (v22); 0 = unbounded (types.md §15).
		if f.Type.Comp == nil && f.Type.IsText() {
			var n uint32
			if f.VarcharLen != nil {
				n = *f.VarcharLen
			}
			out = appendU32(out, n)
		}
	}
	return out
}

// decodeCompositeTypeEntry decodes a composite-type catalog entry's body (inverse of
// compositeTypeEntryBytes); the caller has already consumed the entry_kind byte. Nested composite
// fields hold the referenced type's NAME (resolved/validated after the whole catalog is read — the
// two-pass load).
func decodeCompositeTypeEntry(buf []byte, pos *int) (*compositeType, error) {
	name, err := readString(buf, pos)
	if err != nil {
		return nil, err
	}
	fieldCount, err := readU16(buf, pos)
	if err != nil {
		return nil, err
	}
	fields := make([]compositeField, 0, fieldCount)
	for i := uint16(0); i < fieldCount; i++ {
		fname, err := readString(buf, pos)
		if err != nil {
			return nil, err
		}
		tc, err := readU8(buf, pos)
		if err != nil {
			return nil, err
		}
		var fty dataType
		isDecimal := false
		isText := false
		if tc == 14 {
			tn, err := readString(buf, pos)
			if err != nil {
				return nil, err
			}
			fty = compositeT(tn)
		} else if tc == 15 {
			// An array-typed field (spec/design/array.md §12): the element-type descriptor, then
			// (below) the flags byte — the inverse of the array arm in compositeTypeEntryBytes.
			elem, err := readArrayElementType(buf, pos)
			if err != nil {
				return nil, err
			}
			fty = arrayT(elem)
		} else {
			s, ok := scalarForTypeCode(tc)
			if !ok {
				return nil, newError(DataCorrupted, "unknown field type code")
			}
			fty = scalarT(s)
			isDecimal = s.IsDecimal()
			isText = s.IsText()
		}
		flags, err := readU8(buf, pos)
		if err != nil {
			return nil, err
		}
		if flags&^uint8(0b1) != 0 {
			return nil, newError(DataCorrupted, "reserved composite field flag set")
		}
		var decimal *decimalTypmod
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
				decimal = &decimalTypmod{Precision: precision, Scale: scale}
			}
		}
		// A text field carries its varchar(n) max length (v22); 0 = unbounded (types.md §15).
		var varcharLen *uint32
		if isText {
			n, err := readU32(buf, pos)
			if err != nil {
				return nil, err
			}
			if n != 0 {
				varcharLen = &n
			}
		}
		fields = append(fields, compositeField{Name: fname, Type: fty, Decimal: decimal, VarcharLen: varcharLen, NotNull: flags&0b1 != 0})
	}
	return &compositeType{Name: name, Fields: fields}, nil
}

// sequenceEntryBytes serializes a sequence catalog entry's BODY (after its entry_kind = 2 byte):
// name, then the six fixed i64 fields (big-endian two's-complement, no sign-flip) and a flags byte
// — spec/fileformat/format.md *Sequence entry*. Fixed-width, every field present (no presence tags).
func sequenceEntryBytes(s *sequenceDef) []byte {
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
func decodeSequenceEntry(buf []byte, pos *int) (*sequenceDef, error) {
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
		return nil, newError(DataCorrupted, "reserved sequence flag set")
	}
	// The OWNED BY tail (v13): present iff bit2 (has_owner) is set.
	var owner *seqOwner
	if flags&0b100 != 0 {
		ownerTable, err := readString(buf, pos)
		if err != nil {
			return nil, err
		}
		ownerCol, err := readU16(buf, pos)
		if err != nil {
			return nil, err
		}
		owner = &seqOwner{Table: ownerTable, Column: ownerCol}
	}
	return &sequenceDef{
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

// collationEntryBytes serializes a collation reference entry's BODY (after its entry_kind = 3 byte,
// v18): a flags byte (bit0 is_default), then metadata ONLY — name + unicode_version + cldr_version +
// description, each u16-len + UTF-8. NO table: it is vendored into the binary and resolved by name on
// open (spec/design/collation.md §2/§5/§9).
func collationEntryBytes(c *Collation, isDefault bool) []byte {
	var out []byte
	var flags byte
	if isDefault {
		flags = 0b1
	}
	out = append(out, flags)
	out = appendString(out, c.Name)
	out = appendString(out, c.UnicodeVersion)
	out = appendString(out, c.CldrVersion)
	out = appendString(out, c.Description)
	return out
}

// decodeCollationEntry decodes a collation reference entry's body (inverse of collationEntryBytes);
// the caller has consumed the entry_kind byte. Reads the metadata, then resolves the compiled table
// from the binary's VENDORED set by name (§2/§9) — the table is no longer in the file. Returns the
// resolved collation + whether it is the per-database default (the is_default flag bit).
func decodeCollationEntry(buf []byte, pos *int) (*Collation, bool, error) {
	flags, err := readU8(buf, pos)
	if err != nil {
		return nil, false, err
	}
	if flags&^uint8(0b1) != 0 {
		return nil, false, newError(DataCorrupted, "reserved collation flag set")
	}
	isDefault := flags&0b1 != 0
	name, err := readString(buf, pos)
	if err != nil {
		return nil, false, err
	}
	unicode, err := readString(buf, pos)
	if err != nil {
		return nil, false, err
	}
	cldr, err := readString(buf, pos)
	if err != nil {
		return nil, false, err
	}
	desc, err := readString(buf, pos)
	if err != nil {
		return nil, false, err
	}
	// The file records only the version PIN; the table comes from a loaded bundle (the host must have
	// loaded one providing this collation before opening — collation.md §4/§9). A name no loaded bundle
	// provides at all is the graded verdict's legible refusal (slice 2d, collation.md §12 /
	// compatibility.md §7): the open is refused with XX002 naming the collation + version, rather than
	// degrading the rest of the database (the conservative resolution of compatibility.md §12 open #3 —
	// a version-skewed collation, by contrast, opens and is enforced read-only at write time §14).
	loaded := LoadedCollation(name)
	if loaded == nil {
		return nil, false, newError(CollationVersionMismatch,
			fmt.Sprintf("collation %q (@ %s/%s) is not provided by any loaded bundle", name, unicode, cldr))
	}
	coll := &Collation{
		Name:           name,
		UnicodeVersion: unicode,
		CldrVersion:    cldr,
		Description:    desc,
		Singles:        loaded.Singles,
		Contractions:   loaded.Contractions,
	}
	return coll, isDefault, nil
}

// tableEntryBytes builds one table's catalog entry (format.md). indexRoots is each
// index's tree root page, parallel to table.Indexes.
// statisticsCatalogEntries writes P9 kind-4 entries in canonical
// (table, column, subkind, ordinal) order.
func statisticsCatalogEntries(s *snapshot) [][]byte {
	var entries [][]byte
	for _, item := range s.statisticsSorted() {
		table := s.tables[item.table]
		statistics := item.statistics
		summary := []byte{4, 0}
		summary = appendString(summary, table.Name)
		summary = appendU16(summary, uint16(item.column))
		flags := byte(0)
		if statistics.Stale {
			flags |= 1
		}
		if statistics.DistinctCount != nil {
			flags |= 2
		}
		summary = append(summary, flags)
		summary = appendI64(summary, statistics.AnalyzedRows)
		summary = appendI64(summary, statistics.NullCount)
		summary = appendI64(summary, statistics.WidthSum)
		distinct := int64(0)
		if statistics.DistinctCount != nil {
			distinct = *statistics.DistinctCount
		}
		summary = appendI64(summary, distinct)
		summary = appendU32(summary, statistics.SampleRows)
		summary = appendU32(summary, statistics.SampleNonNullRows)
		summary = appendU16(summary, uint16(len(statistics.MCV)))
		summary = appendU16(summary, uint16(len(statistics.Histogram)))
		entries = append(entries, summary)

		colType := s.stores[item.table].colTypes[item.column]
		for ordinal, mcv := range statistics.MCV {
			entry := []byte{4, 1}
			entry = appendString(entry, table.Name)
			entry = appendU16(entry, uint16(item.column))
			entry = appendU16(entry, uint16(ordinal))
			entry = appendU32(entry, mcv.Frequency)
			encoded := encodeValue(colType, mcv.Value.Value)
			entry = appendU16(entry, uint16(len(encoded)))
			entry = append(entry, encoded...)
			entries = append(entries, entry)
		}
		for ordinal, bound := range statistics.Histogram {
			entry := []byte{4, 2}
			entry = appendString(entry, table.Name)
			entry = appendU16(entry, uint16(item.column))
			entry = appendU16(entry, uint16(ordinal))
			encoded := encodeValue(colType, bound.Value)
			entry = appendU16(entry, uint16(len(encoded)))
			entry = append(entry, encoded...)
			entries = append(entries, entry)
		}
	}
	return entries
}

func decodeStatisticsValue(buf []byte, pos *int, s *snapshot, tableKey string, column int) (statisticsValue, error) {
	table := s.tables[tableKey]
	if table == nil || column < 0 || column >= len(table.Columns) {
		return statisticsValue{}, newError(DataCorrupted, "statistics reference an unknown table or column")
	}
	colType := s.stores[tableKey].colTypes[column]
	valueLen, err := readU16(buf, pos)
	if err != nil {
		return statisticsValue{}, err
	}
	if valueLen == 0 || int(valueLen) > statisticsMaxValueBytes {
		return statisticsValue{}, newError(DataCorrupted, "invalid statistics value length")
	}
	encoded, err := take(buf, pos, int(valueLen))
	if err != nil {
		return statisticsValue{}, err
	}
	valuePos := 0
	value, err := readValue(colType, encoded, &valuePos, nil, nil)
	if err != nil {
		return statisticsValue{}, err
	}
	if valuePos != len(encoded) || !bytes.Equal(encodeValue(colType, value), encoded) {
		return statisticsValue{}, newError(DataCorrupted, "noncanonical statistics value")
	}
	if value.Kind == ValNull {
		return statisticsValue{}, newError(DataCorrupted, "statistics values may not be NULL")
	}
	_, _, _, _, collationSkewed := s.collationSkew(table.Columns[column].Collation)
	var coll *Collation
	if table.Columns[column].Collation != "" {
		coll = s.resolveCollation(table.Columns[column].Collation)
	}
	var key []byte
	if !collationSkewed {
		key, err = encodeTypedKey(table.Columns[column].Type, value, coll)
		if err != nil {
			return statisticsValue{}, newError(DataCorrupted, "invalid statistics comparison value")
		}
	}
	// A skewed collation's values were ordered with the file-pinned bundle. They remain
	// byte-canonical, but current comparison keys are not valid evidence about their old order.
	// The estimator ignores these facts and upgradeCollations clears them.
	if len(encodeValue(colType, value))-1 > statisticsMaxValueBytes || len(key) > statisticsMaxValueBytes {
		return statisticsValue{}, newError(DataCorrupted, "oversized persisted statistics value")
	}
	return statisticsValue{Value: value, Key: key}, nil
}

func decodeStatisticsEntry(buf []byte, pos *int, s *snapshot, expected map[string][2]int) error {
	subkind, err := readU8(buf, pos)
	if err != nil {
		return err
	}
	tableName, err := readString(buf, pos)
	if err != nil {
		return err
	}
	tableKey := strings.ToLower(tableName)
	columnRaw, err := readU16(buf, pos)
	if err != nil {
		return err
	}
	column := int(columnRaw)
	table := s.tables[tableKey]
	if table == nil {
		return newError(DataCorrupted, "statistics reference an unknown table")
	}
	if column >= len(table.Columns) {
		return newError(DataCorrupted, "statistics reference an unknown column")
	}
	_, _, _, _, collationSkewed := s.collationSkew(table.Columns[column].Collation)
	groupKey := fmt.Sprintf("%s\x00%d", tableKey, column)
	switch subkind {
	case 0:
		if _, exists := expected[groupKey]; exists {
			return newError(DataCorrupted, "duplicate statistics summary")
		}
		flags, err := readU8(buf, pos)
		if err != nil {
			return err
		}
		analyzedRows, err := readI64(buf, pos)
		if err != nil {
			return err
		}
		nullCount, err := readI64(buf, pos)
		if err != nil {
			return err
		}
		widthSum, err := readI64(buf, pos)
		if err != nil {
			return err
		}
		distinctRaw, err := readI64(buf, pos)
		if err != nil {
			return err
		}
		sampleRows, err := readU32(buf, pos)
		if err != nil {
			return err
		}
		sampleNonNull, err := readU32(buf, pos)
		if err != nil {
			return err
		}
		mcvRaw, err := readU16(buf, pos)
		if err != nil {
			return err
		}
		histRaw, err := readU16(buf, pos)
		if err != nil {
			return err
		}
		distribution := flags&2 != 0
		if flags&^byte(3) != 0 || analyzedRows < 0 || nullCount < 0 || nullCount > analyzedRows || widthSum < 0 || int64(sampleRows) > analyzedRows || int(sampleRows) > statisticsSampleRows || sampleNonNull > sampleRows || int(mcvRaw) > statisticsMCVEntries || int(histRaw) > statisticsHistogramBounds || (distribution && distinctRaw < 0) || (!distribution && distinctRaw != 0) || (distribution && distinctRaw > analyzedRows-nullCount) || (histRaw != 0 && histRaw < 2) || distribution != statisticsDistributionEligible(table.Columns[column].Type) {
			return newError(DataCorrupted, "invalid statistics summary")
		}
		statistics := &columnStatistics{AnalyzedRows: analyzedRows, Stale: flags&1 != 0, NullCount: nullCount, WidthSum: widthSum, SampleRows: sampleRows, SampleNonNullRows: sampleNonNull}
		if distribution {
			statistics.DistinctCount = &distinctRaw
		}
		s.putColumnStatistics(tableKey, column, statistics)
		expected[groupKey] = [2]int{int(mcvRaw), int(histRaw)}
	case 1:
		counts, exists := expected[groupKey]
		if !exists {
			return newError(DataCorrupted, "statistics MCV precedes its summary")
		}
		ordinalRaw, err := readU16(buf, pos)
		if err != nil {
			return err
		}
		frequency, err := readU32(buf, pos)
		if err != nil {
			return err
		}
		value, err := decodeStatisticsValue(buf, pos, s, tableKey, column)
		if err != nil {
			return err
		}
		statistics := s.columnStatistics(tableKey, column)
		ordinal := int(ordinalRaw)
		if ordinal != len(statistics.MCV) || ordinal >= counts[0] || frequency == 0 || frequency > statistics.SampleNonNullRows {
			return newError(DataCorrupted, "invalid statistics MCV ordinal or frequency")
		}
		if !collationSkewed {
			for _, existing := range statistics.MCV {
				if bytes.Equal(existing.Value.Key, value.Key) {
					return newError(DataCorrupted, "duplicate statistics MCV value")
				}
			}
		}
		if !collationSkewed && len(statistics.MCV) > 0 {
			previous := statistics.MCV[len(statistics.MCV)-1]
			if frequency > previous.Frequency || (frequency == previous.Frequency && bytes.Compare(value.Key, previous.Value.Key) < 0) {
				return newError(DataCorrupted, "statistics MCV values are out of order")
			}
		}
		statistics.MCV = append(statistics.MCV, statisticsMCV{Value: value, Frequency: frequency})
	case 2:
		counts, exists := expected[groupKey]
		if !exists {
			return newError(DataCorrupted, "statistics histogram precedes its summary")
		}
		ordinalRaw, err := readU16(buf, pos)
		if err != nil {
			return err
		}
		value, err := decodeStatisticsValue(buf, pos, s, tableKey, column)
		if err != nil {
			return err
		}
		statistics := s.columnStatistics(tableKey, column)
		ordinal := int(ordinalRaw)
		if ordinal != len(statistics.Histogram) || ordinal >= counts[1] {
			return newError(DataCorrupted, "invalid statistics histogram ordinal")
		}
		if !collationSkewed && len(statistics.Histogram) > 0 && bytes.Compare(statistics.Histogram[len(statistics.Histogram)-1].Key, value.Key) > 0 {
			return newError(DataCorrupted, "statistics histogram is out of order")
		}
		statistics.Histogram = append(statistics.Histogram, value)
	default:
		return newError(DataCorrupted, "unknown statistics entry subkind")
	}
	return nil
}

func tableEntryBytes(table *catTable, rootDataPage uint32, indexRoots []uint32, rowCount int64) []byte {
	if rowCount < 0 {
		panic("table row count is nonnegative")
	}
	if (rootDataPage == 0) != (rowCount == 0) {
		panic("table root and row count agree")
	}
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
			if *col.Identity == identityAlways {
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
		// A text column appends its varchar(n) max length — only for type_code 4 (v22). 0 =
		// unbounded, so a plain text column carries 0 (spec/design/types.md §15).
		if col.Type.IsText() {
			var n uint32
			if col.VarcharLen != nil {
				n = *col.VarcharLen
			}
			out = appendU32(out, n)
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
		out = appendU16(out, uint16(len(idx.Keys)))
		// Each key element is a column ordinal (u16, < col_count so never 0xFFFF) OR, for an
		// EXPRESSION key (v26 — indexes.md §6), the 0xFFFF sentinel + a u16 length + the canonical
		// UTF-8 text (the Check-expression text form).
		for _, k := range idx.Keys {
			if k.Expr != nil {
				out = appendU16(out, 0xFFFF)
				out = appendU16(out, uint16(len(k.Expr.ExprText)))
				out = append(out, k.Expr.ExprText...)
			} else {
				out = appendU16(out, uint16(k.Col))
			}
		}
		// index_flags: bit0 unique (v6), bit1 has_predicate (v27 — a partial index, indexes.md §9).
		var iflags byte
		if idx.Unique {
			iflags |= 1
		}
		if idx.Predicate != nil {
			iflags |= 2
		}
		out = append(out, iflags)
		out = append(out, byte(idx.Kind)) // v12: index_kind byte (0 = btree, 1 = GIN)
		out = appendU32(out, indexRoots[k])
		// v27: a partial index's predicate canonical text (u16 len + UTF-8) after index_root_page.
		if idx.Predicate != nil {
			out = appendU16(out, uint16(len(idx.Predicate.ExprText)))
			out = append(out, idx.Predicate.ExprText...)
		}
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
	// EXCLUDE constraints (v21): count, then per exclusion the name, the backing GiST index name, and
	// the (column ordinal u16, operator strategy u8) element vector (&& = 0, = 1), in ascending
	// lowercased-name order (spec/design/gist.md §7/§8). The backing index is stored like any GiST
	// index (in the index list above); this entry layers the operator vector the probe needs.
	out = appendU16(out, uint16(len(table.Exclusions)))
	for _, ex := range table.Exclusions {
		out = appendU16(out, uint16(len(ex.Name)))
		out = append(out, ex.Name...)
		out = appendU16(out, uint16(len(ex.Index)))
		out = append(out, ex.Index...)
		out = appendU16(out, uint16(len(ex.Elements)))
		for _, el := range ex.Elements {
			out = appendU16(out, uint16(el.Column))
			out = append(out, exclusionOpCode(el.Op))
		}
	}
	out = appendU32(out, rootDataPage)
	out = appendI64(out, rowCount)
	return out
}

// exclusionOpCode is the 1-byte on-disk code for an EXCLUDE element operator (format.md): && = 0,
// = 1.
func exclusionOpCode(op exclusionOp) byte {
	if op == exclEqual {
		return 1
	}
	return 0
}

// exclusionOpFromCode decodes an EXCLUDE element operator code; an unsupported code in an
// otherwise-valid file is XX001 (reserved for future GiST exclusion operators).
func exclusionOpFromCode(c byte) (exclusionOp, error) {
	switch c {
	case 0:
		return exclOverlaps, nil
	case 1:
		return exclEqual, nil
	default:
		return 0, newError(DataCorrupted, "unsupported exclusion-constraint operator code")
	}
}

// fkActionCode is the 2-bit on-disk code for a referential action (format.md): NO ACTION = 0,
// RESTRICT = 1.
func fkActionCode(a fkAction) byte {
	switch a {
	case fkRestrict:
		return 1
	default:
		return 0
	}
}

// fkActionFromCode decodes a 2-bit referential-action code; an unsupported code (2/3, reserved
// for the deferred write-actions) in an otherwise-valid file is XX001.
func fkActionFromCode(c byte) (fkAction, error) {
	switch c {
	case 0:
		return fkNoAction, nil
	case 1:
		return fkRestrict, nil
	default:
		return 0, newError(DataCorrupted, "unsupported foreign-key action code")
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
			return nil, newError(FeatureNotSupported,
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
func metaPage(pageSize uint32, txid uint64, root, pageCount, freeListHead uint32) []byte {
	p := make([]byte, pageSize)
	copy(p[0:4], magic[:])
	binary.BigEndian.PutUint16(p[4:], formatVersion)
	binary.BigEndian.PutUint32(p[8:], pageSize)
	binary.BigEndian.PutUint64(p[12:], txid)
	binary.BigEndian.PutUint32(p[20:], root)
	binary.BigEndian.PutUint32(p[24:], pageCount)
	binary.BigEndian.PutUint32(p[28:], freeListHead) // v25: the persisted free-list head (0 = empty)
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

// writeMeta writes a meta slot into image (the whole-image path; metaPage is the single source). A
// from-scratch image has an empty free-list, so free_list_head = 0 (v25).
func writeMeta(image []byte, ps, slot int, pageSize uint32, txid uint64, root, pageCount uint32) {
	off := slot * ps
	copy(image[off:off+ps], metaPage(pageSize, txid, root, pageCount, 0))
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
	// freeListHead is the persisted free-list head (v25 — meta offset 28): the first page_type 7 page,
	// or 0 for an empty free-list. Open follows this chain instead of reconstructing the free-list.
	freeListHead uint32
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
	if m[6] != 0 || m[7] != 0 {
		return meta{}, false
	}
	if crc32IEEE(m[0:32]) != binary.BigEndian.Uint32(m[32:36]) {
		return meta{}, false
	}
	pageCount := binary.BigEndian.Uint32(m[24:28])
	// v25: offset 28 is the free-list head — 0 (empty) or a real body page in [2, page_count).
	freeListHead := binary.BigEndian.Uint32(m[28:32])
	if freeListHead != 0 && (freeListHead < rootPage || freeListHead >= pageCount) {
		return meta{}, false
	}
	return meta{
		txid:         binary.BigEndian.Uint64(m[12:20]),
		rootPage:     binary.BigEndian.Uint32(m[20:24]),
		pageCount:    pageCount,
		freeListHead: freeListHead,
	}, true
}

// readMeta validates one meta slot of a whole image; ok=false if it is not a valid meta.
func readMeta(image []byte, ps, slot int) (meta, bool) {
	off := slot * ps
	if off+ps > len(image) {
		return meta{}, false
	}
	return parseMeta(image[off : off+ps])
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
		return meta{}, newError(DataCorrupted, "no valid meta page")
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
		return page{}, newError(DataCorrupted, "page shorter than its header")
	}
	// Verify the per-page checksum (v7) before trusting any header field — a mismatch is silent
	// at-rest corruption (format.md *Page header*; storage.md §6).
	if pageCRC(block) != binary.BigEndian.Uint32(block[12:16]) {
		return page{}, newError(DataCorrupted, "page checksum mismatch (corrupted page)")
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
		return page{}, newError(DataCorrupted, "page index out of range")
	}
	return parsePage(image[off : off+ps])
}

// pageBlock returns one page's full block, copied out of a whole image — the overflow-chain fetch for
// the in-memory load path (readTree, large-values.md §12).
func pageBlock(image []byte, ps int, index uint32) ([]byte, error) {
	off := int(index) * ps
	if off+ps > len(image) {
		return nil, newError(DataCorrupted, "page index out of range")
	}
	out := make([]byte, ps)
	copy(out, image[off:off+ps])
	return out, nil
}

// paxLeaf is a validated PAX (column-major) leaf's page-backed directories + spans (format.md v24
// "Leaf node"). keyEnd and each variable region's ends remain zero-copy views into the page payload.
// paxRegion is one leaf column region's parsed shape (v24 — format.md "Leaf node"),
// class-dependent: a fixed-width region is a null bitmap + dense untagged slots; a variable region
// is an end-offset value directory + tagged codec bytes (NULL = a zero-length span).
type paxRegion struct {
	// width > 0 ⇒ a fixed-width region (bitmap + body slots are meaningful); width == 0 ⇒ a
	// variable region (ends + body blob).
	width  int
	bitmap []byte // fixed: the ceil(N/8) null bitmap span (MSB-first, set = NULL)
	ends   []byte // variable: zero-copy N×u32 big-endian end-offset directory in the page
	body   []byte // the value bodies: N×width dense slots (fixed) or the value blob (variable)
}

type paxLeaf struct {
	keyBlob []byte // the shared key bytes in the retained page payload
	keyEnd  []byte // zero-copy N×u32 big-endian end-offset directory in the retained page
	regions []paxRegion
}

// paxDirEnd reads entry i from a directory that parsePaxLeaf already validated in full. Keeping
// directories as page views removes their per-leaf integer-array allocations while retaining O(1)
// access to keys and variable-width values.
func paxDirEnd(directory []byte, i int) uint32 {
	at := i * 4
	_ = directory[at+3] // one bounds check for the four direct byte reads below
	return uint32(directory[at])<<24 |
		uint32(directory[at+1])<<16 |
		uint32(directory[at+2])<<8 |
		uint32(directory[at+3])
}

// parsePaxLeaf decodes a v24 PAX leaf payload's directories into key spans and per-column regions
// (format.md "Leaf node"). Column regions are validated to be contiguous, in order, within the
// payload, and matching their class shape; a malformed directory, a set region flags bit, or a
// region whose extent disagrees with its class shape is data_corrupted. Because the page body is
// zero-padded to the page size, the authoritative content end is colStart[K], not len(payload).
func parsePaxLeaf(payload []byte, n int, colTypes []colType) (*paxLeaf, error) {
	k := len(colTypes)
	pos := 0
	keyDir := pos
	prev := uint32(0)
	for i := 0; i < n; i++ {
		e, err := readU32(payload, &pos)
		if err != nil {
			return nil, err
		}
		if e < prev {
			return nil, newError(DataCorrupted, "PAX leaf key directory not ascending")
		}
		prev = e
	}
	keyEnd := payload[keyDir:pos]
	keyBlob := pos
	if keyBlob+int(prev) > len(payload) {
		return nil, newError(DataCorrupted, "PAX leaf key blob overruns page")
	}
	keyBytes := payload[keyBlob : keyBlob+int(prev)]
	pos = keyBlob + int(prev)
	colDir := pos
	colRegions := colDir + (k+1)*4
	colPos := colDir
	start32, err := readU32(payload, &colPos)
	if err != nil {
		return nil, err
	}
	if int(start32) != colRegions {
		return nil, newError(DataCorrupted, "PAX leaf column directory start mismatch")
	}
	regions := make([]paxRegion, k)
	start := int(start32)
	for c, ty := range colTypes {
		end32, err := readU32(payload, &colPos)
		if err != nil {
			return nil, err
		}
		end := int(end32)
		if start > end || end > len(payload) {
			return nil, newError(DataCorrupted, "PAX leaf column region out of range")
		}
		p := start
		flags, err := readU8(payload, &p)
		if err != nil {
			return nil, err
		}
		if flags != 0 {
			return nil, newError(DataCorrupted, "PAX leaf region flags has a reserved bit set")
		}
		if w, fixed := fixedValueWidth(ty); fixed {
			body := p + (n+7)/8
			if body > end || body+n*w != end {
				return nil, newError(DataCorrupted, "PAX leaf fixed region extent mismatch")
			}
			regions[c] = paxRegion{width: w, bitmap: payload[p:body], body: payload[body:end]}
		} else {
			endsStart := p
			vprev := uint32(0)
			for i := 0; i < n; i++ {
				e, err := readU32(payload, &p)
				if err != nil {
					return nil, err
				}
				if e < vprev {
					return nil, newError(DataCorrupted, "PAX leaf value directory not ascending")
				}
				vprev = e
			}
			if p > end || p+int(vprev) != end {
				return nil, newError(DataCorrupted, "PAX leaf variable region extent mismatch")
			}
			regions[c] = paxRegion{ends: payload[endsStart:p], body: payload[p:end]}
		}
		start = end
	}
	return &paxLeaf{keyBlob: keyBytes, keyEnd: keyEnd, regions: regions}, nil
}

// key returns record i's key as a borrowed span of the retained page block.
func (l *paxLeaf) key(i int) []byte {
	lo := uint32(0)
	if i > 0 {
		lo = paxDirEnd(l.keyEnd, i-1)
	}
	return l.keyBlob[lo:paxDirEnd(l.keyEnd, i)]
}

// isNull reports whether value (record i, column c) is NULL — the region bitmap (fixed-width) or
// the zero-length span (variable), with NO value decode (format.md "Leaf node").
func (l *paxLeaf) isNull(c, i int) bool {
	r := &l.regions[c]
	if r.width > 0 {
		return r.bitmap[i/8]&(0x80>>(i%8)) != 0
	}
	lo := uint32(0)
	if i > 0 {
		lo = paxDirEnd(r.ends, i-1)
	}
	return paxDirEnd(r.ends, i) == lo
}

// value returns the bytes of value (record i, column c), a view into the page payload: a
// fixed-width slot (the untagged body) or a variable value's tagged codec bytes. A NULL fixed
// slot is zero-filled and must never be read (the bitmap is the sole authority — isNull first).
func (l *paxLeaf) value(c, i int) ([]byte, error) {
	r := &l.regions[c]
	if r.width > 0 {
		return r.body[i*r.width : (i+1)*r.width], nil
	}
	lo := uint32(0)
	if i > 0 {
		lo = paxDirEnd(r.ends, i-1)
	}
	hi := paxDirEnd(r.ends, i)
	if int(hi) > len(r.body) {
		return nil, newError(DataCorrupted, "PAX leaf value offset out of range")
	}
	return r.body[lo:hi], nil
}

// valueLen is the bytes value (record i, column c) contributes to recordSize — the slot width
// (fixed-width, NULL included) or the span length (variable; 0 for NULL). Derivable from the
// directories alone, with no value decode (packed-leaf.md §3/§5).
func (l *paxLeaf) valueLen(c, i int) int {
	r := &l.regions[c]
	if r.width > 0 {
		return r.width
	}
	lo := uint32(0)
	if i > 0 {
		lo = paxDirEnd(r.ends, i-1)
	}
	return int(paxDirEnd(r.ends, i) - lo)
}

// packedLeaf is a faulted leaf's block-backed resident form (packed-leaf.md §5): the validated PAX
// directory views plus the table's column types, retained instead of
// discarded. It holds NO decoded storedRow — row/value reconstruct on demand via readValueLazy over
// the O(1) column spans. Because the directory/blob fields are sub-slices of the page block, retaining them
// keeps that block alive (Go GC — the equivalent of Rust's Arc<[u8]> pin), so a resident leaf is
// ≈ pageSize for fixed-width and variable-length data alike (§9). colTypes is a shared slice header
// (the table's list), so a resident leaf copies no column types.
type packedLeaf struct {
	dirs     *paxLeaf
	colTypes []colType
	// paging is the database's shared pager — stamped into each deferred external value so it can
	// self-resolve at the evaluator's column access (the B4 demand-fault backstop,
	// bplus-reshape.md §5). A plain pointer (GC — no Rust-style weak ref needed).
	paging *sharedPaging
	n      int
}

func (p *packedLeaf) key(i int) []byte { return p.dirs.key(i) }

// recordWeight derives the exact serialized record size only when mutation/rebalance needs it.
func (p *packedLeaf) recordWeight(i int) uint32 {
	w := len(p.key(i))
	for c := range p.colTypes {
		w += p.dirs.valueLen(c, i)
	}
	return uint32(w)
}

// value reconstructs value (record i, column c) — the O(1) PAX column span (packed-leaf.md §4). A
// NULL is answered from the region bitmap / zero-length span with no decode. A fixed-width slot is
// the untagged inline body, decoded eagerly (deferring a fixed-width scalar buys nothing,
// lazy-record.md §6); a variable value's span takes the lazy tag path — a spillable body becomes an
// inline-deferred Unfetched (a block view). Byte-identical to the eager value, moved from
// fault-time to touch-time (§8); a corrupt touched inline body surfaces XX001 here.
func (p *packedLeaf) value(c, i int) (Value, error) {
	if p.dirs.isNull(c, i) {
		return NullValue(), nil
	}
	vb, err := p.dirs.value(c, i)
	if err != nil {
		return Value{}, err
	}
	pos := 0
	if _, fixed := fixedValueWidth(p.colTypes[c]); fixed {
		return readInlineBody(p.colTypes[c], vb, &pos, decodeConstruct)
	}
	return readValueLazy(p.colTypes, c, vb, &pos, p.paging)
}

// row reconstructs the whole value row i (every column). The rowAt whole-record path.
func (p *packedLeaf) row(i int) (storedRow, error) {
	row := make(storedRow, len(p.colTypes))
	for c := range p.colTypes {
		v, err := p.value(c, i)
		if err != nil {
			return nil, err
		}
		row[c] = v
	}
	return row, nil
}

// decodeLeafNode decodes a single leaf page block into a resident node, for the demand-paging fault
// path (spec/design/pager.md §4; paging.go faultLeaf). block is one page; page is its page id, stamped
// on the node so a later incremental commit keeps it clean. Decoding is LAZY (large-values.md §14):
// an external/compressed value becomes an Unfetched reference — no chain read, no decompression —
// resolved later only for the columns a query touches (or on demand at the evaluator's column
// access — the B4 backstop; paging is stamped into every deferred value for that).
func decodeLeafNode(block []byte, pageID uint32, colTypes []colType, paging *sharedPaging) (*pnode, error) {
	pg, err := parsePage(block)
	if err != nil {
		return nil, err
	}
	if pg.pageType != pageLeaf {
		return nil, newError(DataCorrupted, "demand-paged a non-leaf page")
	}
	n := int(pg.itemCount)
	leaf, err := parsePaxLeaf(pg.payload, n, colTypes)
	if err != nil {
		return nil, err
	}
	// Packed form (packed-leaf.md §5): retain the block (via the paxLeaf's block-view slices) + the
	// PAX directories, decode NO values. parsePaxLeaf validated + parsed the directories in one pass
	// with no value decode, so a malformed directory still surfaces data_corrupted here; a malformed
	// value body surfaces XX001 only when the column is touched (§8). Keys and weights are derivable
	// from the directories alone (§3): the weight is len(key) + Σ_c valueLen(c, i) (the v24
	// record_size), exactly what the writer split on — so the resident leaf is ≈ pageSize (§9),
	// never an inflated row vector.
	packed := &packedLeaf{dirs: leaf, colTypes: colTypes, paging: paging, n: n}
	return &pnode{packed: packed, page: pageID}, nil
}

// decodeTableEntry decodes one catalog table entry: the *Table (its pk list, checks, and
// index definitions included), its root_data_page, and each index's root page (parallel
// to Table.Indexes).
func decodeTableEntry(buf []byte, pos *int, rowCountOut *int64) (*catTable, uint32, []uint32, error) {
	name, err := readString(buf, pos)
	if err != nil {
		return nil, 0, nil, err
	}
	colCount, err := readU16(buf, pos)
	if err != nil {
		return nil, 0, nil, err
	}
	columns := make([]catColumn, 0, colCount)
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
				return nil, 0, nil, newError(DataCorrupted, "reserved column flag bit0 set")
			}
			tname, err := readString(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			columns = append(columns, catColumn{
				Name:    cname,
				Type:    compositeT(tname),
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
				return nil, 0, nil, newError(DataCorrupted, "reserved column flag bit0 set")
			}
			elem, err := readArrayElementType(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			columns = append(columns, catColumn{
				Name:    cname,
				Type:    arrayT(elem),
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
				return nil, 0, nil, newError(DataCorrupted, "reserved column flag bit0 set")
			}
			elem, err := readRangeElementType(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			columns = append(columns, catColumn{
				Name:    cname,
				Type:    rangeT(elem),
				NotNull: flags&0b10 != 0,
			})
			continue
		}
		ty, ok := scalarForTypeCode(tc)
		if !ok {
			return nil, 0, nil, newError(DataCorrupted, "unknown type code")
		}
		flags, err := readU8(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		// bit0 was the primary_key flag through v4; v5 retired it (the pk list below is
		// the authority) and reserves it as must-be-zero. bit6 = has_collation (v17); bit7 reserved.
		if flags&0b01 != 0 {
			return nil, 0, nil, newError(DataCorrupted, "reserved column flag bit0 set")
		}
		if flags&0b1000_0000 != 0 {
			return nil, 0, nil, newError(DataCorrupted, "reserved column flag bit7 set")
		}
		// bit4 is_identity + bit5 identity_always (v15) — identity_always is meaningful only with
		// is_identity (spec/design/sequences.md §13).
		if flags&0b11_0000 == 0b10_0000 {
			return nil, 0, nil, newError(DataCorrupted, "identity_always set without is_identity")
		}
		var identity *identityKind
		if flags&0b1_0000 != 0 {
			k := identityByDefault
			if flags&0b10_0000 != 0 {
				k = identityAlways
			}
			identity = &k
		}
		// A decimal column carries its typmod (precision, scale); precision 0 = unconstrained.
		var decimal *decimalTypmod
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
				decimal = &decimalTypmod{Precision: precision, Scale: scale}
			}
		}
		// A text column carries its varchar(n) max length (v22); 0 = unbounded (types.md §15).
		var varcharLen *uint32
		if ty.IsText() {
			n, err := readU32(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			if n != 0 {
				varcharLen = &n
			}
		}
		// The default follows the typmod (spec/fileformat/format.md): a CONSTANT default (flags
		// bit2) is a value via the same value codec rows use — never externalized, so no
		// overflow reader is needed (a 0x02 tag here would be a corrupt catalog). An EXPRESSION
		// default (flags bit3, v8) is instead the expr-text (u16 length + UTF-8), re-parsed with
		// the ordinary expression parser (XX001 if it fails, like a stored check). The two bits
		// are mutually exclusive — both set is a corrupt catalog.
		if flags&0b1100 == 0b1100 {
			return nil, 0, nil, newError(DataCorrupted, "column has both a constant and an expression default")
		}
		var defaultVal *Value
		if flags&0b100 != 0 {
			var sink []uint32
			// A constant default is a scalar value (this branch is the scalar type path).
			dv, err := readValue(scalarColType(ty), buf, pos, nil, &sink)
			if err != nil {
				return nil, 0, nil, err
			}
			defaultVal = &dv
		}
		var defaultExpr *defaultExprDef
		if flags&0b1000 != 0 {
			exprText, err := readString(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			expr, err := parseExpression(exprText)
			if err != nil {
				return nil, 0, nil, newError(DataCorrupted, "stored default expression does not parse: "+err.Error())
			}
			defaultExpr = &defaultExprDef{ExprText: exprText, Expr: expr}
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
		columns = append(columns, catColumn{
			Name:       cname,
			Type:       scalarT(ty),
			Decimal:    decimal,
			VarcharLen: varcharLen,
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
			return nil, 0, nil, newError(DataCorrupted, "invalid primary key ordinal")
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
	checks := make([]checkConstraint, 0, checkCount)
	for i := uint16(0); i < checkCount; i++ {
		checkName, err := readString(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		exprText, err := readString(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		expr, err := parseExpression(exprText)
		if err != nil {
			return nil, 0, nil, newError(DataCorrupted,
				"stored check constraint does not parse: "+err.Error())
		}
		checks = append(checks, checkConstraint{Name: checkName, ExprText: exprText, Expr: expr})
	}
	// Secondary indexes (v5): name + key-column ordinals + the v6 flags byte (bit0
	// unique; the rest reserved-zero) + root page, in the catalog's (lowercased-name
	// ascending) order — a reader trusts the order. Duplicate ordinals within one index
	// are legal (indexes.md §1).
	indexCount, err := readU16(buf, pos)
	if err != nil {
		return nil, 0, nil, err
	}
	indexes := make([]indexDef, 0, indexCount)
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
			return nil, 0, nil, newError(DataCorrupted, "index with no key columns")
		}
		keys := make([]indexKey, 0, kc)
		for j := uint16(0); j < kc; j++ {
			ord, err := readU16(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			if ord == 0xFFFF {
				// An expression key (v26): the sentinel, then the canonical text; re-parse it
				// (XX001 on failure, like a stored CHECK — spec/design/indexes.md §6).
				text, err := readString(buf, pos)
				if err != nil {
					return nil, 0, nil, err
				}
				expr, perr := parseExpression(text)
				if perr != nil {
					return nil, 0, nil, newError(DataCorrupted, "unparseable index expression")
				}
				keys = append(keys, indexKey{Expr: &indexKeyExpr{ExprText: text, Expr: expr}})
				continue
			}
			if int(ord) >= len(columns) {
				return nil, 0, nil, newError(DataCorrupted, "invalid index column ordinal")
			}
			keys = append(keys, indexKey{Col: int(ord)})
		}
		iflags, err := readU8(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		// bit0 unique (v6), bit1 has_predicate (v27 — a partial index, indexes.md §9); rest reserved.
		if iflags&^uint8(0b11) != 0 {
			return nil, 0, nil, newError(DataCorrupted, "reserved index flag set")
		}
		ikind, err := readU8(buf, pos) // v13: index_kind byte (0 = btree, 1 = GIN); v20: 2 = GiST
		if err != nil {
			return nil, 0, nil, err
		}
		if ikind > 2 {
			return nil, 0, nil, newError(DataCorrupted, "unsupported index kind")
		}
		// A GIN/GiST index is single-column plain (this slice): an expression key on either is
		// structurally impossible in a valid file.
		if indexKind(ikind) != indexBtree {
			for _, k := range keys {
				if k.Expr != nil {
					return nil, 0, nil, newError(DataCorrupted, "a non-btree index cannot have an expression key")
				}
			}
		}
		hasPredicate := iflags&0b10 != 0
		// A partial index is B-tree only (indexes.md §9): bit1 with a GIN/GiST kind is corrupt.
		if hasPredicate && indexKind(ikind) != indexBtree {
			return nil, 0, nil, newError(DataCorrupted, "a non-btree index cannot be partial")
		}
		iroot, err := readU32(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		// v27: the partial-index predicate canonical text follows index_root_page (bit1 set) — re-parse
		// it (XX001 on failure, like a stored CHECK — spec/design/indexes.md §9).
		var predicate *indexKeyExpr
		if hasPredicate {
			text, perr := readString(buf, pos)
			if perr != nil {
				return nil, 0, nil, perr
			}
			expr, perr := parseExpression(text)
			if perr != nil {
				return nil, 0, nil, newError(DataCorrupted, "unparseable index predicate")
			}
			predicate = &indexKeyExpr{ExprText: text, Expr: expr}
		}
		indexes = append(indexes, indexDef{Name: iname, Keys: keys, Unique: iflags&0b01 != 0, Kind: indexKind(ikind), Predicate: predicate})
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
	foreignKeys := make([]foreignKey, 0, fkCount)
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
			return nil, 0, nil, newError(DataCorrupted, "foreign key with no columns")
		}
		cols := make([]int, 0, lc)
		for j := uint16(0); j < lc; j++ {
			ord, err := readU16(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			if int(ord) >= len(columns) {
				return nil, 0, nil, newError(DataCorrupted, "invalid foreign-key column ordinal")
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
			return nil, 0, nil, newError(DataCorrupted, "foreign-key referencing/referenced column count mismatch")
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
			return nil, 0, nil, newError(DataCorrupted, "reserved foreign-key action bit set")
		}
		onDelete, err := fkActionFromCode(actions & 0b11)
		if err != nil {
			return nil, 0, nil, err
		}
		onUpdate, err := fkActionFromCode((actions >> 2) & 0b11)
		if err != nil {
			return nil, 0, nil, err
		}
		foreignKeys = append(foreignKeys, foreignKey{
			Name:       fname,
			Columns:    cols,
			RefTable:   refTable,
			RefColumns: refCols,
			OnDelete:   onDelete,
			OnUpdate:   onUpdate,
		})
	}
	// EXCLUDE constraints (v21): name + backing GiST index name + the (column ordinal, operator)
	// element vector, in name order (spec/design/gist.md §7/§8).
	excCount, err := readU16(buf, pos)
	if err != nil {
		return nil, 0, nil, err
	}
	exclusions := make([]exclusionConstraint, 0, excCount)
	for i := uint16(0); i < excCount; i++ {
		ename, err := readString(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		iname, err := readString(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		ec, err := readU16(buf, pos)
		if err != nil {
			return nil, 0, nil, err
		}
		if ec == 0 {
			return nil, 0, nil, newError(DataCorrupted, "exclusion constraint with no elements")
		}
		elements := make([]exclusionElement, 0, ec)
		for j := uint16(0); j < ec; j++ {
			ord, err := readU16(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			if int(ord) >= len(columns) {
				return nil, 0, nil, newError(DataCorrupted, "invalid exclusion-constraint column ordinal")
			}
			opb, err := readU8(buf, pos)
			if err != nil {
				return nil, 0, nil, err
			}
			op, err := exclusionOpFromCode(opb)
			if err != nil {
				return nil, 0, nil, err
			}
			elements = append(elements, exclusionElement{Column: int(ord), Op: op})
		}
		exclusions = append(exclusions, exclusionConstraint{Name: ename, Index: iname, Elements: elements})
	}
	root, err := readU32(buf, pos)
	if err != nil {
		return nil, 0, nil, err
	}
	rowCount, err := readI64(buf, pos)
	if err != nil {
		return nil, 0, nil, err
	}
	if rowCount < 0 {
		return nil, 0, nil, newError(DataCorrupted, "negative table row count")
	}
	if (root == 0) != (rowCount == 0) {
		return nil, 0, nil, newError(DataCorrupted, "table root and row count disagree")
	}
	*rowCountOut = rowCount
	return &catTable{Name: name, Columns: columns, PK: pk, Checks: checks, Indexes: indexes, ForeignKeys: foreignKeys, Exclusions: exclusions}, root, indexRoots, nil
}

// readValueLazy reads one value (column c of a table with the shared resolved types) lazily
// (spec/design/large-values.md §14): inline-plain and NULL decode as today, but an
// external/compressed form becomes an Unfetched reference holding exactly the record's pointer
// fields — no chain read, no decompression. Every deferred reference is stamped with its
// resolution handles (types+c always, paging on the external forms — bplus-reshape.md §5, B4) so
// it can self-resolve at the evaluator's column access. The scan layer resolves the references
// for the columns a query touches (resolveUnfetched); the commit path resolves the rest when a
// dirty leaf re-encodes (resolveForEncode); a touched-set miss resolves on demand
// (resolveUnfetchedSelf).
func readValueLazy(types []colType, c int, buf []byte, pos *int, paging *sharedPaging) (Value, error) {
	ty := types[c]
	tag, err := readU8(buf, pos)
	if err != nil {
		return Value{}, err
	}
	switch tag {
	case 0x00:
		// A present inline value (lazy-record.md §12, L3): a variable-length / structured body (the
		// isSpillable set — §6) is DEFERRED as an Unfetched (Form 0x00) referencing the shared page
		// block — FORM (a), zero-copy (§5a): keep the span as a SLICE of the faulted page block
		// instead of copying it. A Go slice keeps its backing array alive under GC, so the leaf's one
		// page block stays resident and is shared by every deferred value in it (the scan-emit clone
		// is then a slice-header copy, never a byte copy) — resident leaf memory tracks ≈ pageSize,
		// the honest buffer-pool bound (§9). The block is read fresh per fault (blockstore.readAt)
		// and never mutated after decode (copy-on-write commits write new pages), so the view is
		// stable. A fixed-width scalar is decoded eagerly (deferring it buys nothing — §6).
		// resolveUnfetched reconstructs a touched one from the span, byte-identically (readInlineBody
		// in construct mode).
		if isSpillable(ty) {
			span, err := inlineBodySpan(ty, buf, pos)
			if err != nil {
				return Value{}, err
			}
			return Value{Kind: ValUnfetched, ref: &Unfetched{Form: 0x00, Comp: span, types: types, typeIdx: c}}, nil
		}
		return readInlineBody(ty, buf, pos, decodeConstruct)
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
		return Value{Kind: ValUnfetched, ref: &Unfetched{Form: tagExternal, FirstPage: first, StoredLen: length, types: types, typeIdx: c, paging: paging}}, nil
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
		return Value{Kind: ValUnfetched, ref: &Unfetched{Form: tagInlineComp, RawLen: rawLen, Comp: comp, types: types, typeIdx: c}}, nil
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
		return Value{Kind: ValUnfetched, ref: &Unfetched{Form: tagExternalComp, FirstPage: first, StoredLen: stored, RawLen: rawLen, types: types, typeIdx: c, paging: paging}}, nil
	default:
		return Value{}, newError(DataCorrupted, "invalid value presence tag")
	}
}

// resolveUnfetched materializes an unfetched reference into its plain Value
// (spec/design/large-values.md §14): gather the overflow chain through fetch for an external
// form, decompress a compressed one, and reconstruct by column type. Decompression errors are
// data_corrupted, surfaced only when the value is actually touched.
func resolveUnfetched(ty colType, u *Unfetched, fetch func(uint32) ([]byte, error)) (Value, error) {
	var sink []uint32
	switch u.Form {
	case 0x00:
		// Inline-deferred (lazy-record.md §5b, L2): the bytes are already owned — no chain read, no
		// decompression. Re-run the decoder over the captured span in construct mode.
		p := 0
		return readInlineBody(ty, u.Comp, &p, decodeConstruct)
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
		return Value{}, newError(DataCorrupted, "invalid unfetched value form")
	}
}

// resolveUnfetchedSelf resolves a deferred value FROM ITS OWN CARRIED HANDLES — the B4
// demand-fault backstop (bplus-reshape.md §5/§6): the evaluator's column access calls this when
// the static touched set missed a value, so a prediction miss is a deterministic on-demand fetch —
// never a NULL-fold, never wrong rows. The fetch is deliberately UNMETERED (metering it would make
// cost depend on prediction quality rather than the spec'd static set — §6); the touched set stays
// the cost basis + prefetch hint. A spill-run-file reload carries the nil sentinel handles
// (spill.go — it rides the sort output unread by contract), so touching one stays the loud pre-B4
// poison.
func resolveUnfetchedSelf(u *Unfetched) (Value, error) {
	if u.types == nil {
		panic("BUG: unfetched large value escaped the storage layer (spill pass-through)")
	}
	ty := u.types[u.typeIdx]
	switch u.Form {
	case 0x00, tagInlineComp:
		// Inline forms own their bytes — no pager involved (resolveUnfetched never calls fetch).
		return resolveUnfetched(ty, u, nil)
	default:
		// A deferred external value is reachable only through a snapshot whose stores hold the
		// paging pointer, so a stamped handle is always live while the value is observable.
		if u.paging == nil {
			panic("BUG: deferred external value carries no pager handle")
		}
		fetch := func(p uint32) ([]byte, error) { return u.paging.readBlock(p) }
		return resolveUnfetched(ty, u, fetch)
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
			return nil, newError(DataCorrupted, "overflow chain ended before the value length")
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
			return nil, newError(DataCorrupted, "expected an overflow page")
		}
		n := int(pg.itemCount)
		if n == 0 || n > len(pg.payload) || gathered+n > length {
			return nil, newError(DataCorrupted, "overflow page slab out of range")
		}
		gathered += n
		p = pg.nextPage
	}
	return out, nil
}

// markChains adds the overflow chain pages a lazily-decoded row references to reached (the
// free-list reachability walk), via the header-only chainPages hop.
func markChains(row storedRow, fetch func(uint32) ([]byte, error), reached map[uint32]bool) error {
	for _, v := range row {
		if v.Kind != ValUnfetched {
			continue
		}
		switch v.unfetched().Form {
		case tagExternal, tagExternalComp:
			pages, err := chainPages(v.unfetched().FirstPage, int(v.unfetched().StoredLen), fetch)
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

// readValue reads one value via the value codec (inverse of encodeValue). The presence tag is read
// first: 0x00 an inline body, 0x01 NULL, 0x02 an external pointer (u32 first_page + u32 len) whose
// payload is gathered from the overflow chain via fetch and reconstructed by type (large-values.md
// §12). Pages visited while following a chain are appended to *ovfOut for the free-list walk.
func readValue(ty colType, buf []byte, pos *int, fetch func(uint32) ([]byte, error), ovfOut *[]uint32) (Value, error) {
	tag, err := readU8(buf, pos)
	if err != nil {
		return Value{}, err
	}
	switch tag {
	case 0x00:
		return readInlineBody(ty, buf, pos, decodeConstruct)
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
			return Value{}, newError(DataCorrupted, "external value with no overflow reader")
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
			return Value{}, newError(DataCorrupted, "external value with no overflow reader")
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
		return Value{}, newError(DataCorrupted, "invalid value presence tag")
	}
}

// decodeMode selects whether the value decoder CONSTRUCTS each leaf Value (decodeConstruct) or
// merely ADVANCES the cursor past its body (decodeSkip) — spec/design/lazy-record.md §6. Both modes
// run the identical cursor-advancing reads (the same length / tag / count reads and the same
// recursion), so a skip walk finds every column boundary identically to a construct decode by
// construction: the zero-drift property the lazy-record reshape rests on. Skip omits only the
// expensive leaf construction (the string copy + UTF-8 validation, the Decimal, the JsonNode /
// []Value tree) and the content validation bundled with it; fixed-width scalars are cheap and stay
// eager either way (§6). A skip-mode return is an unobserved placeholder — callers use only the
// advanced cursor (see inlineBodySpan).
type decodeMode int

const (
	decodeConstruct decodeMode = iota
	decodeSkip
)

// constructs reports whether the mode builds the Value (decodeConstruct) rather than only advancing
// past it (decodeSkip).
func (m decodeMode) constructs() bool { return m == decodeConstruct }

// readInlineBody reads the present-value body (after a 0x00 tag) for any ColType: a scalar via
// readInlineScalar, or a composite via readCompositeBody (spec/design/composite.md §4). mode selects
// construct vs. skip (decodeMode).
func readInlineBody(ty colType, buf []byte, pos *int, mode decodeMode) (Value, error) {
	if ty.Elem != nil {
		return readArrayBody(ty, buf, pos, mode)
	}
	if ty.RangeElem != nil {
		return readRangeBody(*ty.RangeElem, buf, pos, mode)
	}
	if ty.Composite {
		return readCompositeBody(ty, buf, pos, mode)
	}
	return readInlineScalar(ty.Scalar, buf, pos, mode)
}

// inlineBodySpan walks a present inline value body in decodeSkip and returns its byte span WITHOUT
// constructing the value (spec/design/lazy-record.md §6). The caller has already consumed the 0x00
// present tag; this advances *pos past the body exactly as readInlineBody in construct mode would —
// the same length reads, tag dispatch, and recursion — so the returned span equals the bytes a
// construct decode consumes, by construction (the zero-drift property). L2 will use this to defer an
// inline value as its compact on-disk bytes; at L1 it is the seam, exercised by the cross-check
// test inlineBodySpanMatchesDecode.
func inlineBodySpan(ty colType, buf []byte, pos *int) ([]byte, error) {
	start := *pos
	if _, err := readInlineBody(ty, buf, pos, decodeSkip); err != nil {
		return nil, err
	}
	return buf[start:*pos], nil
}

// readRangeBody reads a range value's present body (after the 0x00 tag): inverse of encodeRangeBody
// (spec/design/ranges.md §4). Reads the flags byte; an EMPTY range stops there. Otherwise the finite
// lower bound (!LB_INF) then the finite upper bound (!UB_INF) are each read as the element's
// value-codec body (no presence tag). A reserved flag bit set is XX001.
func readRangeBody(elem colType, buf []byte, pos *int, mode decodeMode) (Value, error) {
	flags, err := readU8(buf, pos)
	if err != nil {
		return Value{}, err
	}
	if flags&^0x1f != 0 {
		return Value{}, newError(DataCorrupted, "range flags has a reserved bit set")
	}
	if flags&0x01 != 0 {
		if !mode.constructs() {
			return NullValue(), nil // skip-mode placeholder (no bounds follow)
		}
		return RangeValue(emptyRangeVal()), nil
	}
	lbInf := flags&0x02 != 0
	ubInf := flags&0x04 != 0
	// Each present bound is advanced past in both modes (the recursion is the cursor advance); only
	// construct mode keeps it.
	var lower, upper *Value
	if !lbInf {
		v, err := readInlineBody(elem, buf, pos, mode)
		if err != nil {
			return Value{}, err
		}
		if mode.constructs() {
			lower = &v
		}
	}
	if !ubInf {
		v, err := readInlineBody(elem, buf, pos, mode)
		if err != nil {
			return Value{}, err
		}
		if mode.constructs() {
			upper = &v
		}
	}
	if !mode.constructs() {
		return NullValue(), nil
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
func readArrayBody(ty colType, buf []byte, pos *int, mode decodeMode) (Value, error) {
	if ty.Elem == nil {
		return Value{}, newError(DataCorrupted, "readArrayBody on a non-array type")
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
		return Value{}, newError(DataCorrupted, "array flags has a reserved bit set")
	}
	if ndim == 0 {
		if !mode.constructs() {
			return NullValue(), nil // skip-mode placeholder
		}
		// An empty array (ndim 0) — all-empty slices.
		return arrayValueOf(emptyArray()), nil
	}
	if ndim > 6 {
		return Value{}, newError(DataCorrupted, "array ndim exceeds the maximum of 6")
	}
	// dims/lbounds/bitmap are small and structural (n drives the loop, bitmap drives null handling),
	// so they are read in both modes — not the expensive leaf construction §6 skips.
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
			return Value{}, newError(DataCorrupted, "array element count overflow")
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
	var elems []Value
	if mode.constructs() {
		elems = make([]Value, n)
	}
	for i := 0; i < n; i++ {
		if hasNulls && bitmap[i/8]&(0x80>>uint(i%8)) != 0 {
			if mode.constructs() {
				elems[i] = NullValue()
			}
		} else {
			v, err := readInlineBody(*ty.Elem, buf, pos, mode) // advance in both modes
			if err != nil {
				return Value{}, err
			}
			if mode.constructs() {
				elems[i] = v
			}
		}
	}
	if !mode.constructs() {
		return NullValue(), nil
	}
	return arrayValueOf(&ArrayVal{Dims: dims, Lbounds: lbounds, Elements: elems}), nil
}

// readCompositeBody reads a composite value's present body (after the 0x00 tag): the null bitmap then
// each present field's body in declaration order (inverse of encodeCompositeBody,
// spec/design/composite.md §4). A field whose bitmap bit is set is NULL and consumes no body bytes;
// otherwise its body is read recursively (no per-field presence tag).
func readCompositeBody(ty colType, buf []byte, pos *int, mode decodeMode) (Value, error) {
	if !ty.Composite {
		return Value{}, newError(DataCorrupted, "readCompositeBody on a non-composite type")
	}
	nbytes := (len(ty.Fields) + 7) / 8
	bitmap, err := take(buf, pos, nbytes) // structural — drives null handling
	if err != nil {
		return Value{}, err
	}
	var vals []Value
	if mode.constructs() {
		vals = make([]Value, len(ty.Fields))
	}
	for i := range ty.Fields {
		if bitmap[i/8]&(0x80>>uint(i%8)) != 0 {
			if mode.constructs() {
				vals[i] = NullValue()
			}
		} else {
			v, err := readInlineBody(ty.Fields[i].Type, buf, pos, mode) // advance in both modes
			if err != nil {
				return Value{}, err
			}
			if mode.constructs() {
				vals[i] = v
			}
		}
	}
	if !mode.constructs() {
		return NullValue(), nil
	}
	return CompositeValue(vals), nil
}

// readInlineScalar reads the present-value body of a SCALAR (after a 0x00 tag): a fixed-width integer,
// a u16 length + UTF-8 bytes for text, a single bool-byte, the decimal body, etc. (format.md *Value
// codec*).
func readInlineScalar(ty scalarType, buf []byte, pos *int, mode decodeMode) (Value, error) {
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
		if !mode.constructs() {
			return NullValue(), nil // skip: no copy, no UTF-8 validation (lazy-record.md §6)
		}
		if !utf8.Valid(sb) {
			return Value{}, newError(DataCorrupted, "non-UTF-8 text value")
		}
		return TextValue(string(sb)), nil
	case ty.IsBool():
		// Fixed-width (1 byte) — decoded eagerly even on the lazy path (§6); the validity check is
		// cheap and harmless in either mode.
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
			return Value{}, newError(DataCorrupted, "invalid boolean value byte")
		}
	case ty.IsDecimal():
		return decodeDecimalBody(buf, pos, mode)
	case ty.IsBytea():
		n, err := readU16(buf, pos)
		if err != nil {
			return Value{}, err
		}
		bb, err := take(buf, pos, int(n))
		if err != nil {
			return Value{}, err
		}
		if !mode.constructs() {
			return NullValue(), nil // skip: no copy
		}
		// ByteaValue copies the bytes into a string, so the value owns its content.
		return ByteaValue(bb), nil
	case ty.IsJson():
		// json: verbatim text, length-prefixed exactly like text (spec/design/json.md §4).
		n, err := readU16(buf, pos)
		if err != nil {
			return Value{}, err
		}
		sb, err := take(buf, pos, int(n))
		if err != nil {
			return Value{}, err
		}
		if !mode.constructs() {
			return NullValue(), nil // skip: no copy, no UTF-8 validation
		}
		if !utf8.Valid(sb) {
			return Value{}, newError(DataCorrupted, "non-UTF-8 json value")
		}
		return JsonValue(string(sb)), nil
	case ty.IsJsonb():
		// jsonb: the self-delimiting tagged-node tree (spec/design/json.md §2).
		node, err := decodeJsonbBody(buf, pos, mode)
		if err != nil {
			return Value{}, err
		}
		if !mode.constructs() {
			return NullValue(), nil // skip: tree walked, not built
		}
		return JsonbValue(node), nil
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
		m := decodeInt(ty, vb)
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
		return DateValue(int32(decodeInt(ty, vb))), nil
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
		return IntValue(decodeInt(ty, vb)), nil
	}
}

// decodeDecimalBody decodes a decimal value's body — flags (sign), u16 scale, u16 ndigits, then that
// many base-10^4 groups (format.md). Shared by the inline path and by external reconstruction (a
// spilled decimal's chain payload is exactly this body — large-values.md §12).
func decodeDecimalBody(buf []byte, pos *int, mode decodeMode) (Value, error) {
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
	var groups []uint16
	if mode.constructs() {
		groups = make([]uint16, ndigits)
	}
	for i := 0; i < int(ndigits); i++ {
		g, err := readU16(buf, pos) // advance in both modes
		if err != nil {
			return Value{}, err
		}
		if mode.constructs() {
			groups[i] = g
		}
	}
	if !mode.constructs() {
		return NullValue(), nil // skip-mode placeholder (no Decimal built)
	}
	return DecimalValue(decimalFromCodec(flags&1 != 0, uint32(scale), groups)), nil
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
			return nil, newError(DataCorrupted, "overflow chain ended before the value length")
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
			return nil, newError(DataCorrupted, "expected an overflow page")
		}
		n := int(pg.itemCount)
		if n == 0 || n > len(pg.payload) || len(out)+n > length {
			return nil, newError(DataCorrupted, "overflow page slab out of range")
		}
		out = append(out, pg.payload[:n]...)
		p = pg.nextPage
	}
	return out, nil
}

// --- bounds-checked big-endian readers over a payload cursor ---

func take(buf []byte, pos *int, n int) ([]byte, error) {
	if *pos+n > len(buf) {
		return nil, newError(DataCorrupted, "unexpected end of page data")
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
		return "", newError(DataCorrupted, "non-UTF-8 name")
	}
	return string(s), nil
}
