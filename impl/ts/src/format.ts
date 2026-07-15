// On-disk single-file format: serialize / load (spec/fileformat/format.md).
//
// Whole-image model (step-5b): a commit serializes the entire database to one byte
// image; loading reconstructs it. The byte layout is the canonical contract and is
// verified byte-for-byte against shared goldens, so a file written by this core is
// byte-identical to one written by the Rust and Go cores (CLAUDE.md §8). All multi-byte
// integers are big-endian.
//
// JS hazards handled here: u64 txid via DataView.setBigUint64 (bigint); UTF-8 names via
// TextEncoder/TextDecoder (name_len is the UTF-8 byte length, not String#length);
// big-endian via DataView (never host order); CRC-32 backend results normalized unsigned.

import type { Expr } from "./ast.ts";
import {
  type CheckConstraint,
  type ColField,
  type ColType,
  type Column,
  type CompositeField,
  type CompositeType,
  type DefaultExpr,
  type ExclusionConstraint,
  type ExclusionElement,
  type FkAction,
  type ForeignKey,
  type IdentityKind,
  type IndexDef,
  type IndexKey,
  type SeqOwner,
  type SequenceDef,
  type Table,
  indexColumnOrdinals,
  pkIndices,
} from "./catalog.ts";
import { parseExpression } from "./parser.ts";
import { type Collation, loadedCollation } from "./collation.ts";
import { Decimal } from "./decimal.ts";
import { crc32Ieee, crc32Update } from "./crc32.ts";
import { decodeInt, decodeIntAt, encodeNullable } from "./encoding.ts";
import { engineError } from "./errors.ts";
import { encodeTypedKey, Engine, Snapshot } from "./executor.ts";
import {
  buildGistFromLeafKeys,
  GIST_SCALAR_OPCLASS,
  type GistOpclass,
  gistRangeOpclass,
  readGistLeafKeys,
  serializeGistTree,
} from "./gist.ts";
import { lz4Compress, lz4Decompress } from "./lz4.ts";
import { MemoryBlockStore } from "./memoryblockstore.ts";
import { Pager } from "./pager.ts";
import { compareBytes, decodedRows, keyViews, nodeLen, onDiskRef, residentRef } from "./pmap.ts";
import { rangeForElement } from "./range.ts";
import type { Child, LeafShape, PackedLeaf, PNode } from "./pmap.ts";
import { SharedPaging } from "./paging.ts";
import { TableStore, type Row } from "./storage.ts";
import {
  type DecimalTypmod,
  type ScalarType,
  type Type,
  arrayT,
  compositeT,
  isBool,
  isBytea,
  isText,
  isTimestamp,
  isTimestamptz,
  isInterval,
  isDate,
  isUuid,
  isJson,
  isJsonb,
  rangeT,
  scalarT,
  typeScalar,
  widthBytes,
} from "./types.ts";
import {
  type TypeRef,
  type Unfetched,
  type Value,
  boolValue,
  isSentinelTypeRef,
  byteaValue,
  emptyArray,
  compositeValue,
  decimalValue,
  emptyRangeValue,
  rangeValue,
  float32Value,
  float64Value,
  intValue,
  intervalValue,
  dateValue,
  nullValue,
  textValue,
  timestampValue,
  timestamptzValue,
  uuidValue,
  jsonValue,
  jsonbValue,
} from "./value.ts";
import type { JsonNode } from "./json.ts";
import type { ColumnStatistics, StatisticsValue } from "./statistics.ts";
import {
  STATISTICS_HISTOGRAM_BOUNDS,
  STATISTICS_MAX_VALUE_BYTES,
  STATISTICS_MCV_ENTRIES,
  STATISTICS_SAMPLE_ROWS,
} from "./estimator_constants.ts";

const FORMAT_VERSION = 29; // 29 = deterministic per-column statistics (kind 4; spec/design/statistics.md); 28 = exact table row count: each table catalog entry appends a nonnegative i64 row_count after root_data_page, with (root_data_page == 0) == (row_count == 0); on-disk format version (27 = partial-index predicates — spec/design/indexes.md §9: the per-index index_flags byte gains bit1 has_predicate, and (only when set) a u16 length + the canonical predicate text (the *Check-expression text* form) follows index_root_page; on load a partial predicate re-parses that text (XX001 on failure, like a stored CHECK) and a non-btree index with bit1 set is data_corrupted. B-tree only. A non-partial index is byte-identical to v26, so a file with no partial index moves to v27 only by its version byte + meta CRC. 26 = expression index keys — spec/design/indexes.md §1/§6: a per-index key element is a u16 column ordinal OR the 0xFFFF sentinel (never a valid ordinal, col_count ≤ 65535) + a u16 length + the expression's canonical UTF-8 text (the *Check-expression text* form, re-parsed on load — XX001 on failure, like a stored CHECK; a GIN/GiST index with a non-column key is data_corrupted). Only the index-list changes; a plain column index is byte-identical to v6, so a file with no expression index moves to v26 only by its version byte + meta CRC. 25 = on-disk free-list persistence — spec/fileformat/format.md; storage.md §6: meta offset 28 becomes free_list_head (0 = empty), and a page_type 7 free-list page persists the unconsumed free-list so open reads it directly instead of reconstructing it by walking every leaf; paired with continuous within-session reclamation. A from-scratch image (create/goldens) has an EMPTY free-list, so free_list_head = 0 and no page_type 7 page: every golden's only v25 change is its version byte + meta CRC. 24 = the B+tree reshape — spec/design/bplus-reshape.md, spec/fileformat/format.md "The per-table data B+tree": records live ONLY in leaves; an INTERIOR page (page_type 3) is a record-free routing skeleton — N+1 child pointers (u32 BE) ‖ an N-entry END-OFFSET separator directory (u32 BE) ‖ the separator key blob. A separator is a COPY of a boundary key (a leaf split copies the right half's first key up; an interior split pushes its median separator up; leaf merges remove the parent separator, interior merges pull it down — the regenerated "Fan-out" byte contract). The LEAF column regions gain a leading flags byte (reserved 0 — the dictionary door) and split by column CLASS: a FIXED-WIDTH column region is a null bitmap (ceil(N/8), MSB-first, set = NULL) + N×width dense UNTAGGED slots (a NULL slot zero-filled); a VARIABLE-WIDTH region is an N-entry end-offset value directory + the v23 tagged codec bytes with NULL a ZERO-LENGTH SPAN — the presence tag 0x01 never appears inside a v24 leaf (the single-value codec elsewhere — catalog defaults, overflow content, composite/array element bodies — is byte-unchanged). Directories throughout drop the redundant leading zero (N end offsets, not N+1 prefix sums). record_size is restated as key_len + Σ value_size (fixed → width, NULL variable → 0; the v23 phantom 2+ is dropped); RECORD_MAX keeps its value (C − max(12, 12+16K))/2, re-derived leaf-only. 23 = PAX leaf layout — a B-tree LEAF page stored its records COLUMN-MAJOR (key directory ‖ key blob ‖ column directory ‖ per column a value directory + tagged bodies, NULL = a 0x01 byte); interior pages stayed row-major and carried full records. 22 = varchar(n) length limits — spec/design/types.md §15: a text column entry appends a u32 varchar_max_len in the typmod slot (type_code 4) — 0 = unbounded, 1…10485760 = the varchar(n)/string(n) limit; a composite text field carries the same u32. The value codec is unchanged (a value is checked/truncated before encoding). A file whose every text column is unbounded still moves to v22 by its version byte + a 0 on each text column/field. 21 = EXCLUDE constraints — spec/design/gist.md §7/§8, GX3: a per-table exclusion list after the foreign-key list, each entry the constraint name + its backing GiST index name + a (column ordinal u16, operator strategy u8) element vector (&& = 0, = 1). The backing GiST index is stored like any GiST index — the index list now admits MULTI-COLUMN GiST indexes whose leaf/interior bound is the per-column component bounds concatenated (single-column GX1/GX2 bytes unchanged). A table with no exclusion still moves to v21 by its version byte + the zero count. 20 = GiST indexes — spec/design/gist.md GX1: a per-index index_kind = 2 selects the GiST access method, and the index's on-disk form is a persisted R-tree of bounding-predicate nodes — two new page types 5 (GiST leaf) / 6 (GiST interior). A leaf entry is bound_len(u16) ‖ encodeRangeBody(bound) ‖ skey_len(u16) ‖ skey; an interior entry is bound_len(u16) ‖ encodeRangeBody(union) ‖ child_page(u32). The catalog index entry is unchanged (index_root_page points at the R-tree root, 0 for empty); a file with no GiST index moves to v20 only by its version byte. 19 = storable json/jsonb columns — spec/design/json.md, slice J1/J1b: a column type can be json (type_code 18) or jsonb (type_code 19), plain scalar catalog entries with no extra descriptor (the has_jsonb_dict door §3.2 stays clear, zero bytes). A json value's body is the verbatim text, length-prefixed like text (§4); a jsonb value's body is the self-delimiting tagged-node tree (§2 — node tags + unsigned LEB128 varint counts, numbers as the decimal body), riding the large-value overflow + LZ4 path. No catalog-shape change, so a file with no json/jsonb column moves to v19 only by its version byte. 18 = reference-only collations: the catalog entry_kind 3 collation entry is metadata ONLY — a flags byte bit0 is_default, then name + unicodeVersion + cldrVersion + description (each u16-len + UTF-8) — emitted after sequences and before tables; the compiled table is NOT in the file, it is vendored into the binary and resolved by name on open, spec/design/collation.md §2/§5/§9. This supersedes v17's baked snapshot (the LZ4-compressed .coll artifact is gone). The per-column collation is unchanged (column flags byte bit6 has_collation + a trailing name). 17 = baked collations (superseded). 16 = range columns: a column type can be a range — type_code 17 + an inline element-type descriptor, one scalar code, spec/design/ranges.md §3 — and a range value is a flags byte (EMPTY/LB_INF/UB_INF/LB_INC/UB_INC) followed by the present bound bodies, §4). 15 = IDENTITY columns: the column-entry flags byte gains bit4 is_identity + bit5 identity_always; an identity column desugars like serial plus those two bits, spec/design/sequences.md §13. 14 = the serial owned-sequence link: the sequence-entry flags byte gains a has_owner bit + a trailing owner table-name/column-ordinal, spec/design/sequences.md §12. 13 = GIN inverted indexes: each catalog index entry gains a one-byte index_kind (0 = ordered B-tree, 1 = GIN) between index_flags and index_root_page, spec/design/gin.md. 12 = sequences: a kind-2 catalog entry — name + six big-endian i64 fields + a flags byte — emitted after composite-type (kind 1) entries and before table (kind 0) entries, spec/design/sequences.md §3, plus the date scalar. 11 = FOREIGN KEY constraints: a per-table catalog foreign-key list after the index list, spec/design/constraints.md §6. 10 = array (T[]) columns: type_code 15 + an element-type descriptor in the catalog, spec/design/array.md §3, and the compact array value body, §4; 9 = composite (row) types; 8 = per-column expression-default flag; 7 = per-page crc32. Each bump is atomic across Rust/Go/TS + the Ruby golden reference (every .jed golden's version byte + CRC changed together).
const PAGE_HEADER = 16; // bytes of the catalog/B-tree/overflow page header (v7: 12-byte v6 header + a 4-byte per-page crc32 at offset 12)
const RECORD_MAX_RESERVE = 12; // bytes reserved inside RECORD_MAX beyond the per-column term — independent of PAGE_HEADER (format.md "Why the record cap"). Historically the two-key interior node's 3 child pointers (4·3); since v24 the value is kept as the K=0 floor of the leaf-only re-derivation (a two-record index leaf is exactly 2·(C−12)/2 + 4·2 + 4 = C).
const PAGE_CATALOG = 1; // page_type for a catalog page
const PAGE_LEAF = 2; // page_type for a B-tree leaf node
const PAGE_INTERIOR = 3; // page_type for a B-tree interior node
const PAGE_OVERFLOW = 4; // page_type for an out-of-line value slab (large-values.md §12)
const PAGE_FREELIST = 7; // page_type for a persisted free-list page (v25 — item_count u32 free page indices, chained by next_page; spec/fileformat/format.md *Free-list page*)
export const ROOT_PAGE = 2; // catalog root of a fresh empty db (relocatable thereafter); exported for within-session compaction (persist.ts maybeCompact)
// Value-codec presence tags beyond 0x00 present-inline-plain / 0x01 NULL (large-values.md
// §12/§13; format.md "Large values"): 0x02 external-plain (u32 first_page + u32 payload_len),
// 0x03 inline-compressed (u32 raw_len + u16 comp_len + LZ4 block — lz4.md), 0x04
// external-compressed (u32 first_page + u32 stored_len + u32 raw_len; the chain carries the
// COMPRESSED block). The *_LEN constants are each form's full in-record size (tag included).
const TAG_EXTERNAL = 0x02;
const TAG_INLINE_COMP = 0x03;
const TAG_EXTERNAL_COMP = 0x04;
const EXTERNAL_PTR_LEN = 1 + 4 + 4; // tag + first_page(u32) + payload_len(u32) in a record
const INLINE_COMP_OVERHEAD = 1 + 4 + 2; // tag + raw_len(u32) + comp_len(u16)
const EXTERNAL_COMP_PTR_LEN = 1 + 4 + 4 + 4; // tag + first_page + stored_len + raw_len
// Content payloads below this many bytes are never fed to the LZ4 encoder (header overhead
// dominates; PostgreSQL pglz's default min_input_size — large-values.md §13).
const S_COMPRESS = 32;
const MIN_PAGE_SIZE = 256; // smallest valid page size; chosen floor above the structural min PAGE_HEADER+36=52 (format.md *Page model*)
const MAX_PAGE_SIZE = 65536; // largest valid page size, 64 KiB (format.md *Page model*; CLAUDE.md §13)

// A legal page size is a power of two within [MIN_PAGE_SIZE, MAX_PAGE_SIZE] (format.md *Page model* —
// the nine values {256, 512, … 65536}). Power-of-two keeps every page boundary sector-aligned (the
// SSD target, CLAUDE.md §9) and shrinks the legal set; the ps !== 0 guard also keeps the pager's
// page_size divisor non-zero.
function pageSizeValid(ps: number): boolean {
  return ps !== 0 && (ps & (ps - 1)) === 0 && ps >= MIN_PAGE_SIZE && ps <= MAX_PAGE_SIZE;
}

const UTF8 = new TextEncoder();
const UTF8_DECODE = new TextDecoder("utf-8", { fatal: true });

// typeCodeForScalar maps a scalar type to its stable on-disk code, independent of any
// in-memory ordering. See format.md.
function typeCodeForScalar(ty: ScalarType): number {
  switch (ty) {
    case "i16":
      return 1;
    case "i32":
      return 2;
    case "i64":
      return 3;
    case "text":
      return 4;
    case "boolean":
      return 5;
    case "decimal":
      return 6;
    case "bytea":
      return 7;
    case "uuid":
      return 8;
    case "timestamp":
      return 9;
    case "timestamptz":
      return 10;
    case "interval":
      return 11;
    case "f64":
      return 12;
    case "f32":
      return 13;
    case "date":
      return 16;
    // 14 (composite) / 15 (array) / 17 (range) are container element-type codes, not scalars.
    case "json":
      return 18;
    case "jsonb":
      return 19;
    // jsonpath reserves type code 20, but is literal-only this slice (no storable column), so this
    // code is never written to disk yet — a storable jsonpath column is a P1a follow-on.
    case "jsonpath":
      return 20;
  }
}

// scalarForTypeCode is the inverse; undefined for an unknown code.
function scalarForTypeCode(code: number): ScalarType | undefined {
  switch (code) {
    case 1:
      return "i16";
    case 2:
      return "i32";
    case 3:
      return "i64";
    case 4:
      return "text";
    case 5:
      return "boolean";
    case 6:
      return "decimal";
    case 7:
      return "bytea";
    case 8:
      return "uuid";
    case 9:
      return "timestamp";
    case 10:
      return "timestamptz";
    case 11:
      return "interval";
    case 12:
      return "f64";
    case 13:
      return "f32";
    case 16:
      return "date";
    case 18:
      return "json";
    case 19:
      return "jsonb";
    // jsonpath reserves code 20 (non-storable this slice, so never actually decoded off disk).
    case 20:
      return "jsonpath";
    default:
      return undefined;
  }
}

// pushArrayElementType writes an array column's element type descriptor (spec/design/array.md §3):
// the element's type code, then (for a composite element) its name. v1 element types are scalars; a
// composite element is handled for forward-compat, a nested array element is rejected.
function pushArrayElementType(w: ByteWriter, elem: Type): void {
  if (elem.kind === "array") {
    throw new Error("nested array element (array-of-array) is not a jed type — array.md §2");
  }
  if (elem.kind === "composite") {
    w.u8(14);
    const tn = UTF8.encode(elem.name);
    w.u16(tn.length);
    w.bytes(tn);
    return;
  }
  if (elem.kind === "range") {
    throw new Error("array-of-range is not storable yet (range columns land in R2)");
  }
  w.u8(typeCodeForScalar(elem.scalar));
}

// readArrayElementType decodes an array column's element type descriptor (inverse of the above).
function readArrayElementType(buf: Uint8Array, cur: Cursor): Type {
  const code = readU8(buf, cur);
  if (code === 14) {
    const n = readU16(buf, cur);
    const name = UTF8_DECODE.decode(take(buf, cur, n));
    return compositeT(name);
  }
  const s = scalarForTypeCode(code);
  if (s === undefined) throw engineError("data_corrupted", "invalid array element code");
  return scalarT(s);
}

// pushRangeElementType writes a range column's element type descriptor (spec/design/ranges.md §3):
// a single u8 scalar type code. A range element is always one of the six scalar subtypes (i32/i64/
// decimal/timestamp/timestamptz/date) — never composite, array, or nested range — and numrange's
// element is the unconstrained decimal, so no typmod is stored (the type code fully determines the
// element). The descriptor identifies which of the six ranges the column is.
function pushRangeElementType(w: ByteWriter, elem: Type): void {
  if (elem.kind !== "scalar") {
    throw new Error("a range element is always a scalar subtype (ranges.md §2)");
  }
  w.u8(typeCodeForScalar(elem.scalar));
}

// readRangeElementType decodes a range column's element type descriptor (inverse of the above): one
// scalar code, validated to be one of the six range element subtypes (else XX001).
function readRangeElementType(buf: Uint8Array, cur: Cursor): Type {
  const code = readU8(buf, cur);
  const s = scalarForTypeCode(code);
  if (s === undefined || rangeForElement(s) === undefined) {
    throw engineError("data_corrupted", "type code is not a valid range element subtype");
  }
  return scalarT(s);
}

// Keep this re-export for the collation/time-zone artifact codecs and white-box format tests. The
// backend itself lives in crc32.ts so the Node host can select zlib without entering the OPFS graph.
export { crc32Ieee } from "./crc32.ts";

// pageCrc is the per-page checksum (v7, format.md *Page header*): CRC-32/IEEE over a body page's
// bytes EXCLUDING its own 4-byte crc32 field at [12,16) — i.e. [0,12) then [16,pageSize), covering
// the header, payload, and zero-fill tail. makePage writes it; parsePage/readPage re-verify it
// (mismatch → XX001). page is one full page (pageSize bytes).
export function pageCrc(page: Uint8Array): number {
  const checksum = crc32Update(0, page.subarray(0, 12));
  return crc32Update(checksum, page.subarray(PAGE_HEADER));
}

// encodeValue is the value codec (format.md): a 1-byte presence tag (0x01 = NULL), then the type's
// present-value body. A scalar dispatches to encodeScalar; a COMPOSITE value (spec/design/composite.md
// §4) is the shared presence tag then a body of `null-bitmap ‖ each present field's value-codec body`
// (no per-field tag — the bitmap carries presence): see encodeCompositeBody. Recurses for nested
// composites.
export function encodeValue(ty: ColType, v: Value): Uint8Array {
  if (ty.kind === "scalar") return encodeScalar(ty.scalar, v);
  if (ty.kind === "array") {
    // An array column (spec/design/array.md §4): the shared presence tag then the array body.
    if (v.kind === "null") return Uint8Array.of(0x01);
    if (v.kind !== "array") throw new Error("BUG: a non-array value in an array column");
    const body = encodeArrayBody(ty.elem, v);
    const out = new Uint8Array(1 + body.length);
    out[0] = 0x00; // present
    out.set(body, 1);
    return out;
  }
  if (ty.kind === "range") {
    // A range column (spec/design/ranges.md §4): the shared presence tag then the range body.
    if (v.kind === "null") return Uint8Array.of(0x01);
    if (v.kind !== "range") throw new Error("BUG: a non-range value in a range column");
    const body = encodeRangeBody(ty.elem, v);
    const out = new Uint8Array(1 + body.length);
    out[0] = 0x00; // present
    out.set(body, 1);
    return out;
  }
  // ty is a composite column type.
  if (v.kind === "null") return Uint8Array.of(0x01);
  if (v.kind !== "composite") throw new Error("BUG: a non-composite value in a composite column");
  const body = encodeCompositeBody(ty.fields, v.fields);
  const out = new Uint8Array(1 + body.length);
  out[0] = 0x00; // present
  out.set(body, 1);
  return out;
}

// encodeArrayBody builds an array value's BODY (after the 0x00 present tag, spec/design/array.md
// §4): ndim u8, flags u8, per-dim (len u32 BE, lb i32 BE), then the optional null bitmap (present
// iff HAS_NULLS) and the present element bodies (row-major). An empty array is ndim 0; otherwise
// ndim is the dimension count and each dimension records its length and lower bound (multidim +
// custom lower bounds — spec/design/array.md §12). The bitmap (MSB-first, like composite) is present
// iff any element is NULL; a NULL element contributes zero body bytes.
function encodeArrayBody(
  elem: ColType,
  a: { dims: number[]; lbounds: number[]; elements: Value[] },
): Uint8Array {
  const elems = a.elements;
  if (elems.length === 0) return Uint8Array.of(0, 0); // ndim 0, flags 0 (empty array)
  let hasNulls = false;
  for (const e of elems) {
    if (e.kind === "null") {
      hasNulls = true;
      break;
    }
  }
  const ndim = a.dims.length;
  const header = new Uint8Array(2 + 8 * ndim);
  const dv = new DataView(header.buffer);
  header[0] = ndim;
  header[1] = hasNulls ? 0x01 : 0x00; // flags: bit 0 = HAS_NULLS
  for (let d = 0; d < ndim; d++) {
    dv.setUint32(2 + 8 * d, a.dims[d]! >>> 0, false); // dim length (u32 BE)
    dv.setInt32(2 + 8 * d + 4, a.lbounds[d]! | 0, false); // lower bound (i32 BE)
  }
  const parts: Uint8Array[] = [header];
  if (hasNulls) {
    const bitmap = new Uint8Array(Math.ceil(elems.length / 8));
    for (let i = 0; i < elems.length; i++) {
      if (elems[i]!.kind === "null") bitmap[i >> 3]! |= 0x80 >> (i % 8);
    }
    parts.push(bitmap);
  }
  for (const e of elems) {
    if (e.kind !== "null") parts.push(encodeValue(elem, e).subarray(1)); // body only (no presence tag)
  }
  return concat(parts);
}

// encodeRangeBody builds a range value's BODY (after the 0x00 present tag, spec/design/ranges.md
// §4): a single flags u8 then the present bound bodies. Flags bits: EMPTY (0), LB_INF (1), UB_INF
// (2), LB_INC (3), UB_INC (4); bits 5–7 reserved 0. An empty range is the lone flags byte 0x01 (no
// bounds follow). Otherwise a finite lower bound (!LB_INF) then a finite upper bound (!UB_INF) each
// contribute the element's value-codec body MINUS the presence tag (the same tag-byte+body split
// array/composite use). The stored value is canonical (§4) — canonicalization happens at parse/cast.
export function encodeRangeBody(elem: ColType, rv: Value & { kind: "range" }): Uint8Array {
  if (rv.empty) return Uint8Array.of(0x01); // RANGE_EMPTY
  let flags = 0;
  if (rv.lower === null) flags |= 0x02; // LB_INF
  if (rv.upper === null) flags |= 0x04; // UB_INF
  if (rv.lowerInc) flags |= 0x08; // LB_INC
  if (rv.upperInc) flags |= 0x10; // UB_INC
  const parts: Uint8Array[] = [Uint8Array.of(flags)];
  if (rv.lower !== null) parts.push(encodeValue(elem, rv.lower).subarray(1)); // body only (no presence tag)
  if (rv.upper !== null) parts.push(encodeValue(elem, rv.upper).subarray(1));
  return concat(parts);
}

// encodeCompositeBody builds a composite value's BODY (after the 0x00 present tag,
// spec/design/composite.md §4): a null bitmap of ceil(field_count/8) bytes (MSB-first — field i is
// bit 0x80 >> (i%8) of byte i/8; a set bit = NULL) followed by each PRESENT field's value-codec body
// in declaration order. A NULL field contributes zero body bytes; a present field's body is its
// encodeValue minus the leading presence tag (a nested composite recurses).
function encodeCompositeBody(fields: ColField[], vals: Value[]): Uint8Array {
  const nbytes = Math.ceil(fields.length / 8);
  const bitmap = new Uint8Array(nbytes);
  const bodies: Uint8Array[] = [];
  for (let i = 0; i < fields.length; i++) {
    const val = vals[i]!;
    if (val.kind === "null") {
      bitmap[i >> 3]! |= 0x80 >> (i % 8);
    } else {
      bodies.push(encodeValue(fields[i]!.type, val).subarray(1)); // strip the leading presence tag
    }
  }
  return concat([bitmap, ...bodies]);
}

// --- jsonb value codec (the tagged-node tree, spec/design/json.md §2) -------------------------
//
// A `jsonb` value's BODY (after the 0x00 present tag) is a self-delimiting depth-first
// serialization of the canonical node tree: every node leads with a one-byte tag (low nibble =
// kind, high nibble = flags, reserved 0). Like array/range, there is NO outer length prefix — the
// tree walks itself, so a large `jsonb` body rides the large-value overflow + LZ4 path opaquely
// (§2). The node tags are NTAG_* below; counts/string lengths are an unsigned LEB128 varint
// (§2.2). A `json` value's body is the text VERBATIM, length-prefixed exactly like `text` (§4).
//
// TS hazards (CLAUDE.md §2): string lengths in the varint are UTF-8 BYTE lengths (a JS string is
// UTF-16), so a string node measures + writes its TextEncoder bytes and reads UTF-8 back; JSON
// numbers are the exact Decimal (the decimal body), never a JS number.

const NTAG_NULL = 0x0;
const NTAG_FALSE = 0x1;
const NTAG_TRUE = 0x2;
const NTAG_NUMBER = 0x3;
const NTAG_STRING = 0x4;
const NTAG_STRING_DICT = 0x5; // reserved — the dictionary door (§3); a reader rejects it XX001
const NTAG_ARRAY = 0x6;
const NTAG_OBJECT = 0x7;

// writeUvarint appends an unsigned LEB128 varint (7 bits/byte, high bit = continuation) — the
// count/length codec for the jsonb node bodies (spec/design/json.md §2.1). `v` is a non-negative
// safe integer (a byte count within a page-bounded record); the loop shifts by /128 (not >>>, which
// is 32-bit in JS) so it stays exact for the whole safe-integer range.
function writeUvarint(out: number[], v: number): void {
  for (;;) {
    const byte = v % 0x80;
    v = Math.floor(v / 0x80);
    if (v === 0) {
      out.push(byte);
      return;
    }
    out.push(byte | 0x80);
  }
}

// readUvarint reads an unsigned LEB128 varint (inverse of writeUvarint). XX001 on a truncated or
// over-53-bit value (a jsonb count this large cannot fit a page-bounded record). Accumulates with
// `+ byte*mul` (mul = 128^shift) so it stays exact past 32 bits.
function readUvarint(buf: Uint8Array, cur: Cursor): number {
  let result = 0;
  let mul = 1;
  for (;;) {
    const byte = readU8(buf, cur);
    result += (byte & 0x7f) * mul;
    if (!Number.isSafeInteger(result)) {
      throw engineError("data_corrupted", "jsonb varint overflows a safe integer");
    }
    if ((byte & 0x80) === 0) return result;
    mul *= 0x80;
  }
}

// encodeDecimalBody appends a decimal value's BODY (no presence tag): flags(sign) ‖ u16 scale ‖ u16
// ndigits ‖ groups (base-10⁴, MS-first) — the NTAG_NUMBER payload and the inverse of
// decodeDecimalBody. Byte-identical to encodeScalar's decimal arm minus the leading present tag.
function encodeDecimalBody(d: Decimal, out: number[]): void {
  const [neg, scale, groups] = d.toCodec();
  out.push(neg ? 1 : 0);
  out.push((scale >>> 8) & 0xff, scale & 0xff);
  out.push((groups.length >>> 8) & 0xff, groups.length & 0xff);
  for (const g of groups) out.push((g >>> 8) & 0xff, g & 0xff);
}

// encodeJsonbBody serializes a jsonb node tree into `out` (the body bytes — spec/design/json.md
// §2.1). Object members are already in canonical key order (the canonicalizer's invariant); each
// member's key is itself a string node (NTAG_STRING), so the dictionary door covers keys and values
// uniformly. String lengths are UTF-8 byte lengths (the TS hazard).
function encodeJsonbBody(node: JsonNode, out: number[]): void {
  switch (node.kind) {
    case "null":
      out.push(NTAG_NULL);
      return;
    case "bool":
      out.push(node.value ? NTAG_TRUE : NTAG_FALSE);
      return;
    case "number":
      out.push(NTAG_NUMBER);
      encodeDecimalBody(node.dec, out);
      return;
    case "string": {
      out.push(NTAG_STRING);
      const bytes = UTF8.encode(node.value);
      writeUvarint(out, bytes.length);
      for (const b of bytes) out.push(b);
      return;
    }
    case "array":
      out.push(NTAG_ARRAY);
      writeUvarint(out, node.elements.length);
      for (const e of node.elements) encodeJsonbBody(e, out);
      return;
    case "object":
      out.push(NTAG_OBJECT);
      writeUvarint(out, node.members.length);
      for (const m of node.members) {
        out.push(NTAG_STRING);
        const kbytes = UTF8.encode(m.key);
        writeUvarint(out, kbytes.length);
        for (const b of kbytes) out.push(b);
        encodeJsonbBody(m.value, out);
      }
      return;
  }
}

// jsonbBodyBytes returns the encoded body of a jsonb node as a Uint8Array (encodeJsonbBody over a
// fresh accumulator).
function jsonbBodyBytes(node: JsonNode): Uint8Array {
  const out: number[] = [];
  encodeJsonbBody(node, out);
  return Uint8Array.from(out);
}

// decodeJsonbBody deserializes a jsonb node from `buf` at `cur` (inverse of encodeJsonbBody). A
// nonzero flag nibble, the reserved NTAG_STRING_DICT (no dictionary slice yet), or an unknown kind
// is XX001 data_corrupted (spec/design/json.md §3.1/§6.3).
function decodeJsonbBody(buf: Uint8Array, cur: Cursor, mode: DecodeMode): JsonNode {
  const tag = readU8(buf, cur);
  if ((tag & 0xf0) !== 0) {
    throw engineError("data_corrupted", "jsonb node tag has a reserved flag bit set");
  }
  switch (tag & 0x0f) {
    case NTAG_NULL:
      return { kind: "null" };
    case NTAG_FALSE:
      return { kind: "bool", value: false };
    case NTAG_TRUE:
      return { kind: "bool", value: true };
    case NTAG_NUMBER: {
      const v = decodeDecimalBody(buf, cur, mode);
      if (mode === "skip") return { kind: "null" }; // placeholder (decimal body advanced, not built)
      if (v.kind !== "decimal")
        throw engineError("data_corrupted", "jsonb number is not a decimal");
      return { kind: "number", dec: v.dec };
    }
    case NTAG_STRING: {
      const s = decodeJsonbString(buf, cur, mode);
      if (mode === "skip") return { kind: "null" }; // placeholder
      return { kind: "string", value: s };
    }
    case NTAG_STRING_DICT:
      throw engineError(
        "data_corrupted",
        "jsonb string-dictionary reference before the dictionary slice",
      );
    case NTAG_ARRAY: {
      const count = readUvarint(buf, cur);
      const elements: JsonNode[] = [];
      for (let i = 0; i < count; i++) {
        const e = decodeJsonbBody(buf, cur, mode);
        if (mode === "construct") elements.push(e);
      }
      return { kind: "array", elements };
    }
    case NTAG_OBJECT: {
      const count = readUvarint(buf, cur);
      const members: { key: string; value: JsonNode }[] = [];
      for (let i = 0; i < count; i++) {
        // Each member's key is a string node (NTAG_STRING / reserved NTAG_STRING_DICT).
        const ktag = readU8(buf, cur);
        if ((ktag & 0xf0) !== 0) {
          throw engineError("data_corrupted", "jsonb object key tag has a reserved flag bit set");
        }
        const k = ktag & 0x0f;
        if (k === NTAG_STRING_DICT) {
          throw engineError(
            "data_corrupted",
            "jsonb string-dictionary reference before the dictionary slice",
          );
        }
        if (k !== NTAG_STRING) {
          throw engineError("data_corrupted", "jsonb object key is not a string node");
        }
        const key = decodeJsonbString(buf, cur, mode);
        const value = decodeJsonbBody(buf, cur, mode);
        if (mode === "construct") members.push({ key, value });
      }
      return { kind: "object", members };
    }
    default:
      throw engineError("data_corrupted", "unknown jsonb node tag");
  }
}

// decodeJsonbString reads a NTAG_STRING payload (varint len ‖ UTF-8 bytes) after its tag has been
// consumed. Decodes UTF-8 BYTES (not UTF-16 units) back to a JS string; non-UTF-8 is XX001. In "skip"
// mode it advances past the bytes and returns "" (no decode / no validation — lazy-record.md §6).
function decodeJsonbString(buf: Uint8Array, cur: Cursor, mode: DecodeMode): string {
  const len = readUvarint(buf, cur);
  const bytes = take(buf, cur, len);
  if (mode === "skip") return "";
  try {
    return UTF8_DECODE.decode(bytes);
  } catch {
    throw engineError("data_corrupted", "non-UTF-8 jsonb string");
  }
}

// encodeScalar is the scalar value codec (the body of encodeValue for a scalar ColType). A 1-byte
// presence tag (0x01 = NULL), then the type's present-value body. Integers reuse the order-preserving
// key encoding; text is where the seam diverges — a stored text value needs no ordering, so it is a
// compact u16 byte-length + UTF-8 bytes (collation C, verbatim). A text value whose UTF-8 length
// exceeds 0xFFFF is unsupported; in practice it also exceeds a page and is caught by the
// oversized-item rule in packing (0A000), so the u16 write is sound for every supported page size
// (spec/fileformat/format.md). boolean is a single bool-byte body — 0x00 false, 0x01 true (types.md §9).
function encodeScalar(ty: ScalarType, v: Value): Uint8Array {
  if (v.kind === "null") return encodeNullable(ty, null);
  if (v.kind === "unfetched") {
    // An unfetched reference is resolved before any encode/plan (the scan layer for reads,
    // the mutation path for stores, resolveForEncode at commit — large-values.md §14).
    throw new Error("BUG: encoding an unfetched large value");
  }
  if (v.kind === "text") {
    const bytes = UTF8.encode(v.text);
    const out = new Uint8Array(3 + bytes.length);
    out[0] = 0x00; // present
    out[1] = (bytes.length >>> 8) & 0xff;
    out[2] = bytes.length & 0xff;
    out.set(bytes, 3);
    return out;
  }
  if (v.kind === "bytea") {
    // Same compact length-prefixed body as text, but the raw bytes verbatim (no UTF-8).
    const out = new Uint8Array(3 + v.bytes.length);
    out[0] = 0x00; // present
    out[1] = (v.bytes.length >>> 8) & 0xff;
    out[2] = v.bytes.length & 0xff;
    out.set(v.bytes, 3);
    return out;
  }
  if (v.kind === "uuid") {
    // Fixed 16-byte body, NO length prefix (the first fixed-width non-integer value) —
    // spec/fileformat/format.md.
    const out = new Uint8Array(1 + 16);
    out[0] = 0x00; // present
    out.set(v.bytes, 1);
    return out;
  }
  if (v.kind === "bool") {
    return new Uint8Array([0x00, v.value ? 0x01 : 0x00]); // present tag + bool-byte
  }
  if (v.kind === "decimal") {
    // Decimal value codec (spec/fileformat/format.md): tag, flags (sign), u16 scale, u16
    // ndigits, then that many big-endian base-10^4 coefficient groups (MS-first).
    const [neg, scale, groups] = v.dec.toCodec();
    const out = new Uint8Array(6 + groups.length * 2);
    out[0] = 0x00; // present
    out[1] = neg ? 1 : 0; // flags: bit0 = sign
    out[2] = (scale >>> 8) & 0xff;
    out[3] = scale & 0xff;
    out[4] = (groups.length >>> 8) & 0xff;
    out[5] = groups.length & 0xff;
    for (let i = 0; i < groups.length; i++) {
      out[6 + i * 2] = (groups[i]! >>> 8) & 0xff;
      out[7 + i * 2] = groups[i]! & 0xff;
    }
    return out;
  }
  if (v.kind === "timestamp" || v.kind === "timestamptz") {
    // Timestamps store their i64 microsecond instant via the same fixed-width codec as
    // i64 (spec/design/timestamp.md §6).
    return encodeNullable(ty, v.micros);
  }
  if (v.kind === "date") {
    // A date stores its i32 day count via the same fixed-width (4-byte) order-preserving codec
    // as i32 (spec/design/date.md).
    return encodeNullable(ty, v.days);
  }
  if (v.kind === "interval") {
    // Fixed 16-byte body: i32 months, i32 days, i64 micros — big-endian two's-complement, no
    // sign-flip (a value codec, not an order-preserving key) — spec/fileformat/format.md.
    const out = new Uint8Array(1 + 16);
    out[0] = 0x00; // present
    const dv = new DataView(out.buffer);
    dv.setInt32(1, v.iv.months, false);
    dv.setInt32(5, v.iv.days, false);
    dv.setBigInt64(9, v.iv.micros, false);
    return out;
  }
  if (v.kind === "f64") {
    // 8 IEEE bytes, big-endian, no length prefix (fixed-width like uuid/timestamp). Stored VERBATIM
    // for every value EXCEPT NaN: a -0.0 keeps its sign bit and ±Inf/finite keep theirs, but a NaN
    // is canonicalized to the single quiet pattern 0x7FF8000000000000. A NaN's payload is
    // core-specific (Go's math.NaN() is …001, hardware Inf-Inf is the negative 0xFFF8…), so the
    // codec pins it cross-core (spec/design/float.md §10, determinism.md §4); the -0→+0 collapse is
    // a compare/key concern only, NOT applied here. (V8 already materializes a canonical NaN, so the
    // branch is belt-and-suspenders parity with the Rust/Go codecs.)
    const out = new Uint8Array(1 + 8);
    out[0] = 0x00; // present
    const dv = new DataView(out.buffer);
    if (Number.isNaN(v.value)) dv.setBigUint64(1, 0x7ff8000000000000n, false);
    else dv.setFloat64(1, v.value, false); // big-endian
    return out;
  }
  if (v.kind === "f32") {
    // 4 IEEE bytes, big-endian. v.value is already Math.fround'd (binary32), so setFloat32 stores it
    // without further rounding loss. NaN is canonicalized to 0x7FC00000 (see f64 above).
    const out = new Uint8Array(1 + 4);
    out[0] = 0x00; // present
    const dv = new DataView(out.buffer);
    if (Number.isNaN(v.value)) dv.setUint32(1, 0x7fc00000, false);
    else dv.setFloat32(1, v.value, false); // big-endian
    return out;
  }
  if (v.kind === "json") {
    // json: the verbatim text body, length-prefixed exactly like text (spec/design/json.md §4).
    const bytes = UTF8.encode(v.text);
    const out = new Uint8Array(3 + bytes.length);
    out[0] = 0x00; // present
    out[1] = (bytes.length >>> 8) & 0xff;
    out[2] = bytes.length & 0xff;
    out.set(bytes, 3);
    return out;
  }
  if (v.kind === "jsonb") {
    // jsonb: present tag, then the self-delimiting tagged-node tree (spec/design/json.md §2).
    const body = jsonbBodyBytes(v.node);
    const out = new Uint8Array(1 + body.length);
    out[0] = 0x00; // present
    out.set(body, 1);
    return out;
  }
  // jsonpath is literal-only (non-storable) — it never reaches the scalar codec (a jsonpath column
  // is 0A000 at CREATE TABLE, so no value is ever stored).
  if (v.kind === "jsonpath") {
    throw new Error("BUG: a jsonpath value reached the scalar codec");
  }
  if (v.kind !== "int") throw engineError("data_corrupted", "cannot store a non-integer value");
  return encodeNullable(ty, v.int);
}

// ByteWriter accumulates big-endian bytes for a variable-length item.
class ByteWriter {
  private buf: number[] = [];
  u8(v: number): void {
    this.buf.push(v & 0xff);
  }
  u16(v: number): void {
    this.buf.push((v >>> 8) & 0xff, v & 0xff);
  }
  u32(v: number): void {
    this.buf.push((v >>> 24) & 0xff, (v >>> 16) & 0xff, (v >>> 8) & 0xff, v & 0xff);
  }
  // i64 writes an 8-byte big-endian two's-complement i64 (a value-codec context, not a key —
  // the interval-micros / sequence-field encoding). Big-endian via DataView.
  i64(v: bigint): void {
    const dv = new DataView(new ArrayBuffer(8));
    dv.setBigInt64(0, v, false);
    for (let i = 0; i < 8; i++) this.buf.push(dv.getUint8(i));
  }
  bytes(b: Uint8Array): void {
    for (const x of b) this.buf.push(x);
  }
  toBytes(): Uint8Array {
    return Uint8Array.from(this.buf);
  }
}

function concat(parts: Uint8Array[]): Uint8Array {
  let total = 0;
  for (const p of parts) total += p.length;
  const out = new Uint8Array(total);
  let off = 0;
  for (const p of parts) {
    out.set(p, off);
    off += p.length;
  }
  return out;
}

// isSpillable reports whether a value of this type can be stored out-of-line (a variable-length
// type). Fixed-width scalars are tiny and always stay inline (spec/design/large-values.md §12). A
// COMPOSITE is treated as spillable — its opaque inline body spills via the same overflow + LZ4 path
// when a record exceeds RECORD_MAX (spec/design/composite.md §4); a small composite is never actually
// chosen by the plan.
function isSpillable(ty: ColType): boolean {
  if (ty.kind === "composite") return true;
  // An array's opaque inline body spills via the same overflow + LZ4 path (array.md §4).
  if (ty.kind === "array") return true;
  // A range's body is its flags byte + bound bodies; a numrange over huge decimals could exceed
  // RECORD_MAX, so it rides the same overflow + LZ4 path. A discrete range (tiny, fixed-width
  // bounds) is never actually chosen by the plan (spec/design/ranges.md §4).
  if (ty.kind === "range") return true;
  const s = ty.scalar;
  // json/jsonb are variable-length document bodies that ride the same overflow + LZ4 path as
  // text/bytea when a record exceeds RECORD_MAX (spec/design/json.md §2/§4).
  return isText(s) || isBytea(s) || s === "decimal" || isJson(s) || isJsonb(s);
}

// pagePayload is the page payload capacity C = pageSize − PAGE_HEADER — the bytes a single page has
// for body content (the B-tree split threshold and the overflow-chain slab size). The in-memory
// store, the whole-image serializer, and the cost meter must all use this one value, or the split
// decision diverges from the serialized layout (the `−12` drift that PAGE_HEADER's v7 growth to 16
// silently introduced — format.md §7).
export function pagePayload(pageSize: number): number {
  return pageSize - PAGE_HEADER;
}

// recordMaxFor is the largest a single LEAF record may serialize to and still satisfy the B+tree
// split contract — RECORD_MAX(K) = (C − max(12, 12+16·K))/2 where C = capacity is the page payload
// and K the value-column count (format.md "Why the record cap"). The value is deliberately KEPT
// from v23 (bplus-reshape.md §4.2), re-derived leaf-only: the worst-case (all-variable) two-record
// leaf overhead is 12 + 13·K ≤ 12 + 16·K, so a two-record leaf never overflows. The spill planner
// reduces a record to ≤ this by externalizing values.
function recordMaxFor(capacity: number, k: number): number {
  return Math.max(0, Math.floor((capacity - (RECORD_MAX_RESERVE + 16 * k)) / 2));
}

// fixedValueWidth is the storage width of a FIXED-WIDTH column's value body (the dense leaf slot
// stride — format.md v24 "Leaf node"), or null for a VARIABLE-WIDTH column (text / bytea / decimal /
// json / jsonb / composite / array / range — exactly the spillable set). The class decides the
// column's leaf region shape: fixed-width regions are bitmap + dense untagged slots; variable
// regions are a value directory + tagged codec bytes (NULL = a zero-length span).
export function fixedValueWidth(ty: ColType): number | null {
  if (ty.kind !== "scalar") return null; // composite / array / range are variable-width
  switch (ty.scalar) {
    case "i16":
      return 2;
    case "i32":
      return 4;
    case "i64":
      return 8;
    case "boolean":
      return 1;
    case "uuid":
      return 16;
    case "timestamp":
    case "timestamptz":
      return 8;
    case "date":
      return 4;
    case "interval":
      return 16;
    case "f64":
      return 8;
    case "f32":
      return 4;
    // jsonpath is not storable as a column (type code 20 is reserved — a value reaching the codec
    // throws there); classed variable defensively.
    default:
      return null;
  }
}

// leafShape is the leaf column-class shape for a table with these value-column types — the
// { fixed, var } counts the B+tree's leafOverhead arithmetic needs (pmap.ts; an index tree — empty
// colTypes — is { 0, 0 }). Computed once per TableStore.
export function leafShape(colTypes: ColType[]): LeafShape {
  let fixed = 0;
  for (const ty of colTypes) if (fixedValueWidth(ty) !== null) fixed++;
  return { fixed, var: colTypes.length - fixed };
}

// A value's planned on-disk disposition (large-values.md §2/§12/§13).
type ValueDisp = "inline" | "inlineComp" | "external" | "externalComp";

// A record's resolved disposition plan: per-column form, the LZ4 block a compressed form
// carries (so the serializer never re-compresses), the on-disk record size (the B-tree split
// weight), and the value_compress slabs the plan's pass-1 attempts cost (cost.md §3).
type RecordPlan = {
  disp: ValueDisp[];
  comp: (Uint8Array | null)[];
  size: number;
  compressUnits: number;
};

// planDispositions decides each column's on-disk disposition (large-values.md §3/§12/§13;
// format.md "Large values"). Spill only when forced: if the all-inline-plain record already fits
// RECORD_MAX, nothing is compressed or spilled. Otherwise two passes, each visiting largest
// encoded size first, ties by ascending column index — deterministic, a §8 contract:
// (1) compress eligible values (payload ≥ S_COMPRESS), adopting iff the encoded compressed form
// is strictly smaller (store-smaller); (2) externalize values whose current encoded size still
// beats their pointer, moving the bytes pass 1 chose (compressed → a 0x04 chain of the
// compressed block) until the record fits. Shared by the serializer and recordSize (the B-tree
// split weight): in-memory node boundaries must match the serialized pages.
function planDispositions(
  colTypes: ColType[],
  key: Uint8Array,
  row: Row,
  capacity: number,
): RecordPlan {
  // Each column's inline-plain contribution to recordSize (the v24 basis — format.md "Record"): a
  // fixed-width column always its width (a NULL occupies a zero-filled slot); a variable-width
  // column 0 when NULL (a zero-length span) else its tagged inline encoding.
  const inline = colTypes.map((ty, i) => {
    const w = fixedValueWidth(ty);
    if (w !== null) return w;
    return row[i]!.kind === "null" ? 0 : encodeValue(ty, row[i]!).length;
  });
  const plan: RecordPlan = {
    disp: new Array<ValueDisp>(colTypes.length).fill("inline"),
    comp: new Array<Uint8Array | null>(colTypes.length).fill(null),
    size: 0,
    compressUnits: 0,
  };
  const cur = inline.slice();
  let size = key.length + inline.reduce((a, b) => a + b, 0);
  const max = recordMaxFor(capacity, colTypes.length);
  if (size <= max) {
    plan.size = size;
    return plan;
  }
  // Pass 1 — compress (lz4.md): spillable, non-NULL, payload ≥ S_COMPRESS; largest inline-plain
  // encoded size first, ties by ascending index (Array.prototype.sort is stable, ES2019+).
  // Every attempt is metered (ceil(raw/capacity) value_compress slabs) whether or not
  // store-smaller adopts it.
  let cand: number[] = [];
  for (let i = 0; i < colTypes.length; i++) {
    if (
      isSpillable(colTypes[i]!) &&
      row[i]!.kind !== "null" &&
      valuePayload(colTypes[i]!, row[i]!).length >= S_COMPRESS
    ) {
      cand.push(i);
    }
  }
  cand.sort((a, b) => inline[b]! - inline[a]!);
  for (const i of cand) {
    if (size <= max) break;
    const payload = valuePayload(colTypes[i]!, row[i]!);
    plan.compressUnits += Math.ceil(payload.length / capacity);
    const comp = lz4Compress(payload);
    if (INLINE_COMP_OVERHEAD + comp.length < inline[i]!) {
      size = size - cur[i]! + INLINE_COMP_OVERHEAD + comp.length;
      cur[i] = INLINE_COMP_OVERHEAD + comp.length;
      plan.disp[i] = "inlineComp";
      plan.comp[i] = comp;
    }
  }
  if (size <= max) {
    plan.size = size;
    return plan;
  }
  // Pass 2 — externalize: anything whose current encoded size beats its pointer, largest
  // current size first, ties by ascending index. (A NULL is 1 byte and never qualifies.)
  cand = [];
  for (let i = 0; i < colTypes.length; i++) {
    const ptr = plan.disp[i] === "inlineComp" ? EXTERNAL_COMP_PTR_LEN : EXTERNAL_PTR_LEN;
    if (isSpillable(colTypes[i]!) && cur[i]! > ptr) cand.push(i);
  }
  cand.sort((a, b) => cur[b]! - cur[a]!);
  for (const i of cand) {
    if (size <= max) break;
    const compressed = plan.disp[i] === "inlineComp";
    const ptr = compressed ? EXTERNAL_COMP_PTR_LEN : EXTERNAL_PTR_LEN;
    plan.disp[i] = compressed ? "externalComp" : "external";
    size = size - cur[i]! + ptr;
    cur[i] = ptr;
  }
  plan.size = size;
  return plan;
}

// recordSize is the on-disk size of a record — the weight the page-backed B-tree splits on
// (format.md). Accounts for compression and out-of-line spill: a compressed value contributes
// its compressed inline form, an externalized one its fixed pointer size (large-values.md
// §12/§13). Must equal what the serializer produces, so in-memory node boundaries match
// serialized page boundaries.
export function recordSize(
  colTypes: ColType[],
  key: Uint8Array,
  row: Row,
  capacity: number,
): number {
  return planDispositions(colTypes, key, row, capacity).size;
}

// recordScanUnits returns the per-record units a scan's up-front cost block charges beyond the
// B-tree nodes (cost.md §3; large-values.md §8/§12/§14): for every column in the query's TOUCHED
// SET (mask), pages = one page_read per overflow chain page (the chain carries the payload for
// external-plain, the COMPRESSED block for external-compressed) and decompress =
// ceil(raw/capacity) value_decompress slabs per compressed stored value (inline- or external-).
// Zero/zero for a fully-inline-plain record or an untouched column.
export function recordScanUnits(
  colTypes: ColType[],
  key: Uint8Array,
  row: Row,
  capacity: number,
  mask: boolean[],
): { pages: number; decompress: number } {
  let pages = 0;
  let decompress = 0;
  // A lazily-loaded row carries its on-disk forms as unfetched references (large-values.md
  // §14): read the units straight off them — no disposition re-plan, which would need the
  // unfetched bytes. The numbers equal the resident plan below by construction (the
  // references ARE that plan's stored output), so a paged and an in-memory database charge
  // identically (cost.md §3, logical cost).
  if (row.some((v) => v.kind === "unfetched")) {
    for (let i = 0; i < row.length; i++) {
      const v = row[i]!;
      if (!mask[i] || v.kind !== "unfetched") continue;
      if (v.ref.form === 0x00) {
        // Inline-deferred values live in the record — no chain page, no decompress slab
        // (lazy-record.md §8: cost is invariant; matches the resident plan's "inline").
      } else if (v.ref.form === TAG_EXTERNAL) {
        pages += Math.ceil(v.ref.storedLen / capacity);
      } else if (v.ref.form === TAG_INLINE_COMP) {
        decompress += Math.ceil(v.ref.rawLen / capacity);
      } else {
        pages += Math.ceil(v.ref.storedLen / capacity);
        decompress += Math.ceil(v.ref.rawLen / capacity);
      }
    }
    return { pages, decompress };
  }
  const plan = planDispositions(colTypes, key, row, capacity);
  for (let i = 0; i < plan.disp.length; i++) {
    if (!mask[i]) continue; // an untouched column's chain/slabs are never read (cost.md §3)
    switch (plan.disp[i]) {
      case "external":
        pages += Math.ceil(valuePayload(colTypes[i]!, row[i]!).length / capacity);
        break;
      case "inlineComp":
        decompress += Math.ceil(valuePayload(colTypes[i]!, row[i]!).length / capacity);
        break;
      case "externalComp":
        pages += Math.ceil(plan.comp[i]!.length / capacity);
        decompress += Math.ceil(valuePayload(colTypes[i]!, row[i]!).length / capacity);
        break;
    }
  }
  return { pages, decompress };
}

// recordCompressUnits returns the value_compress slabs storing this record costs — one
// ceil(raw/capacity) block per pass-1 compression attempt, adopted or not (cost.md §3;
// large-values.md §13). Charged once per stored row version at the statement's write site,
// never for B-tree re-encodes.
export function recordCompressUnits(
  colTypes: ColType[],
  key: Uint8Array,
  row: Row,
  capacity: number,
): number {
  return planDispositions(colTypes, key, row, capacity).compressUnits;
}

// valuePayload is a value's content payload P(v) — the bytes stored in the overflow chain when it is
// externalized (large-values.md §12): raw UTF-8 for text, raw bytes for bytea, the decimal body
// (encoding minus its presence tag) for decimal, and for a COMPOSITE its body — the encoding minus the
// leading presence tag, i.e. the null bitmap + present-field bodies (spec/design/composite.md §4).
// Only spillable types reach here.
function valuePayload(ty: ColType, v: Value): Uint8Array {
  if (ty.kind === "composite" && v.kind === "composite")
    return encodeCompositeBody(ty.fields, v.fields);
  // An array's payload is its body (the ndim/flags/dims header + bitmap + element bodies); a large
  // array spills through the same overflow + LZ4 path (spec/design/array.md §4).
  if (ty.kind === "array" && v.kind === "array") return encodeArrayBody(ty.elem, v);
  // A range's payload is its body (the flags byte + present bound bodies, spec/design/ranges.md §4).
  if (ty.kind === "range" && v.kind === "range") return encodeRangeBody(ty.elem, v);
  if (v.kind === "text") return UTF8.encode(v.text);
  if (v.kind === "bytea") return v.bytes;
  // json's payload is the verbatim UTF-8 (no length prefix — the chain tracks its own length,
  // exactly like text); jsonb's payload is the tagged-node tree body (spec/design/json.md §4/§2).
  if (v.kind === "json") return UTF8.encode(v.text);
  if (v.kind === "jsonb") return jsonbBodyBytes(v.node);
  if (v.kind === "decimal" && ty.kind === "scalar") return encodeScalar(ty.scalar, v).subarray(1); // strip the presence tag
  throw engineError("data_corrupted", "only spillable values are externalized");
}

// valueFromPayload reconstructs a value from the P(v) content gathered from its overflow chain
// (inverse of valuePayload) — large-values.md §12.
function valueFromPayload(ty: ColType, payload: Uint8Array): Value {
  if (ty.kind === "composite") {
    // A composite's payload is its body (bitmap + present-field bodies); decode it with a fresh
    // cursor (spec/design/composite.md §4).
    return readCompositeBody(ty, payload, { pos: 0 }, "construct");
  }
  if (ty.kind === "array") {
    // An array's payload is its body; decode it with a fresh cursor (spec/design/array.md §4).
    return readArrayBody(ty, payload, { pos: 0 }, "construct");
  }
  if (ty.kind === "range") {
    // A range's payload is its body; decode it with a fresh cursor (spec/design/ranges.md §4).
    return readRangeBody(ty.elem, payload, { pos: 0 }, "construct");
  }
  const s = ty.scalar;
  if (isText(s)) {
    try {
      return textValue(UTF8_DECODE.decode(payload));
    } catch {
      throw engineError("data_corrupted", "non-UTF-8 text value");
    }
  }
  if (isBytea(s)) return byteaValue(payload.slice());
  if (isJson(s)) {
    try {
      return jsonValue(UTF8_DECODE.decode(payload));
    } catch {
      throw engineError("data_corrupted", "non-UTF-8 json value");
    }
  }
  if (isJsonb(s)) return jsonbValue(decodeJsonbBody(payload, { pos: 0 }, "construct"));
  if (s === "decimal") return decodeDecimalBody(payload, { pos: 0 }, "construct");
  throw engineError("data_corrupted", "a non-spillable type was stored external");
}

// OverflowPageOut is one overflow page produced while serializing a record's external value.
// Exported for the lazy-record white-box test (lazy_inline_values.test.ts); the barrel is unaffected.
export type OverflowPageOut = {
  index: number;
  itemCount: number;
  nextPage: number;
  payload: Uint8Array;
};

// encodeInterior builds a v24 INTERIOR node payload (format.md "Interior node"): N+1 child
// pointers ‖ an N-entry end-offset separator directory ‖ the separator key blob. Record-free —
// no value codec, no overflow chains; a separator is raw order-preserving key bytes.
function encodeInterior(seps: Uint8Array[], childPages: number[]): Uint8Array {
  const w = new ByteWriter();
  for (const cp of childPages) w.u32(cp);
  let off = 0;
  for (const s of seps) {
    off += s.length;
    w.u32(off);
  }
  for (const s of seps) w.bytes(s);
  return w.toBytes();
}

// encodeDisposedValue encodes one value's on-disk body given its resolved disposition — the value
// codec, unchanged across the row-major and PAX leaf layouts (large-values.md §12/§13). An
// inline-plain value is encodeValue; the large-value forms carry a pointer / inline block and
// allocate overflow chains via `take`. `comp` is the LZ4 block a compressed form carries.
function encodeDisposedValue(
  ty: ColType,
  v: Value,
  disp: ValueDisp,
  comp: Uint8Array | null,
  capacity: number,
  take: () => number,
  ovf: OverflowPageOut[],
): Uint8Array {
  const w = new ByteWriter();
  switch (disp) {
    case "external": {
      const payload = valuePayload(ty, v);
      const first = writeOverflowChain(payload, capacity, take, ovf);
      w.u8(TAG_EXTERNAL);
      w.u32(first);
      w.u32(payload.length);
      break;
    }
    case "inlineComp": {
      const rawLen = valuePayload(ty, v).length;
      w.u8(TAG_INLINE_COMP);
      w.u32(rawLen);
      w.u16(comp!.length);
      w.bytes(comp!);
      break;
    }
    case "externalComp": {
      // The chain carries the COMPRESSED block (its page count follows comp size).
      const rawLen = valuePayload(ty, v).length;
      const first = writeOverflowChain(comp!, capacity, take, ovf);
      w.u8(TAG_EXTERNAL_COMP);
      w.u32(first);
      w.u32(comp!.length);
      w.u32(rawLen);
      break;
    }
    default:
      w.bytes(encodeValue(ty, v));
  }
  return w.toBytes();
}

// encodeLeafPax builds a v24 PAX (column-major) leaf payload from records in ascending key order
// (format.md "Leaf node"). Values encode in (record, column) order — so each external value's
// overflow chain allocates via `take` in exactly that order, keeping overflow page indices
// golden-pinned — then assembled: key directory (N u32 end offsets) ‖ key blob ‖ column directory
// (K+1 u32 absolute offsets, colStart[K] = payload end) ‖ per column a region: a flags byte (0),
// then — fixed-width — the null bitmap + N×width dense untagged slots (a NULL slot zero-filled),
// or — variable-width — an N-entry end-offset value directory + the tagged value bodies (NULL = a
// zero-length span). Exported for the lazy-record white-box test (lazy_inline_values.test.ts); the
// barrel (tooling.ts) is unaffected.
export function encodeLeafPax(
  colTypes: ColType[],
  keys: Uint8Array[],
  rows: Row[],
  capacity: number,
  take: () => number,
  ovf: OverflowPageOut[],
): Uint8Array {
  const n = keys.length;
  const k = colTypes.length;
  // Encode each value in (record, column) order; overflow chains allocate here. A fixed-width
  // column's slot is the untagged inline body (encodeValue minus its 0x00 tag; zeros for NULL); a
  // variable column's bytes are the tagged disposed form (empty for NULL).
  const valBytes: Uint8Array[][] = Array.from({ length: k }, () => new Array<Uint8Array>(n));
  const nulls: boolean[][] = Array.from({ length: k }, () => new Array<boolean>(n).fill(false));
  for (let i = 0; i < n; i++) {
    const plan = planDispositions(colTypes, keys[i]!, rows[i]!, capacity);
    for (let c = 0; c < k; c++) {
      const v = rows[i]![c]!;
      const isNull = v.kind === "null";
      nulls[c]![i] = isNull;
      const width = fixedValueWidth(colTypes[c]!);
      if (width !== null) {
        valBytes[c]![i] = isNull ? new Uint8Array(width) : encodeValue(colTypes[c]!, v).subarray(1);
      } else if (isNull) {
        valBytes[c]![i] = new Uint8Array(0);
      } else {
        valBytes[c]![i] = encodeDisposedValue(
          colTypes[c]!,
          v,
          plan.disp[c]!,
          plan.comp[c] ?? null,
          capacity,
          take,
          ovf,
        );
      }
    }
  }
  const w = new ByteWriter();
  // key directory (N end offsets) + key blob.
  let off = 0;
  for (let i = 0; i < n; i++) {
    off += keys[i]!.length;
    w.u32(off);
  }
  for (let i = 0; i < n; i++) w.bytes(keys[i]!);
  const totalKeyBytes = off;
  // column directory: absolute payload offset of each region (computed analytically).
  const baseAfterColDir = 4 * n + totalKeyBytes + 4 * (k + 1);
  const colStart = new Array<number>(k + 1);
  let cur = baseAfterColDir;
  for (let c = 0; c < k; c++) {
    colStart[c] = cur;
    let bodies = 0;
    for (let i = 0; i < n; i++) bodies += valBytes[c]![i]!.length;
    cur +=
      1 + (fixedValueWidth(colTypes[c]!) !== null ? Math.ceil(n / 8) + bodies : 4 * n + bodies);
  }
  colStart[k] = cur;
  for (let c = 0; c <= k; c++) w.u32(colStart[c]!);
  // each column region: flags byte, then bitmap + dense slots (fixed) or value directory + tagged
  // bodies (variable).
  for (let c = 0; c < k; c++) {
    w.u8(0); // region flags — reserved (the dictionary door)
    if (fixedValueWidth(colTypes[c]!) !== null) {
      const bitmap = new Uint8Array(Math.ceil(n / 8));
      for (let i = 0; i < n; i++) {
        if (nulls[c]![i]) bitmap[i >> 3] |= 0x80 >> (i % 8);
      }
      w.bytes(bitmap);
    } else {
      let voff = 0;
      for (let i = 0; i < n; i++) {
        voff += valBytes[c]![i]!.length;
        w.u32(voff);
      }
    }
    for (let i = 0; i < n; i++) w.bytes(valBytes[c]![i]!);
  }
  return w.toBytes();
}

// writeOverflowChain writes payload across a chain of overflow pages (capacity-byte slabs, in order),
// allocating each page via `take` and linking it with nextPage (0 terminates). Returns the first page
// index for the record's pointer. payload is always non-empty (only values larger than the pointer
// spill — planDispositions).
function writeOverflowChain(
  payload: Uint8Array,
  capacity: number,
  take: () => number,
  ovf: OverflowPageOut[],
): number {
  const n = Math.ceil(payload.length / capacity);
  const indices: number[] = [];
  for (let i = 0; i < n; i++) indices.push(take());
  for (let j = 0; j < n; j++) {
    const lo = j * capacity;
    const hi = Math.min(lo + capacity, payload.length);
    const nextPage = j + 1 < n ? indices[j + 1]! : 0;
    ovf.push({
      index: indices[j]!,
      itemCount: hi - lo,
      nextPage,
      payload: payload.subarray(lo, hi),
    });
  }
  return indices[0]!;
}

function statisticsDistributionEligible(type: Type): boolean {
  if (type.kind === "composite" || type.kind === "array") return false;
  return (
    type.kind === "range" ||
    (type.scalar !== "json" && type.scalar !== "jsonb" && type.scalar !== "jsonpath")
  );
}

// P9 kind-4 catalog entries in canonical (table, column, subkind, ordinal) order.
function statisticsCatalogEntries(snap: Snapshot): Uint8Array[] {
  const entries: Uint8Array[] = [];
  const tableKeys = [...snap.statistics.keys()].sort();
  for (const tableKey of tableKeys) {
    const table = snap.tables.get(tableKey)!;
    const columns = snap.statistics.get(tableKey)!;
    for (const column of [...columns.keys()].sort((a, b) => a - b)) {
      const statistics = columns.get(column)!;
      const summary = new ByteWriter();
      summary.u8(4);
      summary.u8(0);
      wstr(summary, table.name);
      summary.u16(column);
      summary.u8((statistics.stale ? 1 : 0) | (statistics.distinctCount !== null ? 2 : 0));
      summary.i64(statistics.analyzedRows);
      summary.i64(statistics.nullCount);
      summary.i64(statistics.widthSum);
      summary.i64(statistics.distinctCount ?? 0n);
      summary.u32(statistics.sampleRows);
      summary.u32(statistics.sampleNonNullRows);
      summary.u16(statistics.mcv.length);
      summary.u16(statistics.histogram.length);
      entries.push(summary.toBytes());

      const colType = snap.stores.get(tableKey)!.columnTypes()[column]!;
      for (let ordinal = 0; ordinal < statistics.mcv.length; ordinal++) {
        const mcv = statistics.mcv[ordinal]!;
        const entry = new ByteWriter();
        entry.u8(4);
        entry.u8(1);
        wstr(entry, table.name);
        entry.u16(column);
        entry.u16(ordinal);
        entry.u32(mcv.frequency);
        const encoded = encodeValue(colType, mcv.value.value);
        entry.u16(encoded.length);
        entry.bytes(encoded);
        entries.push(entry.toBytes());
      }
      for (let ordinal = 0; ordinal < statistics.histogram.length; ordinal++) {
        const bound = statistics.histogram[ordinal]!;
        const entry = new ByteWriter();
        entry.u8(4);
        entry.u8(2);
        wstr(entry, table.name);
        entry.u16(column);
        entry.u16(ordinal);
        const encoded = encodeValue(colType, bound.value);
        entry.u16(encoded.length);
        entry.bytes(encoded);
        entries.push(entry.toBytes());
      }
    }
  }
  return entries;
}

function decodeStatisticsValue(
  buf: Uint8Array,
  cur: Cursor,
  snap: Snapshot,
  tableKey: string,
  column: number,
): StatisticsValue {
  const table = snap.tables.get(tableKey);
  if (table === undefined || column >= table.columns.length) {
    throw engineError("data_corrupted", "statistics reference an unknown table or column");
  }
  const colType = snap.stores.get(tableKey)!.columnTypes()[column]!;
  const valueLength = readU16(buf, cur);
  if (valueLength === 0 || valueLength > STATISTICS_MAX_VALUE_BYTES) {
    throw engineError("data_corrupted", "invalid statistics value length");
  }
  const encoded = take(buf, cur, valueLength);
  const valueCursor = { pos: 0 };
  const value = readValue(colType, encoded, valueCursor, null, []);
  if (
    valueCursor.pos !== encoded.length ||
    compareBytes(encodeValue(colType, value), encoded) !== 0
  ) {
    throw engineError("data_corrupted", "noncanonical statistics value");
  }
  if (value.kind === "null") {
    throw engineError("data_corrupted", "statistics values may not be NULL");
  }
  const declared = table.columns[column]!;
  const collationSkewed =
    declared.collation !== null && snap.collationSkew(declared.collation) !== undefined;
  const coll =
    declared.collation === null ? null : (snap.resolveCollation(declared.collation) ?? null);
  let key: Uint8Array = new Uint8Array();
  if (!collationSkewed) {
    try {
      key = encodeTypedKey(declared.type, value, coll);
    } catch {
      throw engineError("data_corrupted", "invalid statistics comparison value");
    }
  }
  // A skewed collation's values were ordered by the file-pinned bundle. Their value bytes remain
  // canonical, but rebuilding comparison keys with the loaded bundle cannot validate the old order.
  // The estimator ignores these facts and upgradeCollations clears them.
  if (
    encodeValue(colType, value).length - 1 > STATISTICS_MAX_VALUE_BYTES ||
    key.length > STATISTICS_MAX_VALUE_BYTES
  ) {
    throw engineError("data_corrupted", "oversized persisted statistics value");
  }
  return { value, key };
}

function decodeStatisticsEntry(
  buf: Uint8Array,
  cur: Cursor,
  snap: Snapshot,
  expected: Map<string, readonly [number, number]>,
): void {
  const subkind = readU8(buf, cur);
  const tableKey = readString(buf, cur).toLowerCase();
  const column = readU16(buf, cur);
  const table = snap.tables.get(tableKey);
  if (table === undefined)
    throw engineError("data_corrupted", "statistics reference an unknown table");
  if (column >= table.columns.length)
    throw engineError("data_corrupted", "statistics reference an unknown column");
  const declared = table.columns[column]!;
  const collationSkewed =
    declared.collation !== null && snap.collationSkew(declared.collation) !== undefined;
  const groupKey = `${tableKey}\0${column}`;

  if (subkind === 0) {
    if (expected.has(groupKey)) throw engineError("data_corrupted", "duplicate statistics summary");
    const flags = readU8(buf, cur);
    const analyzedRows = readI64(buf, cur);
    const nullCount = readI64(buf, cur);
    const widthSum = readI64(buf, cur);
    const distinctRaw = readI64(buf, cur);
    const sampleRows = readU32(buf, cur);
    const sampleNonNullRows = readU32(buf, cur);
    const mcvCount = readU16(buf, cur);
    const histogramCount = readU16(buf, cur);
    const distribution = (flags & 2) !== 0;
    if (
      (flags & ~3) !== 0 ||
      analyzedRows < 0n ||
      nullCount < 0n ||
      nullCount > analyzedRows ||
      widthSum < 0n ||
      BigInt(sampleRows) > analyzedRows ||
      sampleRows > STATISTICS_SAMPLE_ROWS ||
      sampleNonNullRows > sampleRows ||
      mcvCount > STATISTICS_MCV_ENTRIES ||
      histogramCount > STATISTICS_HISTOGRAM_BOUNDS ||
      (distribution && distinctRaw < 0n) ||
      (!distribution && distinctRaw !== 0n) ||
      (distribution && distinctRaw > analyzedRows - nullCount) ||
      (histogramCount !== 0 && histogramCount < 2) ||
      distribution !== statisticsDistributionEligible(table.columns[column]!.type)
    ) {
      throw engineError("data_corrupted", "invalid statistics summary");
    }
    const statistics: ColumnStatistics = {
      analyzedRows,
      stale: (flags & 1) !== 0,
      nullCount,
      widthSum,
      distinctCount: distribution ? distinctRaw : null,
      sampleRows,
      sampleNonNullRows,
      mcv: [],
      histogram: [],
    };
    snap.putColumnStatistics(tableKey, column, statistics);
    expected.set(groupKey, [mcvCount, histogramCount]);
    return;
  }

  const counts = expected.get(groupKey);
  if (counts === undefined) {
    throw engineError(
      "data_corrupted",
      subkind === 1
        ? "statistics MCV precedes its summary"
        : "statistics histogram precedes its summary",
    );
  }
  const statistics = snap.columnStatistics(tableKey, column)!;
  if (subkind === 1) {
    const ordinal = readU16(buf, cur);
    const frequency = readU32(buf, cur);
    const value = decodeStatisticsValue(buf, cur, snap, tableKey, column);
    if (
      ordinal !== statistics.mcv.length ||
      ordinal >= counts[0] ||
      frequency === 0 ||
      frequency > statistics.sampleNonNullRows
    ) {
      throw engineError("data_corrupted", "invalid statistics MCV ordinal or frequency");
    }
    if (
      !collationSkewed &&
      statistics.mcv.some((existing) => compareBytes(existing.value.key, value.key) === 0)
    ) {
      throw engineError("data_corrupted", "duplicate statistics MCV value");
    }
    const previous = statistics.mcv.at(-1);
    if (
      !collationSkewed &&
      previous !== undefined &&
      (frequency > previous.frequency ||
        (frequency === previous.frequency && compareBytes(value.key, previous.value.key) < 0))
    ) {
      throw engineError("data_corrupted", "statistics MCV values are out of order");
    }
    statistics.mcv.push({ value, frequency });
    return;
  }
  if (subkind === 2) {
    const ordinal = readU16(buf, cur);
    const value = decodeStatisticsValue(buf, cur, snap, tableKey, column);
    if (ordinal !== statistics.histogram.length || ordinal >= counts[1]) {
      throw engineError("data_corrupted", "invalid statistics histogram ordinal");
    }
    const previous = statistics.histogram.at(-1);
    if (!collationSkewed && previous !== undefined && compareBytes(previous.key, value.key) > 0) {
      throw engineError("data_corrupted", "statistics histogram is out of order");
    }
    statistics.histogram.push(value);
    return;
  }
  throw engineError("data_corrupted", "unknown statistics entry subkind");
}

// tableEntryBytes builds one table's catalog entry (format.md). indexRoots is each
// index's tree root page, parallel to table.indexes.
function tableEntryBytes(
  table: Table,
  rootDataPage: number,
  indexRoots: number[],
  rowCount: bigint,
): Uint8Array {
  if (rowCount < 0n || rowCount > 9223372036854775807n) {
    throw new Error("table row count must fit a nonnegative i64");
  }
  if ((rootDataPage === 0) !== (rowCount === 0n)) {
    throw new Error("table root and row count must agree");
  }
  const w = new ByteWriter();
  const nameB = UTF8.encode(table.name);
  w.u16(nameB.length);
  w.bytes(nameB);
  w.u16(table.columns.length);
  for (const col of table.columns) {
    const cn = UTF8.encode(col.name);
    w.u16(cn.length);
    w.bytes(cn);
    if (col.type.kind === "composite") {
      // A composite column (v9): type_code 14, then flags, then the type name in the typmod slot
      // (spec/fileformat/format.md). Forward-ready — composite columns are not produced this slice
      // (composite.md §12), but the encoder handles the case so a later-slice file writes cleanly.
      // Composite columns carry no default this slice, so flags bits 2/3 are 0.
      w.u8(14);
      w.u8(col.notNull ? 0b10 : 0);
      const tn = UTF8.encode(col.type.name);
      w.u16(tn.length);
      w.bytes(tn);
      continue;
    }
    if (col.type.kind === "array") {
      // An array column (v10): type_code 15, flags, then the element type descriptor
      // (spec/design/array.md §3). Arrays carry no default this slice (flags bits 2/3 = 0).
      w.u8(15);
      w.u8(col.notNull ? 0b10 : 0);
      pushArrayElementType(w, col.type.elem);
      continue;
    }
    if (col.type.kind === "range") {
      // A range column (v16): type_code 17, flags, then the element type descriptor — one scalar
      // code (spec/design/ranges.md §3). Ranges carry no default this slice (flags bits 2/3 = 0).
      w.u8(17);
      w.u8(col.notNull ? 0b10 : 0);
      pushRangeElementType(w, col.type.elem);
      continue;
    }
    const s = typeScalar(col.type);
    w.u8(typeCodeForScalar(s));
    // bit0 (primary_key through v4) is RETIRED in v5 — the pk ordinal list below is the
    // single authority; the bit is reserved, written 0 (spec/fileformat/format.md).
    let flags = 0;
    if (col.notNull) flags |= 0b10;
    if (col.default !== null) flags |= 0b100;
    // bit3 default_is_expr (v8) — mutually exclusive with bit2 (a column has at most one of a
    // constant or an expression default — spec/fileformat/format.md).
    if (col.defaultExpr !== null) flags |= 0b1000;
    // bit4 is_identity + bit5 identity_always (v15) — an IDENTITY column also carries not_null
    // (bit1) + the nextval expression default (bit3) — spec/design/sequences.md §13.
    if (col.identity !== null) {
      flags |= 0b1_0000;
      if (col.identity === "always") flags |= 0b10_0000;
    }
    // bit6 has_collation (v17) — a text column with a non-C effective collation
    // (spec/design/collation.md §5); the name is appended after the default.
    if (col.collation !== null) flags |= 0b100_0000;
    w.u8(flags);
    // A decimal column appends its typmod (precision, scale) — only for type_code 6, so
    // non-decimal entries are byte-unchanged (format.md). precision 0 = unconstrained numeric.
    if (s === "decimal") {
      w.u16(col.decimal ? col.decimal.precision : 0);
      w.u16(col.decimal ? col.decimal.scale : 0);
    }
    // A text column appends its varchar(n) max length — only for type_code 4 (v22). 0 = unbounded,
    // so a plain text column carries 0 (spec/design/types.md §15).
    if (s === "text") {
      w.u32(col.varcharLen ?? 0);
    }
    // A column with a constant DEFAULT (flags bit2) appends its pre-evaluated default value via
    // the same value codec rows use — AFTER the typmod, presence-gated, so a column without a
    // default is byte-unchanged (format.md). A DEFAULT NULL is one 0x01. An EXPRESSION default
    // (flags bit3, v8) instead appends its expr-text (u16 length + UTF-8) there, the same token
    // rendering a CHECK uses — bit2/bit3 are exclusive.
    if (col.default !== null) {
      // A column DEFAULT is always a scalar value (composite columns carry no default this slice —
      // composite.md §12), so encode the scalar body directly.
      w.bytes(encodeScalar(s, col.default));
    } else if (col.defaultExpr !== null) {
      const et = UTF8.encode(col.defaultExpr.exprText);
      w.u16(et.length);
      w.bytes(et);
    }
    // The effective collation name (v17, flags bit6) — last in the per-column entry, so a
    // non-collated column is byte-unchanged (spec/design/collation.md §5).
    if (col.collation !== null) {
      const cb = UTF8.encode(col.collation);
      w.u16(cb.length);
      w.bytes(cb);
    }
  }
  // The primary key (v5): count, then the member column ordinals in KEY order
  // (constraints.md §3 — the list persists an order independent of declaration order).
  w.u16(table.pk.length);
  for (const i of table.pk) w.u16(i);
  // CHECK constraints (v4): count, then (name, expression text) per check, in the
  // catalog's evaluation order — the text is written back VERBATIM, so the bytes are
  // stable across create → commit → load → commit (spec/fileformat/format.md
  // "Check-expression text").
  w.u16(table.checks.length);
  for (const check of table.checks) {
    const cn = UTF8.encode(check.name);
    w.u16(cn.length);
    w.bytes(cn);
    const ce = UTF8.encode(check.exprText);
    w.u16(ce.length);
    w.bytes(ce);
  }
  // Secondary indexes (v5): count, then per index the name, key-column ordinals
  // (index-key order, duplicates allowed), the v6 flags byte (bit0 unique —
  // spec/design/indexes.md §8), and its tree's root page — in the catalog's ascending
  // lowercased-name order (spec/design/indexes.md §6).
  w.u16(table.indexes.length);
  for (let k = 0; k < table.indexes.length; k++) {
    const idx = table.indexes[k]!;
    const inm = UTF8.encode(idx.name);
    w.u16(inm.length);
    w.bytes(inm);
    w.u16(idx.keys.length);
    for (const k of idx.keys) {
      if (k.kind === "column") {
        // A column key: its ordinal (< col_count, so never 0xFFFF).
        w.u16(k.column);
      } else {
        // An expression key (v26): the 0xFFFF sentinel, then the canonical text (u16 len +
        // UTF-8) — spec/design/indexes.md §6, format.md.
        w.u16(0xffff);
        wstr(w, k.exprText);
      }
    }
    // index_flags: bit0 unique (v6), bit1 has_predicate (v27 — a partial index, indexes.md §9).
    w.u8((idx.unique ? 1 : 0) | (idx.predicate ? 2 : 0));
    // v13: index_kind byte (0 = btree, 1 = GIN); v20: 2 = GiST (gist.md §8).
    w.u8(idx.kind === "gist" ? 2 : idx.kind === "gin" ? 1 : 0);
    w.u32(indexRoots[k]!);
    // v27: a partial index's predicate canonical text (u16 len + UTF-8) after index_root_page.
    if (idx.predicate) wstr(w, idx.predicate.exprText);
  }
  // Foreign keys (v11): count, then per FK the name, the local-column ordinals (into THIS
  // table, list order), the referenced table name, the referenced-column ordinals (into the
  // PARENT, list order), and the actions byte (bits 0-1 on_delete, bits 2-3 on_update) — in the
  // catalog's ascending lowercased-name order (spec/design/constraints.md §6.9). An FK owns no
  // B-tree (no root page).
  w.u16(table.fks.length);
  for (const fk of table.fks) {
    const fnm = UTF8.encode(fk.name);
    w.u16(fnm.length);
    w.bytes(fnm);
    w.u16(fk.columns.length);
    for (const c of fk.columns) w.u16(c);
    const rt = UTF8.encode(fk.refTable);
    w.u16(rt.length);
    w.bytes(rt);
    w.u16(fk.refColumns.length);
    for (const c of fk.refColumns) w.u16(c);
    w.u8(fkActionCode(fk.onDelete) | (fkActionCode(fk.onUpdate) << 2));
  }
  // EXCLUDE constraints (v21): count, then per exclusion the name, the backing GiST index name, and
  // the (column ordinal u16, operator strategy u8) element vector (&& = 0, = 1), in ascending
  // lowercased-name order (spec/design/gist.md §7/§8). The backing index is stored like any GiST
  // index (in the index list above); this entry layers the operator vector the probe needs.
  w.u16(table.exclusions.length);
  for (const ex of table.exclusions) {
    const en = UTF8.encode(ex.name);
    w.u16(en.length);
    w.bytes(en);
    const ei = UTF8.encode(ex.index);
    w.u16(ei.length);
    w.bytes(ei);
    w.u16(ex.elements.length);
    for (const el of ex.elements) {
      w.u16(el.column);
      w.u8(el.op === "equal" ? 1 : 0);
    }
  }
  w.u32(rootDataPage);
  w.i64(rowCount);
  return w.toBytes();
}

// fkActionCode is the 2-bit on-disk code for a referential action (format.md): NO ACTION = 0,
// RESTRICT = 1.
function fkActionCode(a: FkAction): number {
  return a === "restrict" ? 1 : 0;
}

// fkActionFromCode decodes a 2-bit referential-action code; an unsupported code (2/3, reserved
// for the deferred write-actions) in an otherwise-valid file is XX001.
function fkActionFromCode(c: number): FkAction {
  if (c === 0) return "noAction";
  if (c === 1) return "restrict";
  throw engineError("data_corrupted", "unsupported foreign-key action code");
}

// compositeTypeEntryBytes serializes a composite-type catalog entry's BODY (after its
// entry_kind = 1 byte): name, field count, then per field — name, type code, [type name when code
// 14 (nested composite)], flags (bit0 not_null), [decimal typmod when code 6]
// (spec/fileformat/format.md *Composite-type entry*).
function compositeTypeEntryBytes(ct: CompositeType): Uint8Array {
  const w = new ByteWriter();
  const nameB = UTF8.encode(ct.name);
  w.u16(nameB.length);
  w.bytes(nameB);
  w.u16(ct.fields.length);
  for (const f of ct.fields) {
    const fn = UTF8.encode(f.name);
    w.u16(fn.length);
    w.bytes(fn);
    if (f.type.kind === "composite") {
      w.u8(14);
      const tn = UTF8.encode(f.type.name);
      w.u16(tn.length);
      w.bytes(tn);
    } else if (f.type.kind === "array") {
      // An array-typed field (spec/design/array.md §12): type_code 15, then the same inline
      // element-type descriptor an array column uses (§3), before the flags byte — mirroring where
      // a nested-composite field's name sits.
      w.u8(15);
      pushArrayElementType(w, f.type.elem);
    } else if (f.type.kind === "range") {
      throw new Error("a composite range field is rejected at CREATE TYPE (R2)");
    } else {
      w.u8(typeCodeForScalar(f.type.scalar));
    }
    w.u8(f.notNull ? 0b1 : 0);
    if (f.type.kind === "scalar" && f.type.scalar === "decimal") {
      w.u16(f.decimal ? f.decimal.precision : 0);
      w.u16(f.decimal ? f.decimal.scale : 0);
    }
    // A text field appends its varchar(n) max length (v22); 0 = unbounded (types.md §15).
    if (f.type.kind === "scalar" && f.type.scalar === "text") {
      w.u32(f.varcharLen ?? 0);
    }
  }
  return w.toBytes();
}

// decodeCompositeTypeEntry decodes a composite-type catalog entry's body (inverse of
// compositeTypeEntryBytes); the caller has already consumed the entry_kind byte. Nested composite
// fields hold the referenced type's NAME (resolved/validated after the whole catalog is read — the
// two-pass load).
function decodeCompositeTypeEntry(buf: Uint8Array, cur: Cursor): CompositeType {
  const name = readString(buf, cur);
  const fieldCount = readU16(buf, cur);
  const fields: CompositeField[] = [];
  for (let i = 0; i < fieldCount; i++) {
    const fname = readString(buf, cur);
    const tc = readU8(buf, cur);
    let fty: Type;
    let decimal: DecimalTypmod | null = null;
    if (tc === 14) {
      const tn = readString(buf, cur);
      fty = compositeT(tn);
    } else if (tc === 15) {
      // An array-typed field (spec/design/array.md §12): the element-type descriptor, then (below)
      // the flags byte — the inverse of the array arm in compositeTypeEntryBytes.
      fty = arrayT(readArrayElementType(buf, cur));
    } else {
      const s = scalarForTypeCode(tc);
      if (s === undefined) throw engineError("data_corrupted", "unknown field type code");
      fty = scalarT(s);
    }
    const flags = readU8(buf, cur);
    if ((flags & ~0b1) !== 0) {
      throw engineError("data_corrupted", "reserved composite field flag set");
    }
    const notNull = (flags & 0b1) !== 0;
    if (fty.kind === "scalar" && fty.scalar === "decimal") {
      const precision = readU16(buf, cur);
      const scale = readU16(buf, cur);
      if (precision !== 0) decimal = { precision, scale };
    }
    // A text field carries its varchar(n) max length (v22); 0 = unbounded (types.md §15).
    let varcharLen: number | null = null;
    if (fty.kind === "scalar" && fty.scalar === "text") {
      const n = readU32(buf, cur);
      if (n !== 0) varcharLen = n;
    }
    fields.push({ name: fname, type: fty, decimal, varcharLen, notNull });
  }
  return { name, fields };
}

// sequenceEntryBytes serializes a sequence catalog entry's BODY (after its entry_kind = 2 byte):
// name, then the six fixed i64 fields (big-endian two's-complement, no sign-flip) and a flags byte
// — spec/fileformat/format.md *Sequence entry*. Fixed-width, every field present (no presence tags).
function sequenceEntryBytes(s: SequenceDef): Uint8Array {
  const w = new ByteWriter();
  const nameB = UTF8.encode(s.name);
  w.u16(nameB.length);
  w.bytes(nameB);
  w.i64(s.increment);
  w.i64(s.minValue);
  w.i64(s.maxValue);
  w.i64(s.start);
  w.i64(s.cache);
  w.i64(s.lastValue);
  let flags = 0;
  if (s.cycle) flags |= 0b1;
  if (s.isCalled) flags |= 0b10;
  if (s.ownedBy !== undefined) flags |= 0b100; // bit2 has_owner (v13)
  w.u8(flags);
  // The OWNED BY tail (v13): only present when has_owner — owner table name + column ordinal
  // (spec/design/sequences.md §12, format.md *Sequence entry*).
  if (s.ownedBy !== undefined) {
    const tableB = UTF8.encode(s.ownedBy.table);
    w.u16(tableB.length);
    w.bytes(tableB);
    w.u16(s.ownedBy.column);
  }
  return w.toBytes();
}

// decodeSequenceEntry decodes a sequence catalog entry's body (inverse of sequenceEntryBytes); the
// caller has already consumed the entry_kind byte. A sequence is self-contained — registered
// directly, no two-pass (spec/design/sequences.md §3).
function decodeSequenceEntry(buf: Uint8Array, cur: Cursor): SequenceDef {
  const name = readString(buf, cur);
  const increment = readI64(buf, cur);
  const minValue = readI64(buf, cur);
  const maxValue = readI64(buf, cur);
  const start = readI64(buf, cur);
  const cache = readI64(buf, cur);
  const lastValue = readI64(buf, cur);
  const flags = readU8(buf, cur);
  if ((flags & ~0b111) !== 0) {
    throw engineError("data_corrupted", "reserved sequence flag set");
  }
  // The OWNED BY tail (v13): present iff bit2 (has_owner) is set.
  let ownedBy: SeqOwner | undefined;
  if ((flags & 0b100) !== 0) {
    const table = readString(buf, cur);
    const column = readU16(buf, cur);
    ownedBy = { table, column };
  }
  return {
    name,
    increment,
    minValue,
    maxValue,
    start,
    cache,
    cycle: (flags & 0b1) !== 0,
    lastValue,
    isCalled: (flags & 0b10) !== 0,
    ownedBy,
  };
}

// wstr writes a u16-length-prefixed UTF-8 string (the catalog's name/string encoding).
function wstr(w: ByteWriter, s: string): void {
  const b = UTF8.encode(s);
  w.u16(b.length);
  w.bytes(b);
}

// collationEntryBytes serializes a collation reference entry's BODY (after its entry_kind = 3 byte,
// v18): a flags byte (bit0 is_default), then metadata ONLY — name + unicodeVersion + cldrVersion +
// description, each u16-len + UTF-8. NO table: it is vendored into the binary and resolved by name on
// open (spec/design/collation.md §2/§5/§9).
function collationEntryBytes(c: Collation, isDefault: boolean): Uint8Array {
  const w = new ByteWriter();
  w.u8(isDefault ? 0b1 : 0);
  wstr(w, c.name);
  wstr(w, c.unicodeVersion);
  wstr(w, c.cldrVersion);
  wstr(w, c.description);
  return w.toBytes();
}

// decodeCollationEntry decodes a collation reference entry's body (inverse of collationEntryBytes);
// the caller has consumed the entry_kind byte. Reads the metadata, then resolves the compiled table
// from the binary's VENDORED set by name (§2/§9) — the table is no longer in the file. Returns the
// resolved collation + whether it is the per-database default (the is_default flag bit).
function decodeCollationEntry(
  buf: Uint8Array,
  cur: Cursor,
): { coll: Collation; isDefault: boolean } {
  const flags = readU8(buf, cur);
  if ((flags & ~0b1) !== 0) {
    throw engineError("data_corrupted", "reserved collation flag set");
  }
  const isDefault = (flags & 0b1) !== 0;
  const name = readString(buf, cur);
  const unicode = readString(buf, cur);
  const cldr = readString(buf, cur);
  const desc = readString(buf, cur);
  // The file records only the version PIN; the table comes from a loaded bundle (the host must have
  // loaded one providing this collation before opening — collation.md §4/§9). A name no loaded bundle
  // provides at all is the graded verdict's legible refusal (slice 2d, collation.md §12 /
  // compatibility.md §7): the open is refused with XX002 naming the collation + version, rather than
  // degrading the rest of the database (the conservative resolution of compatibility.md §12 open #3 —
  // a version-skewed collation, by contrast, opens and is enforced read-only at write time §14).
  const loaded = loadedCollation(name);
  if (loaded === undefined) {
    throw engineError(
      "collation_version_mismatch",
      `collation "${name}" (@ ${unicode}/${cldr}) is not provided by any loaded bundle`,
    );
  }
  const coll: Collation = {
    name,
    unicodeVersion: unicode,
    cldrVersion: cldr,
    description: desc,
    singles: loaded.singles,
    contractions: loaded.contractions,
  };
  return { coll, isDefault };
}

// pack greedily packs item sizes into pages of capacity `capacity`, returning groups of
// item indices. Empty input yields one empty group. A single item larger than capacity
// is unsupported (no overflow pages in step-5b).
function pack(sizes: number[], capacity: number): number[][] {
  const groups: number[][] = [];
  let cur: number[] = [];
  let used = 0;
  for (let i = 0; i < sizes.length; i++) {
    const sz = sizes[i]!;
    if (sz > capacity) {
      throw engineError(
        "feature_not_supported",
        "a record or table entry larger than a page is not supported",
      );
    }
    if (cur.length > 0 && used + sz > capacity) {
      groups.push(cur);
      cur = [];
      used = 0;
    }
    cur.push(i);
    used += sz;
  }
  groups.push(cur);
  return groups;
}

// toImage serializes a snapshot's whole state to one on-disk image (format.md). pageSize is
// recorded in the meta page; txid is written into both meta slots. Accepts a Snapshot directly
// (the writer's working snapshot at commit) or a Engine (serializing its committed snapshot —
// the form callers/tests holding a Engine use).
export function toImage(src: Engine | Snapshot, pageSize: number, txid: bigint): Uint8Array {
  const snap = src instanceof Snapshot ? src : src.committed;
  const ps = pageSize;
  if (ps < MIN_PAGE_SIZE) {
    throw engineError("feature_not_supported", "page size too small for the format");
  }
  if (ps > MAX_PAGE_SIZE) {
    throw engineError("feature_not_supported", "page size too large for the format");
  }
  if ((ps & (ps - 1)) !== 0) {
    throw engineError("feature_not_supported", "page size must be a power of two");
  }
  const capacity = ps - PAGE_HEADER;

  // Tables in ascending lowercased-name order (no map-iteration order leak).
  const keys = [...snap.tables.keys()].sort();

  // Serialize each table's B-tree post-order, body pages allocated from page 2. Each BodyPage is
  // (index, pageType, itemCount, payload); children precede their parent so parent child-pointers
  // reference already-allocated pages (format.md).
  const body: BodyPage[] = [];
  const rootDataPage: number[] = new Array(keys.length).fill(0);
  const indexRoots: number[][] = keys.map(() => []);
  let nextIndex = ROOT_PAGE;
  for (let ti = 0; ti < keys.length; ti++) {
    const store = snap.stores.get(keys[ti]!)!;
    const root = store.treeRoot();
    if (root !== null) {
      const r = serializeNode(root, store, capacity, nextIndex, body);
      rootDataPage[ti] = r.index;
      nextIndex = r.next;
    }
    // The table's index trees follow its data tree, in catalog (name) order
    // (spec/fileformat/format.md "From-scratch image").
    for (const idx of snap.tables.get(keys[ti]!)!.indexes) {
      let ir = 0;
      if (idx.kind === "gist") {
        // GiST: the on-disk form is the R-tree (pages 5/6), not the flat leaf store (gist.md §4.1).
        const r = serializeGistIndex(
          snap.indexStore(idx.name.toLowerCase()),
          gistColOpclasses(store, indexColumnOrdinals(idx)!),
          nextIndex,
          body,
        );
        ir = r.index;
        nextIndex = r.next;
      } else {
        const istore = snap.indexStore(idx.name.toLowerCase());
        const iroot = istore.treeRoot();
        if (iroot !== null) {
          const r = serializeNode(iroot, istore, capacity, nextIndex, body);
          ir = r.index;
          nextIndex = r.next;
        }
      }
      indexRoots[ti]!.push(ir);
    }
  }

  // The catalog chain follows the data; its head is the relocatable root_page. Each entry is
  // kind-tagged (v9/v12): composite-type entries (kind 1) first in lowercased-name order, then
  // sequence entries (kind 2, name order — v12), then table entries (kind 0) —
  // spec/fileformat/format.md.
  const catRoot = nextIndex;
  const catEntries: Uint8Array[] = [];
  for (const ct of snap.compositeTypesSorted()) {
    catEntries.push(concat([Uint8Array.of(1), compositeTypeEntryBytes(ct)]));
  }
  for (const s of snap.sequencesSorted()) {
    catEntries.push(concat([Uint8Array.of(2), sequenceEntryBytes(s)]));
  }
  // Collation reference entries (kind 3, v18) — after sequences, before tables, so a collated table
  // entry is read after the entry it references. Reference-only: emit one metadata entry per
  // collation the SCHEMA references (columns + default), not an imported set
  // (spec/design/collation.md §2/§5).
  for (const c of snap.referencedCollations()) {
    catEntries.push(
      concat([Uint8Array.of(3), collationEntryBytes(c, snap.defaultCollation === c.name)]),
    );
  }
  for (let ti = 0; ti < keys.length; ti++) {
    const t = snap.tables.get(keys[ti]!)!;
    const rowCount = snap.stores.get(keys[ti]!)!.count();
    if (rowCount === null) throw new Error("table stores always carry an exact row count");
    catEntries.push(
      concat([Uint8Array.of(0), tableEntryBytes(t, rootDataPage[ti]!, indexRoots[ti]!, rowCount)]),
    );
  }
  catEntries.push(...statisticsCatalogEntries(snap));
  const entrySizes = catEntries.map((e) => e.length);
  const catGroups = pack(entrySizes, capacity);
  const pageCount = catRoot + catGroups.length;

  const image = new Uint8Array(pageCount * ps);

  // Meta: both slots hold the current meta (a fresh from-scratch image has no distinct prior
  // version; slot alternation is the live incremental-commit path — format.md).
  writeMeta(image, ps, 0, pageSize, txid, catRoot, pageCount);
  writeMeta(image, ps, 1, pageSize, txid, catRoot, pageCount);

  // B-tree node + overflow pages.
  for (const bp of body) {
    writePage(image, ps, bp.index, bp.pageType, bp.itemCount, bp.nextPage, bp.payload);
  }

  // Catalog chain.
  for (let gi = 0; gi < catGroups.length; gi++) {
    const group = catGroups[gi]!;
    const index = catRoot + gi;
    const next = gi + 1 < catGroups.length ? index + 1 : 0;
    const parts = group.map((ei) => catEntries[ei]!);
    writePage(image, ps, index, PAGE_CATALOG, group.length, next, concat(parts));
  }

  return image;
}

// BodyPage is one serialized page awaiting write: its index, type, key count, chain link, payload.
// nextPage is 0 for B-tree nodes and the chain link for overflow pages (large-values.md §12).
type BodyPage = {
  index: number;
  pageType: number;
  itemCount: number;
  nextPage: number;
  payload: Uint8Array;
};

// gistColOpclasses are the per-column opclasses of a GiST index (spec/design/gist.md §5/§6/§7): one
// per indexed column — range_ops over a range column (its element ColType the codec key), the scalar
// `=` opclass over a fixed-width keyable scalar. Single for a GX1/GX2 index, one per WITH column for
// an EXCLUDE backing index. Taken from the TABLE store's resolved column types so it matches the
// executor's gistEntries encoding; the bound flavor a node holds is keyed off the column type.
function gistColOpclasses(tableStore: TableStore, cols: number[]): GistOpclass[] {
  const types = tableStore.columnTypes();
  return cols.map((ci) => {
    const ct = types[ci]!;
    return ct.kind === "range" ? gistRangeOpclass(ct.elem) : GIST_SCALAR_OPCLASS;
  });
}

// serializeGistIndex builds a GiST index's canonical R-tree from its leaf-key store and serializes it
// to node pages (spec/design/gist.md §3/§4.1). The on-disk form of a GiST index is the R-tree (page
// types 5/6), NOT the flat leaf-key B-tree the in-memory index store holds. The tree is rebuilt
// CANONICALLY from the leaf set, so its bytes are a pure function of the set — content-deterministic
// and cross-core identical (§3). Returns the root page + the next free index; an empty index returns
// root 0 and writes no pages.
function serializeGistIndex(
  istore: TableStore,
  ops: GistOpclass[],
  nextIndex: number,
  body: BodyPage[],
): { index: number; next: number } {
  const keys = istore.entriesInKeyOrder().map((e) => e.key);
  if (keys.length === 0) return { index: 0, next: nextIndex };
  const tree = buildGistFromLeafKeys(ops, keys);
  let n = nextIndex;
  const { pages, root } = serializeGistTree(tree, ops, () => n++);
  for (const p of pages) {
    body.push({
      index: p.pageNo,
      pageType: p.pageType,
      itemCount: p.itemCount,
      nextPage: 0,
      payload: p.payload,
    });
  }
  return { index: root, next: n };
}

// serializeNode serializes one node and its subtree post-order, appending each to `body`, and
// returns this node's assigned page index and the next free index. A leaf's payload is its records;
// an interior's is its N+1 child pointers (big-endian u32) then its N records (format.md). A node
// whose payload would exceed the page is an oversized record (over RECORD_MAX) → feature_not_supported.
function serializeNode(
  n: PNode,
  store: TableStore,
  capacity: number,
  nextIndex: number,
  body: BodyPage[],
): { index: number; next: number } {
  const colTypes = store.columnTypes();
  const childPages: number[] = [];
  for (const c of n.children) {
    // Whole-image serialize renumbers pages from scratch. Under B3 (bplus-reshape.md) every
    // database — in-memory included — is demand-paged, so a clean leaf may be an OnDisk reference
    // into the source store: fault it through the store's pool for the duration of its own
    // serialization (whole-image serialize is not a hot path).
    const child = c.node !== null ? c.node : store.faultLeaf(c.page);
    const r = serializeNode(child, store, capacity, nextIndex, body);
    childPages.push(r.index);
    nextIndex = r.next;
  }
  const index = nextIndex;
  nextIndex++;

  // Encode a leaf's records, spilling over-large values to overflow pages allocated after this
  // node's index (post-order traversal + record-then-column order → deterministic,
  // golden-pinnable). An INTERIOR node is the record-free keys+children skeleton (v24) — no
  // values, no chains.
  const ovf: OverflowPageOut[] = [];
  const take = (): number => nextIndex++;
  let pageType = PAGE_LEAF;
  let payload: Uint8Array;
  if (n.children.length > 0) {
    pageType = PAGE_INTERIOR;
    payload = encodeInterior(n.keys, childPages);
  } else {
    // A leaf may be Packed here: a demand-paged load keeps leaves as page blocks and toImage
    // re-serializes them. Materialize through the seam (decodedRows reconstructs a Packed leaf,
    // clones a Decoded one), then resolve any lazily-deferred large values through the store's
    // pager (large-values.md §14) so encode sees resident bytes. Whole-image serialize is not a
    // hot path (create's empty image / golden generator / toImage canonical), so the clones are
    // acceptable (packed-leaf.md §7).
    const rows = decodedRows(n).map((row) => store.resolveAll(row));
    payload = encodeLeafPax(colTypes, keyViews(n), rows, capacity, take, ovf);
  }
  if (payload.length > capacity) {
    throw engineError(
      "feature_not_supported",
      "a record larger than the per-row limit is not supported",
    );
  }
  body.push({ index, pageType, itemCount: nodeLen(n), nextPage: 0, payload });
  for (const o of ovf) {
    body.push({
      index: o.index,
      pageType: PAGE_OVERFLOW,
      itemCount: o.itemCount,
      nextPage: o.nextPage,
      payload: o.payload,
    });
  }
  return { index, next: nextIndex };
}

// IncrementalWrite is the pages an incremental commit must write durably, plus the new catalog root
// and high-water for the meta slot (spec/fileformat/format.md, P6.1 part B). file.ts pwrites pages,
// then publishes rootPage/pageCount in the alternate meta slot.
export type IncrementalWrite = {
  pages: { index: number; bytes: Uint8Array }[];
  rootPage: number;
  pageCount: number;
  // freeRemaining is the free-list entries this commit did not consume by its tree/catalog pages — all
  // pages dead at the fallback (prior) snapshot, so safe to overwrite this commit. The durable path draws
  // its persisted page_type 7 free-list pages from these (never the high-water) and reclaims this commit's
  // fresh orphans into the persisted list too (serializeFreeList / planFreeList — v25).
  freeRemaining: number[];
};

// PageAlloc hands out page indices for an incremental commit: the free-list first (lowest index, the
// pages a prior root abandoned — spec/fileformat/format.md *Reclamation*), then fresh indices at the
// high-water once the free-list is exhausted. The free-list is pre-sorted ascending, so lowest-first
// allocation is deterministic and the bytes stay cross-core identical. Reusing a free page is
// torn-write-safe: it left the free-list only here, becoming part of the new committed version, so it
// is reachable from no fallback snapshot.
class PageAlloc {
  private free: number[];
  cursor = 0;
  next: number;
  // reuse gates whether this commit draws from the free-list. false ⇒ allocate high-water only, leaving
  // the whole free-list unconsumed (cursor stays 0, so remaining() carries it all through for
  // persistence): the reader-liveness watermark defers reusing a page a still-open reader on an older
  // snapshot could observe (transactions.md §8 — the free-list generation gate). Reconstruct-on-open and
  // the single-handle case leave it true (oldest_live == committed ⇒ no page is still observed), so the
  // on-disk byte layout is unchanged whenever no reader pins an older version.
  private reuse: boolean;

  constructor(free: number[], next: number, reuse = true) {
    this.free = free;
    this.next = next;
    this.reuse = reuse;
  }

  take(): number {
    if (this.reuse && this.cursor < this.free.length) return this.free[this.cursor++]!;
    return this.next++;
  }

  remaining(): number[] {
    return this.free.slice(this.cursor);
  }
}

// incrementalImage assembles the dirty body pages + freshly-rewritten catalog for an incremental
// commit, appending page allocation from startPage (the on-disk high-water) — the write path's
// counterpart to the whole-image toImage (spec/fileformat/format.md, *Allocation & incremental
// commit*). Only dirty nodes are emitted (clean subtrees keep their pages — the incremental win); the
// catalog chain is always rewritten (it carries each table's possibly-moved root). The dirty nodes'
// set-once page ids are assigned here. The page size was validated at file creation, so no size check
// is repeated.
export function incrementalImage(
  snap: Snapshot,
  pageSize: number,
  startPage: number,
  free: number[],
  paging: SharedPaging | null,
  reuse = true,
): IncrementalWrite {
  const ps = pageSize;
  const capacity = ps - PAGE_HEADER;

  const keys = [...snap.tables.keys()].sort();

  // Allocate from the free-list first (reclaiming dead pages), then extend the file — unless the
  // watermark defers reuse (reuse false), in which case only the high-water is drawn and the whole
  // free-list carries through unconsumed for persistence (PageAlloc.reuse, transactions.md §8).
  const alloc = new PageAlloc(free, startPage, reuse);

  const pages: { index: number; bytes: Uint8Array }[] = [];
  const rootDataPage: number[] = new Array(keys.length).fill(0);
  const indexRoots: number[][] = keys.map(() => []);
  const indexColTypes: ColType[] = [];
  for (let ti = 0; ti < keys.length; ti++) {
    const store = snap.stores.get(keys[ti]!)!;
    const root = store.treeRoot();
    if (root !== null) {
      rootDataPage[ti] = serializeDirty(
        root,
        store.columnTypes(),
        capacity,
        ps,
        alloc,
        pages,
        paging,
      );
    }
    // The table's index trees follow its data tree, in catalog (name) order — only their
    // dirty nodes are written, like any tree (spec/fileformat/format.md "Allocation &
    // incremental commit").
    for (const idx of snap.tables.get(keys[ti]!)!.indexes) {
      let ir = 0;
      if (idx.kind === "gist") {
        // GiST rewrites its WHOLE R-tree every commit (gist.md §4.1(b)): fresh pages from the
        // allocator (free-list first), the old tree's pages reclaimed on the next open.
        const istore = snap.indexStore(idx.name.toLowerCase());
        const keysList = istore.entriesInKeyOrder().map((e) => e.key);
        if (keysList.length > 0) {
          const ops = gistColOpclasses(store, indexColumnOrdinals(idx)!);
          const tree = buildGistFromLeafKeys(ops, keysList);
          const { pages: gpages, root } = serializeGistTree(tree, ops, () => alloc.take());
          for (const p of gpages) {
            pages.push({
              index: p.pageNo,
              bytes: makePage(ps, p.pageType, p.itemCount, 0, p.payload),
            });
          }
          ir = root;
        }
      } else {
        const iroot = snap.indexStore(idx.name.toLowerCase()).treeRoot();
        if (iroot !== null) {
          ir = serializeDirty(iroot, indexColTypes, capacity, ps, alloc, pages, paging);
        }
      }
      indexRoots[ti]!.push(ir);
    }
  }

  // The catalog chain is rewritten to fresh pages every commit (table roots move). Allocate its page
  // indices up front — they may be reused free pages, hence not contiguous — so each page can point at
  // the next (`pack` always returns ≥ 1 group, so catPages is non-empty). Entries are kind-tagged
  // (v9/v12): composite-type entries (kind 1, name order) then sequence entries (kind 2, name order —
  // v12) then table entries (kind 0) — spec/fileformat/format.md.
  const catEntries: Uint8Array[] = [];
  for (const ct of snap.compositeTypesSorted()) {
    catEntries.push(concat([Uint8Array.of(1), compositeTypeEntryBytes(ct)]));
  }
  for (const s of snap.sequencesSorted()) {
    catEntries.push(concat([Uint8Array.of(2), sequenceEntryBytes(s)]));
  }
  // Collation reference entries (kind 3, v18) — after sequences, before tables, so a collated table
  // entry is read after the entry it references. Reference-only: emit one metadata entry per
  // collation the SCHEMA references (columns + default), not an imported set
  // (spec/design/collation.md §2/§5).
  for (const c of snap.referencedCollations()) {
    catEntries.push(
      concat([Uint8Array.of(3), collationEntryBytes(c, snap.defaultCollation === c.name)]),
    );
  }
  for (let ti = 0; ti < keys.length; ti++) {
    const t = snap.tables.get(keys[ti]!)!;
    const rowCount = snap.stores.get(keys[ti]!)!.count();
    if (rowCount === null) throw new Error("table stores always carry an exact row count");
    catEntries.push(
      concat([Uint8Array.of(0), tableEntryBytes(t, rootDataPage[ti]!, indexRoots[ti]!, rowCount)]),
    );
  }
  catEntries.push(...statisticsCatalogEntries(snap));
  const entrySizes = catEntries.map((e) => e.length);
  const catGroups = pack(entrySizes, capacity);
  const catPages = catGroups.map(() => alloc.take());
  const catRoot = catPages[0]!;
  for (let gi = 0; gi < catGroups.length; gi++) {
    const group = catGroups[gi]!;
    const nextPage = gi + 1 < catGroups.length ? catPages[gi + 1]! : 0;
    const parts = group.map((ei) => catEntries[ei]!);
    pages.push({
      index: catPages[gi]!,
      bytes: makePage(ps, PAGE_CATALOG, group.length, nextPage, concat(parts)),
    });
  }

  return { pages, rootPage: catRoot, pageCount: alloc.next, freeRemaining: alloc.remaining() };
}

// resolveForEncode materializes any unfetched values in `row` for re-encoding at commit
// (spec/design/large-values.md §14): a dirty leaf may carry rows the lazy load left as
// references; the serializer needs their bytes to re-plan and rewrite the record. Unmetered,
// like all commit work. Returns the row unchanged when nothing is unfetched (the common case);
// resolution builds a fresh copy, never mutating the shared tree's row.
function resolveForEncode(row: Row, colTypes: ColType[], paging: SharedPaging | null): Row {
  if (!row.some((v) => v.kind === "unfetched")) return row;
  if (paging === null)
    throw engineError("data_corrupted", "unfetched large value with no pager at commit");
  const fetch = (p: number): Uint8Array => paging.readBlock(p);
  return row.map((v, i) =>
    v.kind === "unfetched" ? resolveUnfetched(colTypes[i]!, v.ref, fetch) : v,
  );
}

// serializeDirty assigns a page to one dirty node (and its dirty descendants) post-order, appending
// each as a full pageSize page to `pages`, and returns this node's page index. A clean node (already
// persisted, page !== 0) short-circuits: its whole subtree is on disk unchanged (copy-on-write only
// rebuilds the modified path), so nothing is written and its existing page is returned. The node's
// set-once page id is stored here. Page indices come from the allocator (free-list first, then the
// high-water). Mirrors serializeNode for the byte layout.
function serializeDirty(
  n: PNode,
  colTypes: ColType[],
  capacity: number,
  ps: number,
  alloc: PageAlloc,
  pages: { index: number; bytes: Uint8Array }[],
  paging: SharedPaging | null,
): number {
  if (n.page !== 0) {
    return n.page;
  }
  const childPages: number[] = [];
  for (const c of n.children) {
    // A resident child recurses (dirty descendants get pages); an OnDisk child is a clean leaf already
    // durable at its page — keep it, write nothing (the incremental-commit win).
    childPages.push(
      c.node === null
        ? c.page
        : serializeDirty(c.node, colTypes, capacity, ps, alloc, pages, paging),
    );
  }
  // Encode a leaf's records, spilling over-large values to overflow pages drawn from the same
  // allocator (free-list first, then high-water — large-values.md §12). A dirty leaf may carry
  // rows the lazy load left unfetched (a sibling row's mutation dirtied them): resolve those
  // through the pager first — unmetered commit work, large-values.md §14 — so the re-encode
  // re-plans the resident row exactly as an eager writer would. An INTERIOR node is the
  // record-free keys+children skeleton (v24) — no values, no chains.
  const ovf: OverflowPageOut[] = [];
  const take = (): number => alloc.take();
  let pageType = PAGE_LEAF;
  let payload: Uint8Array;
  if (n.children.length > 0) {
    pageType = PAGE_INTERIOR;
    payload = encodeInterior(n.keys, childPages);
  } else {
    const rows = n.vals.map((v) => resolveForEncode(v, colTypes, paging));
    payload = encodeLeafPax(colTypes, n.keys, rows, capacity, take, ovf);
  }
  if (payload.length > capacity) {
    throw engineError(
      "feature_not_supported",
      "a record larger than the per-row limit is not supported",
    );
  }
  const index = alloc.take();
  n.page = index;
  pages.push({ index, bytes: makePage(ps, pageType, n.keys.length, 0, payload) });
  for (const o of ovf) {
    pages.push({
      index: o.index,
      bytes: makePage(ps, PAGE_OVERFLOW, o.itemCount, o.nextPage, o.payload),
    });
  }
  return index;
}

// loadEngine reconstructs a database from an on-disk image (inverse of toImage).
// Throws a structured data_corrupted (XX001) error for malformed input.
//
// B3 (bplus-reshape.md): the image becomes the engine's byte store — a MemoryBlockStore read
// through the SAME demand-paged loader, pager, and Packed leaf path as a file (one read path; the
// eager whole-image readTree loader is gone). The pool is PINNED (unbounded): an in-memory database
// is resident by definition (§5), so cacheBytes bounds only file-backed eviction and the observable
// default is unchanged.
export function loadEngine(image: Uint8Array): Engine {
  if (image.length < 12) {
    throw engineError("data_corrupted", "image smaller than a meta header");
  }
  const dv = new DataView(image.buffer, image.byteOffset, image.byteLength);
  const pageSize = dv.getUint32(8, false);
  if (!pageSizeValid(pageSize) || image.length < pageSize * 2) {
    throw engineError("data_corrupted", "invalid page size");
  }
  const pager = Pager.fromStore(new MemoryBlockStore(image));
  return loadEnginePaged(new SharedPaging(pager, Number.MAX_SAFE_INTEGER));
}

// newTempStorage builds a fresh per-domain storage Engine for a TEMP snapshot (temp-tables.md §6,
// bplus-reshape.md): a private in-RAM MemoryBlockStore read/written through the SAME pager + packed-leaf
// path as an in-memory database, with a PINNED (unbounded) pool — a temp domain is resident by
// definition (§5) — and within-session compaction ON, so its copy-on-write orphans are reclaimed rather
// than leaked (a temp store is never reopened, so reconstruct-on-open never runs). It seeds the store
// with the empty from-scratch image exactly as an in-memory database does (loadEngine), so its pageCount
// starts past the meta slots. Zero file writes: this byte store is entirely separate from the main
// database file. Only its storage fields (paging/pageCount/freePages/reclaimWithinSession) are used — its
// committed snapshot is unused, like the shared core's storage Engine (shared.ts).
export function newTempStorage(pageSize: number): Engine {
  const st = loadEngine(toImage(new Snapshot(0n), pageSize, 0n));
  st.reclaimWithinSession = true;
  return st;
}

// newAttachedStorage builds a fresh, empty in-RAM storage Engine for a host-attached DATABASE-scoped
// in-memory database (spec/design/attached-databases.md §6) — the same recipe as newTempStorage (a
// MemoryBlockStore seeded with the empty from-scratch image, a pinned/unbounded pool, within-session
// compaction on), differing only in that its root is DATABASE-scoped (published into the core's
// attached roots and pinned by the cross-session watermark) rather than session-private. In Slice 1b
// every attachment is in-memory; a file-backed attachment (Slice 2) would open a FileBlockStore here.
export function newAttachedStorage(pageSize: number): Engine {
  return newTempStorage(pageSize);
}

// anySpillable reports whether any column type can spill out-of-line (large-values.md §12).
// anySpillableMasked is anySpillable restricted to the columns a query's touched set selects —
// the gate for the masked scan-units walk (cost.md §3 "The touched set"): if no TOUCHED column
// can spill, the whole walk yields zero and is skipped.
export function anySpillableMasked(colTypes: ColType[], mask: boolean[]): boolean {
  return colTypes.some((ty, i) => mask[i]! && isSpillable(ty));
}

export function anySpillable(colTypes: ColType[]): boolean {
  return colTypes.some(isSpillable);
}

// collectLeafOverflow walks a table's on-disk B+tree, reading each leaf and adding the overflow chain
// pages its records reference to `reached` (large-values.md §12). Only LEAVES own chains in v24 (an
// interior node is a record-free separator skeleton). Used only for tables with spillable columns
// during the paged-open free-list reconstruction; it decodes each leaf lazily and follows its chains
// by HEADERS only (chainPages — large-values.md §14), so opening a file never materializes or
// decompresses a large value. Only variable-width (spillable) columns can own a chain, so
// fixed-width regions are skipped entirely.
function collectLeafOverflow(
  paging: SharedPaging,
  pageIdx: number,
  colTypes: ColType[],
  reached: Set<number>,
): void {
  const pg = parsePage(paging.readBlock(pageIdx));
  if (pg.pageType === PAGE_LEAF) {
    const fetch = (p: number): Uint8Array => paging.readBlock(p);
    const n = pg.itemCount;
    const dirs = parsePaxLeaf(pg.payload, n, colTypes);
    for (let c = 0; c < colTypes.length; c++) {
      if (fixedValueWidth(colTypes[c]!) !== null) continue;
      const tyref: TypeRef = { cols: colTypes, idx: c };
      for (let i = 0; i < n; i++) {
        if (paxIsNull(pg.payload, dirs, c, i)) continue;
        // The value is discarded right after markChains (only its chain pages matter), so the
        // resolution handle is deliberately dead (null paging).
        const v = readValueLazy(
          tyref,
          pg.payload,
          { pos: paxValueOff(pg.payload, dirs, c, i) },
          null,
        );
        markChains([v], fetch, reached);
      }
    }
    return;
  }
  if (pg.pageType === PAGE_INTERIOR) {
    const cur = { pos: 0 };
    const cps: number[] = [];
    for (let i = 0; i < pg.itemCount + 1; i++) cps.push(readU32(pg.payload, cur));
    for (const cp of cps) collectLeafOverflow(paging, cp, colTypes, reached);
    return;
  }
  throw engineError("data_corrupted", "expected a B-tree node page");
}

// reachablePages collects every page reachable from the committed snapshot whose catalog head is
// catRoot: the catalog chain, every table/index B+tree node, and (for spillable columns) the live
// overflow chains. It is the basis of within-session compaction (persist.ts maybeCompact / planFreeList):
// node page ids come from the in-memory tree walk (no pager reads), and only the catalog chain and
// spillable-leaf overflow are read through the pager. It does NOT cover a GiST index's on-disk R-tree
// pages (the resident GiST store holds only the leaf-key set, no on-disk page ids) nor the current
// persisted free-list pages — both are handled by the caller unioning the pages this commit just wrote
// into the reached set (a GiST index rewrites its whole R-tree every commit — gist.md §4.1(b) — so all
// live GiST pages are in that write set), so a rebuild never frees a live GiST or free-list page.
export function reachablePages(snap: Snapshot, paging: SharedPaging, catRoot: number): Set<number> {
  const reached = new Set<number>();
  // The catalog chain (rewritten to fresh pages every commit; its predecessor pages are the bulk of
  // what compaction reclaims).
  for (let p = catRoot; p !== 0; ) {
    reached.add(p);
    const pg = parsePage(paging.readBlock(p));
    if (pg.pageType !== PAGE_CATALOG)
      throw engineError("data_corrupted", "expected a catalog page");
    p = pg.nextPage;
  }
  // Table data trees + their live overflow chains.
  for (const st of snap.stores.values()) {
    const root = st.treeRoot();
    collectTreePages(root, reached);
    if (root !== null && root.page !== 0 && anySpillable(st.columnTypes())) {
      collectLeafOverflow(paging, root.page, st.columnTypes(), reached);
    }
  }
  // Secondary/unique index trees (empty-payload, never spillable).
  for (const ist of snap.indexStores.values()) {
    collectTreePages(ist.treeRoot(), reached);
  }
  return reached;
}

// collectTreePages adds every node page of a resident B+tree to reached: an interior/leaf node's own
// set-once page, and each OnDisk child leaf's page (walked without faulting it — the page id is on the
// child ref). A page-0 node is a dirty node not yet persisted (never on a committed tree at compaction
// time); it is skipped so page 0 (a meta slot) is never marked.
function collectTreePages(n: PNode | null, reached: Set<number>): void {
  if (n === null) return;
  if (n.page !== 0) reached.add(n.page);
  for (const c of n.children) {
    if (c.node === null)
      reached.add(c.page); // an OnDisk leaf: page id known without a fault
    else collectTreePages(c.node, reached);
  }
}

// readFreeList reads a persisted free-list (v25) by following the page_type 7 chain from head (meta
// offset 28) through the pager, collecting every free page index. head === 0 is an empty free-list. The
// inverse of the serialization in serializeFreeList; replaces the v24 reconstruct-on-open reachability
// walk (spec/fileformat/format.md *Reclamation*).
function readFreeList(paging: SharedPaging, head: number): number[] {
  const free: number[] = [];
  for (let p = head; p !== 0; ) {
    const pg = parsePage(paging.readBlock(p));
    if (pg.pageType !== PAGE_FREELIST)
      throw engineError("data_corrupted", "expected a free-list page");
    const cur = { pos: 0 };
    for (let i = 0; i < pg.itemCount; i++) free.push(readU32(pg.payload, cur));
    p = pg.nextPage;
  }
  return free;
}

// serializeFreeList serializes the full free-list `persist` (ascending — the pages dead at the new
// committed snapshot, including this commit's fresh orphans) into a page_type 7 chain (v25 —
// spec/fileformat/format.md *Free-list page*), and returns the chain pages, its head (0 when empty), the
// list actually persisted (persist minus the pages the chain occupies), and the new high-water.
//
// The chain's own pages are drawn from `safe` — the subset dead at the FALLBACK (prior) snapshot too
// (freeRemaining), so overwriting them this commit is torn-write-safe — NOT from the high-water, so
// persisting never grows the file. The high-water is extended only if `safe` is exhausted (rare: a large
// delete on a tight file). A free-list page is consumed here, so it never appears in the list it carries.
function serializeFreeList(
  persist: number[],
  safe: number[],
  cap: number,
  ps: number,
  next: number,
): {
  pages: { index: number; bytes: Uint8Array }[];
  head: number;
  persisted: number[];
  newNext: number;
} {
  // Nothing worth persisting when it would take the whole list to hold itself (empty, or a lone page):
  // leave the residue in RAM, reclaimed at the next compaction (a bounded transient leak).
  if (persist.length < 2) return { pages: [], head: 0, persisted: persist.slice(), newNext: next };
  const per = Math.max(1, Math.floor(cap / 4));
  // Draw free-list pages (from `safe`, then the high-water) until they hold every entry that then
  // remains. Each page drawn from `safe` also removes itself from what must be held (it is in persist),
  // so the loop converges; a high-water page adds a slot without shrinking the content.
  const flIds: number[] = [];
  let si = 0;
  let hw = next;
  let safeDrawn = 0;
  for (;;) {
    const content = persist.length - safeDrawn;
    if (Math.ceil(content / per) <= flIds.length) break;
    if (si < safe.length) {
      flIds.push(safe[si++]!);
      safeDrawn++;
    } else {
      flIds.push(hw++);
    }
  }
  const drawn = new Set(flIds.slice(0, safeDrawn));
  const persisted = persist.filter((p) => !drawn.has(p));
  const pages: { index: number; bytes: Uint8Array }[] = [];
  for (let ci = 0; ci < flIds.length; ci++) {
    const lo = Math.min(ci * per, persisted.length);
    const hi = Math.min((ci + 1) * per, persisted.length);
    const chunk = persisted.slice(lo, hi);
    const nextPage = ci + 1 < flIds.length ? flIds[ci + 1]! : 0;
    const payload = new Uint8Array(chunk.length * 4);
    const dv = new DataView(payload.buffer);
    for (let j = 0; j < chunk.length; j++) dv.setUint32(j * 4, chunk[j]!, false);
    pages.push({
      index: flIds[ci]!,
      bytes: makePage(ps, PAGE_FREELIST, chunk.length, nextPage, payload),
    });
  }
  return { pages, head: flIds[0]!, persisted, newNext: hw };
}

// planFreeList is the v25 durable-commit free-list plan, shared by the file commit paths (persist.ts
// persistImpl / commitDurableAttachment and the bare-engine file.ts persist). It runs IN-COMMIT (after
// the tree + catalog are written to the pager, before the meta), so the list it persists includes THIS
// commit's fresh orphans — without that, a short open→commit→close session would leak them forever (open
// no longer reconstructs the free-list, v25). COMPACT (periodic — high-water past ~2× the last live
// count, and no reader pins an older version): the persisted list is [2, pageCount) − reached (written
// unioned in so a wholesale-rewritten GiST R-tree is never freed); CARRY (otherwise): the persisted list
// is freeRemaining. Either way the chain pages come from freeRemaining. Returns the chain pages, head,
// the new free-list, the new high-water, and the live count to remember (unchanged when not compacting).
export function planFreeList(
  snap: Snapshot,
  paging: SharedPaging,
  catRoot: number,
  written: { index: number; bytes: Uint8Array }[],
  freeRemaining: number[],
  pageCount: number,
  liveAtCompaction: number,
  genTxid: bigint,
  ps: number,
  canReclaim: boolean,
  canReuse: boolean,
): {
  pages: { index: number; bytes: Uint8Array }[];
  head: number;
  persisted: number[];
  newPageCount: number;
  newLive: number;
  newGen: bigint;
} {
  const MIN_COMPACT_PAGES = 16; // don't churn a tiny store
  const compact = canReclaim && pageCount > MIN_COMPACT_PAGES && pageCount > 2 * liveAtCompaction;
  let persistList = freeRemaining;
  let newLive = liveAtCompaction;
  let newGen = genTxid;
  if (compact) {
    const reached = reachablePages(snap, paging, catRoot);
    for (const w of written) reached.add(w.index);
    const free: number[] = [];
    for (let p = ROOT_PAGE; p < pageCount; p++) if (!reached.has(p)) free.push(p);
    persistList = free;
    newLive = reached.size;
    newGen = snap.txid; // the recomputed list is proven dead at snap.txid (the §8 reuse gate)
  }
  // The free-list CHAIN pages overwrite in place, so they may only land on pages no live reader can
  // observe. freeRemaining is dead at the FALLBACK snapshot (torn-write-safe), but a reader pinned OLDER
  // than the free-list generation may still reference one of those pages (transactions.md §8) — the same
  // hazard as data-page reuse. When the watermark defers reuse the chain must grow the high-water instead
  // (empty `safe`), exactly as the data allocator does.
  const safe = canReuse ? freeRemaining : [];
  const s = serializeFreeList(persistList, safe, ps - PAGE_HEADER, ps, pageCount);
  return {
    pages: s.pages,
    head: s.head,
    persisted: s.persisted,
    newPageCount: s.newNext,
    newLive,
    newGen,
  };
}

// loadEnginePaged opens a file-backed database demand-paged (spec/design/pager.md, P6.4b): it loads
// only the interior B-tree skeleton resident, leaving each leaf an OnDisk page faulted through the
// bounded buffer pool on access — so the resident set is bounded by the pool, not the file size. The
// inverse of an incremental commit, reading pages through the pager instead of a whole image. (This
// slice reads every leaf page once to count its rows for length; an O(skeleton) open needs a
// per-subtree row count in the format — a deferred follow-on, pager.md §6. Memory is already bounded.)
export function loadEnginePaged(paging: SharedPaging): Engine {
  const pageSize = paging.pageSize();
  if (!pageSizeValid(pageSize)) {
    throw engineError("data_corrupted", "invalid page size");
  }

  // Select the live meta from slots 0 and 1 (highest valid txid; the lone valid slot on a torn write),
  // read as individual blocks through the pager.
  const a = parseMeta(paging.readBlock(0));
  const b = parseMeta(paging.readBlock(1));
  let mt: Meta | null = a;
  if (b && (mt === null || b.txid > mt.txid)) mt = b;
  if (mt === null) throw engineError("data_corrupted", "no valid meta page");

  const snap = new Snapshot(mt.txid);
  const statisticsExpected = new Map<string, readonly [number, number]>();
  // v25: the free-list is read from the persisted chain (below), not reconstructed by a reachability
  // walk — so the catalog + skeleton load no longer tracks a reached set.
  let catPage = mt.rootPage;
  while (catPage !== 0) {
    const pg = parsePage(paging.readBlock(catPage));
    if (pg.pageType !== PAGE_CATALOG)
      throw engineError("data_corrupted", "expected a catalog page");
    const cur = { pos: 0 };
    for (let i = 0; i < pg.itemCount; i++) {
      // Each catalog entry is kind-tagged (v9/v12): 1 = a composite-type entry (registered now; its
      // nested refs are validated after the full walk), 2 = a sequence entry (v12; self-contained,
      // registered directly — no two-pass), 0 = a table entry.
      const kind = readU8(pg.payload, cur);
      if (kind === 1) {
        snap.putType(decodeCompositeTypeEntry(pg.payload, cur));
        continue;
      }
      if (kind === 2) {
        snap.putSequence(decodeSequenceEntry(pg.payload, cur));
        continue;
      }
      if (kind === 3) {
        // A collation snapshot (v17): the baked .coll artifact + an is_default flag
        // (spec/design/collation.md §5); the default restores the per-database default.
        const { coll, isDefault } = decodeCollationEntry(pg.payload, cur);
        if (isDefault) snap.defaultCollation = coll.name;
        snap.collations.set(coll.name, coll);
        continue;
      }
      if (kind === 4) {
        decodeStatisticsEntry(pg.payload, cur, snap, statisticsExpected);
        continue;
      }
      if (kind !== 0) throw engineError("data_corrupted", "unknown catalog entry kind");
      const { table, root, rowCount, indexRoots } = decodeTableEntry(pg.payload, cur);
      const hasPK = pkIndices(table).length > 0;
      snap.putTable(table, pageSize);
      const store = snap.stores.get(table.name.toLowerCase())!;
      store.attachPaging(paging);
      // The store resolved each column's ColType from the (types-first) catalog at putTable
      // (spec/design/composite.md §3).
      const colTypes = store.columnTypes();
      if (root !== 0) {
        // Reads only the interior spine — leaves stay OnDisk; the exact row count was restored from
        // the v28 catalog entry (spec/design/storage.md §6).
        store.setSkeleton(readSkeleton(paging, root, colTypes), rowCount);
        if (!hasPK) {
          // No-PK rowid reconstruction faults the leaves to find the largest key; only for keyless
          // tables (most have a PK), and bounded by the pool. root !== 0 ⇒ the table is non-empty.
          const entries = store.entriesInKeyOrder();
          store.bumpRowidTo(decodeInt("i64", entries[entries.length - 1]!.key) + 1n);
        }
      }
      // The table's index trees (v5): zero-column demand-paged stores of entry keys
      // (spec/design/indexes.md §3); no spillable columns, so no overflow collection is
      // ever needed.
      for (let k = 0; k < table.indexes.length; k++) {
        const istore = new TableStore(pageSize - PAGE_HEADER, []);
        if (indexRoots[k]! !== 0 && table.indexes[k]!.kind === "gist") {
          // GiST is EAGER-loaded, not demand-paged (gist.md §4.1(a)): read the whole R-tree, recover
          // its leaf keys into a fully-resident leaf store.
          const out: Uint8Array[] = [];
          readGistLeafKeys(
            (p) => {
              const pg2 = parsePage(paging.readBlock(p));
              return { pageType: pg2.pageType, itemCount: pg2.itemCount, payload: pg2.payload };
            },
            indexRoots[k]!,
            out,
          );
          for (const key of out) istore.insert(key, []);
        } else {
          istore.attachPaging(paging);
          if (indexRoots[k]! !== 0) {
            istore.setSkeleton(readSkeleton(paging, indexRoots[k]!, []), null);
          }
        }
        snap.putIndexStore(table.indexes[k]!.name.toLowerCase(), istore);
      }
    }
    catPage = pg.nextPage;
  }

  for (const [groupKey, counts] of statisticsExpected) {
    const separator = groupKey.lastIndexOf("\0");
    const tableKey = groupKey.slice(0, separator);
    const column = Number(groupKey.slice(separator + 1));
    const statistics = snap.columnStatistics(tableKey, column);
    if (
      statistics === undefined ||
      statistics.mcv.length !== counts[0] ||
      statistics.histogram.length !== counts[1]
    ) {
      throw engineError("data_corrupted", "incomplete statistics entry group");
    }
  }

  // Two-pass: validate the composite-type catalog (existence + acyclicity) — XX001 on a bad
  // reference (spec/design/composite.md §3).
  snap.validateCompositeTypes();
  // Build each GiST index's resident R-tree from its eager-loaded leaf store (gist.md §4.1).
  snap.rebuildGistTrees();

  const db = new Engine();
  db.pageSize = pageSize;
  db.pageCount = mt.pageCount;
  // v25: load the free-list directly from the persisted chain (meta offset 28) — no reachability walk
  // (spec/fileformat/format.md *Reclamation*).
  db.freePages = readFreeList(paging, mt.freeListHead);
  // Every persisted free page is dead at the committed version (the free-list is "as of" mt.txid), so its
  // reuse generation is mt.txid: at open oldest_live == committed and any later reader pins ≥ the committed
  // version, so reuse is safe (transactions.md §8, the free-list generation gate).
  db.freeGenTxid = mt.txid;
  // Seed the within-session compaction trigger with the live estimate (pageCount minus the free-list),
  // so the first commit after open does not compact spuriously (planFreeList).
  const live = mt.pageCount - db.freePages.length;
  if (live > 0) db.liveAtCompaction = live;
  db.committed = snap;
  db.paging = paging;
  // Stores created in a LATER session bind this same pager at creation (Snapshot.storePaging), so
  // they join the post-commit residency flip like the loaded stores attached above.
  snap.storePaging = paging;
  return db;
}

// readSkeleton reads a table's on-disk B-tree (rooted at root) into a demand-paged skeleton: interior
// nodes resident, every leaf left OnDisk (faulted on first access). It does not compute a row count;
// the caller installs the exact v28 catalog count alongside the skeleton (spec/design/storage.md
// §6). A table whose root is itself a single leaf has no
// interior parent to hold an OnDisk reference, so the root leaf is faulted resident
// (spec/design/pager.md §1/§4).
function readSkeleton(paging: SharedPaging, root: number, colTypes: ColType[]): PNode {
  const child = readSkeletonNode(paging, root);
  if (child.node !== null) return child.node;
  return paging.faultLeaf(child.page, colTypes);
}

// readSkeletonNode resolves one B+tree node into a Child WITHOUT reading the leaf level. A leaf page
// yields an OnDisk child — its bytes are not read here at all; the parent hands down the page id and
// the leaf faults on first access. An interior page yields a resident child — the record-free
// separators + children skeleton (v24) — with its children resolved.
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
function readSkeletonNode(paging: SharedPaging, pageIdx: number): Child {
  const pg = parsePage(paging.readBlock(pageIdx));
  if (pg.pageType === PAGE_LEAF) {
    return onDiskRef(pageIdx);
  }
  if (pg.pageType === PAGE_INTERIOR) {
    const n = pg.itemCount;
    const cur = { pos: 0 };
    // Child pointers precede the separator directory (format.md "Interior node").
    const childPtrs: number[] = [];
    for (let i = 0; i < n + 1; i++) childPtrs.push(readU32(pg.payload, cur));
    // v24: the record-free routing skeleton — an end-offset separator directory + key blob.
    // Separators carry no values, so no lazy decode and no chains to mark.
    const keys = readSeparators(pg.payload, cur, n);
    // Resolve the first child to classify the level, then avoid reading leaf siblings.
    const first = readSkeletonNode(paging, childPtrs[0]!);
    const childrenAreLeaves = first.node === null;
    const children: Child[] = [first];
    for (let i = 1; i < childPtrs.length; i++) {
      children.push(
        childrenAreLeaves ? onDiskRef(childPtrs[i]!) : readSkeletonNode(paging, childPtrs[i]!),
      );
    }
    return residentRef({ keys, vals: [], weights: [], children, page: pageIdx });
  }
  throw engineError("data_corrupted", "expected a B-tree node page");
}

// readSeparators reads a v24 interior node's separator keys: the N-entry end-offset directory then
// the key blob, cur at the directory's first byte (spec/fileformat/format.md "Interior node"). Keys
// are copied out of the borrowed page slice.
function readSeparators(payload: Uint8Array, cur: Cursor, n: number): Uint8Array[] {
  const ends: number[] = [];
  for (let i = 0; i < n; i++) ends.push(readU32(payload, cur));
  const blob = cur.pos;
  const keys: Uint8Array[] = [];
  let prev = 0;
  for (const e of ends) {
    if (e < prev || blob + e > payload.length) {
      throw engineError("data_corrupted", "interior separator directory out of range");
    }
    keys.push(payload.slice(blob + prev, blob + e));
    prev = e;
  }
  cur.pos = blob + prev;
  return keys;
}

// metaPage is one meta slot's full pageSize bytes (the 36-byte header + its CRC, zero-padded): its
// only content. toImage copies it into both slots; an incremental commit pwrites it to the alternate
// slot (file.ts). Single-sources the meta byte layout (spec/fileformat/format.md). Reserved bytes are
// left zero and are covered by the CRC over [0, 32).
export function metaPage(
  pageSize: number,
  txid: bigint,
  root: number,
  pageCount: number,
  freeListHead: number,
): Uint8Array {
  const p = new Uint8Array(pageSize);
  const dv = new DataView(p.buffer);
  p[0] = 0x4a; // 'J'
  p[1] = 0x45; // 'E'
  p[2] = 0x44; // 'D'
  p[3] = 0x42; // 'B'
  dv.setUint16(4, FORMAT_VERSION, false);
  dv.setUint32(8, pageSize, false);
  dv.setBigUint64(12, txid, false);
  dv.setUint32(20, root, false);
  dv.setUint32(24, pageCount, false);
  dv.setUint32(28, freeListHead, false); // v25: the persisted free-list head (0 = empty)
  dv.setUint32(32, crc32Ieee(p.subarray(0, 32)), false);
  return p;
}

// makePage is a catalog/B-tree page's full pageSize bytes (header + payload, zero-padded). toImage
// copies it into the image; an incremental commit pwrites it directly (file.ts). Single-sources the
// page byte layout.
// Exported for the lazy-record white-box test (lazy_inline_values.test.ts); the barrel is unaffected.
export function makePage(
  ps: number,
  pageType: number,
  itemCount: number,
  nextPage: number,
  payload: Uint8Array,
): Uint8Array {
  const p = new Uint8Array(ps);
  const dv = new DataView(p.buffer);
  p[0] = pageType;
  dv.setUint32(4, itemCount, false);
  dv.setUint32(8, nextPage, false);
  p.set(payload, PAGE_HEADER);
  // The per-page checksum (v7) is computed last, over every byte but its own field at [12,16).
  dv.setUint32(12, pageCrc(p), false);
  return p;
}

// writeMeta writes a meta slot into image (the whole-image path; metaPage is the single source). A
// from-scratch image has an empty free-list, so free_list_head = 0 (v25).
function writeMeta(
  image: Uint8Array,
  ps: number,
  slot: number,
  pageSize: number,
  txid: bigint,
  root: number,
  pageCount: number,
): void {
  image.set(metaPage(pageSize, txid, root, pageCount, 0), slot * ps);
}

// writePage writes a catalog/data page into image (the whole-image path; makePage is the single source).
function writePage(
  image: Uint8Array,
  ps: number,
  index: number,
  pageType: number,
  itemCount: number,
  nextPage: number,
  payload: Uint8Array,
): void {
  image.set(makePage(ps, pageType, itemCount, nextPage, payload), index * ps);
}

// meta holds a validated meta slot's salient fields. pageCount is the on-disk page high-water — the
// next free page an incremental commit appends at (P6.1 part B). freeListHead is the persisted
// free-list head (v25 — meta offset 28): the first page_type 7 page, or 0 for an empty free-list.
type Meta = { txid: bigint; rootPage: number; pageCount: number; freeListHead: number };

// parseMeta validates a standalone meta block; null if it is not a valid meta. Shared by the
// demand-paged loader (which reads meta slots 0/1 as individual blocks — since B3 the ONLY loader;
// the whole-image readMeta/selectMeta/readPage/pageBlock readers went with the eager readTree path).
function parseMeta(block: Uint8Array): Meta | null {
  if (block.length < 36) return null;
  const dv = new DataView(block.buffer, block.byteOffset, block.byteLength);
  if (!(block[0] === 0x4a && block[1] === 0x45 && block[2] === 0x44 && block[3] === 0x42))
    return null;
  if (dv.getUint16(4, false) !== FORMAT_VERSION) return null;
  if (block[6] !== 0 || block[7] !== 0) return null;
  if (crc32Ieee(block.subarray(0, 32)) !== dv.getUint32(32, false)) return null;
  const pageCount = dv.getUint32(24, false);
  // v25: offset 28 is the free-list head — 0 (empty) or a real body page in [2, pageCount).
  const freeListHead = dv.getUint32(28, false);
  if (freeListHead !== 0 && (freeListHead < ROOT_PAGE || freeListHead >= pageCount)) return null;
  return {
    txid: dv.getBigUint64(12, false),
    rootPage: dv.getUint32(20, false),
    pageCount,
    freeListHead,
  };
}

// Page is a parsed page: header fields + a borrowed payload slice.
type Page = { pageType: number; itemCount: number; nextPage: number; payload: Uint8Array };

// parsePage parses one standalone page block (header + payload). The single-block reader the
// demand-paged loader and fault path use (a page read through the pager is exactly one block).
function parsePage(block: Uint8Array): Page {
  if (block.length < PAGE_HEADER)
    throw engineError("data_corrupted", "page shorter than its header");
  const dv = new DataView(block.buffer, block.byteOffset, block.byteLength);
  // Verify the per-page checksum (v7) before trusting any header field (format.md *Page header*).
  if (pageCrc(block) !== dv.getUint32(12, false)) {
    throw engineError("data_corrupted", "page checksum mismatch (corrupted page)");
  }
  return {
    pageType: block[0]!,
    itemCount: dv.getUint32(4, false),
    nextPage: dv.getUint32(8, false),
    payload: block.subarray(PAGE_HEADER),
  };
}

// decodeLeafNode decodes a single leaf page block into a resident node, for the demand-paging fault
// path (spec/design/pager.md §4; paging.ts faultLeaf). block is one page; page is its page id, stamped
// on the node so a later incremental commit keeps it clean. Weights are recomputed from the value
// codec... decoding is LAZY (large-values.md §14): an external/compressed value becomes an
// unfetched reference — no chain read, no decompression — resolved later only for the columns a
// query touches. Each weight is the bytes the record occupies on the page (exactly the writer's
// recordSize).
// One leaf column region's parsed shape (v24 — format.md "Leaf node"), class-dependent: a
// fixed-width region is a null bitmap + dense untagged slots; a variable region is an end-offset
// value directory + tagged codec bytes (NULL = a zero-length span).
type RegionDir =
  | { kind: "fixed"; width: number; bitmap: number; body: number }
  | { kind: "var"; ends: number; body: number };

// A parsed PAX (column-major) leaf's directories (format.md v24 "Leaf node"). All offsets index into
// the page payload; value bytes are read in place so the lazy decoder's zero-copy view still
// references the shared page block.
type PaxDirs = {
  keyBlob: number; // payload offset where the key blob starts
  keyEnd: number; // payload offset of the N-entry key end-offset directory (v24)
  regions: RegionDir[]; // K regions, in declaration order
};

// Read entry i from a validated big-endian end-offset directory retained in the immutable page.
// Direct byte assembly avoids allocating a DataView or a decoded number array on the fault path.
function paxDirEnd(payload: Uint8Array, directory: number, i: number): number {
  const p = directory + i * 4;
  return (
    ((payload[p]! << 24) | (payload[p + 1]! << 16) | (payload[p + 2]! << 8) | payload[p + 3]!) >>> 0
  );
}

// parsePaxLeaf decodes a v24 PAX leaf payload's directories (format.md "Leaf node"). Column regions
// are validated contiguous, in order, and within the payload; a malformed directory, a set region
// flags bit, or a region whose extent disagrees with its class shape is data_corrupted. The page
// body is zero-padded to the page size, so the authoritative content end is colStart[K], not
// payload.length.
function parsePaxLeaf(payload: Uint8Array, n: number, colTypes: ColType[]): PaxDirs {
  const k = colTypes.length;
  const cur = { pos: 0 };
  const keyEnd = cur.pos;
  let prev = 0;
  for (let i = 0; i < n; i++) {
    const e = readU32(payload, cur);
    if (e < prev) throw engineError("data_corrupted", "PAX leaf key directory not ascending");
    prev = e;
  }
  const keyBlob = cur.pos;
  cur.pos = keyBlob + prev;
  if (cur.pos > payload.length)
    throw engineError("data_corrupted", "PAX leaf key blob overruns page");
  const colDir = cur.pos;
  const colRegions = colDir + (k + 1) * 4;
  const colCur = { pos: colDir };
  let start = readU32(payload, colCur);
  if (start !== colRegions)
    throw engineError("data_corrupted", "PAX leaf column directory start mismatch");
  const regions: RegionDir[] = [];
  for (let c = 0; c < k; c++) {
    const end = readU32(payload, colCur);
    if (start > end || end > payload.length)
      throw engineError("data_corrupted", "PAX leaf column region out of range");
    const cc = { pos: start };
    if (readU8(payload, cc) !== 0)
      throw engineError("data_corrupted", "PAX leaf region flags has a reserved bit set");
    const width = fixedValueWidth(colTypes[c]!);
    if (width !== null) {
      const bitmap = cc.pos;
      const body = bitmap + Math.ceil(n / 8);
      if (body + n * width !== end)
        throw engineError("data_corrupted", "PAX leaf fixed region extent mismatch");
      regions.push({ kind: "fixed", width, bitmap, body });
    } else {
      const ends = cc.pos;
      let vprev = 0;
      for (let i = 0; i < n; i++) {
        const e = readU32(payload, cc);
        if (e < vprev)
          throw engineError("data_corrupted", "PAX leaf value directory not ascending");
        vprev = e;
      }
      if (cc.pos + vprev !== end)
        throw engineError("data_corrupted", "PAX leaf variable region extent mismatch");
      regions.push({ kind: "var", ends, body: cc.pos });
    }
    start = end;
  }
  return { keyBlob, keyEnd, regions };
}

// Key i's span within the payload.
function paxKey(payload: Uint8Array, d: PaxDirs, i: number): Uint8Array {
  const lo = d.keyBlob + (i === 0 ? 0 : paxDirEnd(payload, d.keyEnd, i - 1));
  const hi = d.keyBlob + paxDirEnd(payload, d.keyEnd, i);
  if (lo > hi || hi > payload.length)
    throw engineError("data_corrupted", "PAX leaf key directory out of range");
  return payload.subarray(lo, hi);
}

// paxIsNull: whether value (record i, column c) is NULL — the region bitmap (fixed-width) or the
// zero-length span (variable), with NO value decode.
function paxIsNull(payload: Uint8Array, d: PaxDirs, c: number, i: number): boolean {
  const r = d.regions[c]!;
  if (r.kind === "fixed") return (payload[r.bitmap + (i >> 3)]! & (0x80 >> (i % 8))) !== 0;
  const start = i === 0 ? 0 : paxDirEnd(payload, r.ends, i - 1);
  return paxDirEnd(payload, r.ends, i) === start;
}

// paxValueOff: the payload offset where value (record i, column c)'s bytes begin — a fixed-width
// slot (untagged body) or a variable value's tagged codec bytes. Meaningless for a NULL.
function paxValueOff(payload: Uint8Array, d: PaxDirs, c: number, i: number): number {
  const r = d.regions[c]!;
  if (r.kind === "fixed") return r.body + i * r.width;
  return r.body + (i === 0 ? 0 : paxDirEnd(payload, r.ends, i - 1));
}

// paxValueLen: the bytes value (record i, column c) contributes to recordSize — the slot width
// (fixed-width, NULL included) or the span length (variable; 0 for NULL). Derivable from the
// directories alone, with NO value decode (packed-leaf.md §3/§5).
function paxValueLen(payload: Uint8Array, d: PaxDirs, c: number, i: number): number {
  const r = d.regions[c]!;
  if (r.kind === "fixed") return r.width;
  return paxDirEnd(payload, r.ends, i) - (i === 0 ? 0 : paxDirEnd(payload, r.ends, i - 1));
}

export function decodeLeafNode(
  block: Uint8Array,
  page: number,
  colTypes: ColType[],
  paging: SharedPaging | null,
): PNode {
  const pg = parsePage(block);
  if (pg.pageType !== PAGE_LEAF)
    throw engineError("data_corrupted", "demand-paged a non-leaf page");
  const n = pg.itemCount;
  // Packed form (packed-leaf.md §5): retain the page payload (a subarray view of the block — GC keeps
  // the block alive, the equivalent of Rust's Arc<[u8]>) + validated PAX directory offsets, and decode NO
  // values. parsePaxLeaf validated + parsed the directories with no value decode, so a malformed
  // directory still surfaces data_corrupted here; a malformed value body surfaces XX001 only when the
  // column is touched (§8). Keys and weights are derived from the directories alone (§3): the weight is
  // key.length + Σ_c valueLen(c, i) (the v24 recordSize), exactly what the writer split on — so a
  // resident leaf is ≈ pageSize (§9), never an inflated row vector.
  const dirs = parsePaxLeaf(pg.payload, n, colTypes);
  const payload = pg.payload;
  // Reconstruct-on-demand seam (closes over the directories + payload). NULL comes off the region
  // (bitmap / zero span) with no decode; a fixed-width slot is the untagged inline body — decoded
  // eagerly (deferring a fixed-width scalar buys nothing, lazy-record.md §6); a variable value's
  // span is its tagged codec bytes — the lazy tag path (readValueLazy is the SAME codec the eager
  // fault ran; a spillable body becomes an inline-deferred Unfetched block view), byte-identical to
  // the eager value, moved to touch-time (§8). Each deferred value is stamped with its resolution
  // handles (a per-column TypeRef over the shared colTypes + this database's paging context), so a
  // value the static touched set missed self-resolves at the evaluator's column access — the B4
  // demand-fault backstop (bplus-reshape.md §5).
  const tyrefs: TypeRef[] = colTypes.map((_, c) => ({ cols: colTypes, idx: c }));
  const col = (i: number, c: number): Value => {
    if (paxIsNull(payload, dirs, c, i)) return nullValue();
    const ty = colTypes[c]!;
    const cur = { pos: paxValueOff(payload, dirs, c, i) };
    return fixedValueWidth(ty) !== null
      ? readInlineBody(ty, payload, cur, "construct")
      : readValueLazy(tyrefs[c]!, payload, cur, paging);
  };
  const packed: PackedLeaf = {
    n,
    key(i: number): Uint8Array {
      return paxKey(payload, dirs, i);
    },
    weight(i: number): number {
      let size = paxKey(payload, dirs, i).length;
      for (let c = 0; c < colTypes.length; c++) size += paxValueLen(payload, dirs, c, i);
      return size;
    },
    col,
    row(i: number): Row {
      const row: Row = new Array(colTypes.length);
      for (let c = 0; c < colTypes.length; c++) row[c] = col(i, c);
      return row;
    },
  };
  return { keys: [], vals: [], weights: [], children: [], packed, page };
}

type Cursor = { pos: number };

// decodeTableEntry decodes one catalog table entry: the Table (its pk list, checks, and
// index definitions included), its root_data_page, and each index's root page (parallel
// to table.indexes).
function decodeTableEntry(
  buf: Uint8Array,
  cur: Cursor,
): { table: Table; root: number; rowCount: bigint; indexRoots: number[] } {
  const name = readString(buf, cur);
  const colCount = readU16(buf, cur);
  const columns: Column[] = [];
  for (let i = 0; i < colCount; i++) {
    const cname = readString(buf, cur);
    const tc = readU8(buf, cur);
    if (tc === 14) {
      // A composite column (v9): flags, then the type name (spec/fileformat/format.md).
      // Forward-ready — composite columns are not produced this slice (composite.md §12), but a
      // reader handles the code so a later-slice file loads cleanly.
      const cflags = readU8(buf, cur);
      if ((cflags & 0b01) !== 0) {
        throw engineError("data_corrupted", "reserved column flag bit0 set");
      }
      const tname = readString(buf, cur);
      columns.push({
        name: cname,
        type: compositeT(tname),
        decimal: null,
        varcharLen: null,
        primaryKey: false,
        notNull: (cflags & 0b10) !== 0,
        default: null,
        defaultExpr: null,
        identity: null,
        collation: null,
      });
      continue;
    }
    if (tc === 15) {
      // An array column (v10): flags, then the element type descriptor (array.md §3).
      const cflags = readU8(buf, cur);
      if ((cflags & 0b01) !== 0) {
        throw engineError("data_corrupted", "reserved column flag bit0 set");
      }
      const elem = readArrayElementType(buf, cur);
      columns.push({
        name: cname,
        type: arrayT(elem),
        decimal: null,
        varcharLen: null,
        primaryKey: false,
        notNull: (cflags & 0b10) !== 0,
        default: null,
        defaultExpr: null,
        identity: null,
        collation: null,
      });
      continue;
    }
    if (tc === 17) {
      // A range column (v16): flags, then the element type descriptor — one scalar code
      // (spec/design/ranges.md §3). Ranges carry no default this slice (and never identity).
      const cflags = readU8(buf, cur);
      if ((cflags & 0b01) !== 0) {
        throw engineError("data_corrupted", "reserved column flag bit0 set");
      }
      const elem = readRangeElementType(buf, cur);
      columns.push({
        name: cname,
        type: rangeT(elem),
        decimal: null,
        varcharLen: null,
        primaryKey: false,
        notNull: (cflags & 0b10) !== 0,
        default: null,
        defaultExpr: null,
        identity: null,
        collation: null,
      });
      continue;
    }
    const ty = scalarForTypeCode(tc);
    if (ty === undefined) {
      throw engineError("data_corrupted", "unknown type code");
    }
    const flags = readU8(buf, cur);
    // bit0 was the primary_key flag through v4; v5 retired it (the pk list below is the
    // authority) and reserves it as must-be-zero. bit6 = has_collation (v17); bit7 reserved.
    if ((flags & 0b01) !== 0) {
      throw engineError("data_corrupted", "reserved column flag bit0 set");
    }
    if ((flags & 0b1000_0000) !== 0) {
      throw engineError("data_corrupted", "reserved column flag bit7 set");
    }
    // bit4 is_identity + bit5 identity_always (v15) — identity_always is meaningful only with
    // is_identity (spec/design/sequences.md §13).
    if ((flags & 0b11_0000) === 0b10_0000) {
      throw engineError("data_corrupted", "identity_always set without is_identity");
    }
    const identity: IdentityKind | null =
      (flags & 0b1_0000) !== 0 ? ((flags & 0b10_0000) !== 0 ? "always" : "byDefault") : null;
    // A decimal column carries its typmod (precision, scale); precision 0 = unconstrained.
    let decimal: DecimalTypmod | null = null;
    if (ty === "decimal") {
      const precision = readU16(buf, cur);
      const scale = readU16(buf, cur);
      if (precision !== 0) decimal = { precision, scale };
    }
    // A text column carries its varchar(n) max length (v22); 0 = unbounded (types.md §15).
    let varcharLen: number | null = null;
    if (ty === "text") {
      const n = readU32(buf, cur);
      if (n !== 0) varcharLen = n;
    }
    // The default follows the typmod (format.md): a CONSTANT default (flags bit2) is a value via
    // the same value codec rows use — never externalized, so no overflow reader is needed (a
    // 0x02 tag here would be a corrupt catalog). An EXPRESSION default (flags bit3, v8) is
    // instead the expr-text (u16 length + UTF-8), re-parsed with the ordinary expression parser
    // (XX001 if it fails, like a stored check). The two bits are mutually exclusive — both set is
    // a corrupt catalog.
    if ((flags & 0b1100) === 0b1100) {
      throw engineError("data_corrupted", "column has both a constant and an expression default");
    }
    // A constant default is a scalar value (this branch is the scalar type path).
    const colDefault =
      (flags & 0b100) !== 0 ? readValue({ kind: "scalar", scalar: ty }, buf, cur, null, []) : null;
    let colDefaultExpr: DefaultExpr | null = null;
    if ((flags & 0b1000) !== 0) {
      const exprText = readString(buf, cur);
      let expr: Expr;
      try {
        expr = parseExpression(exprText);
      } catch (e) {
        throw engineError(
          "data_corrupted",
          "stored default expression does not parse: " + String(e),
        );
      }
      colDefaultExpr = { exprText, expr };
    }
    // The effective collation (v17, flags bit6) — appended last; a non-collated column has the bit
    // clear and reads nothing (spec/design/collation.md §5).
    const collation = (flags & 0b100_0000) !== 0 ? readString(buf, cur) : null;
    columns.push({
      name: cname,
      type: scalarT(ty),
      decimal,
      varcharLen,
      primaryKey: false, // set from the pk list below
      notNull: (flags & 0b10) !== 0,
      default: colDefault,
      defaultExpr: colDefaultExpr,
      identity,
      collation,
    });
  }
  // The primary key (v5): member ordinals in KEY order. Each must name a real column,
  // once; membership sets the per-column convenience flag.
  const pkCount = readU16(buf, cur);
  const pk: number[] = [];
  for (let i = 0; i < pkCount; i++) {
    const ord = readU16(buf, cur);
    if (ord >= columns.length || pk.includes(ord)) {
      throw engineError("data_corrupted", "invalid primary key ordinal");
    }
    columns[ord]!.primaryKey = true;
    pk.push(ord);
  }
  // CHECK constraints (v4): the stored expression text re-parses with the ordinary
  // expression parser — it was written by the token renderer, so this cannot fail for a
  // file the engine wrote; failure means the file lied (XX001, constraints.md §4.5).
  const checkCount = readU16(buf, cur);
  const checks: CheckConstraint[] = [];
  for (let i = 0; i < checkCount; i++) {
    const checkName = readString(buf, cur);
    const exprText = readString(buf, cur);
    let expr: Expr;
    try {
      expr = parseExpression(exprText);
    } catch (e) {
      throw engineError(
        "data_corrupted",
        "stored check constraint does not parse: " + (e instanceof Error ? e.message : String(e)),
      );
    }
    checks.push({ name: checkName, exprText, expr });
  }
  // Secondary indexes (v5): name + key-column ordinals + the v6 flags byte (bit0
  // unique; the rest reserved-zero) + root page, in the catalog's (lowercased-name
  // ascending) order — a reader trusts the order. Duplicate ordinals within one index
  // are legal (indexes.md §1).
  const indexCount = readU16(buf, cur);
  const indexes: IndexDef[] = [];
  const indexRoots: number[] = [];
  for (let i = 0; i < indexCount; i++) {
    const iname = readString(buf, cur);
    const kc = readU16(buf, cur);
    if (kc === 0) throw engineError("data_corrupted", "index with no key columns");
    const keys: IndexKey[] = [];
    for (let j = 0; j < kc; j++) {
      const ord = readU16(buf, cur);
      if (ord === 0xffff) {
        // An expression key (v26): the sentinel, then the canonical text; re-parse it (XX001 on
        // failure, like a stored CHECK — spec/design/indexes.md §6).
        const exprText = readString(buf, cur);
        let expr: Expr;
        try {
          expr = parseExpression(exprText);
        } catch (e) {
          throw engineError(
            "data_corrupted",
            "stored index expression does not parse: " +
              (e instanceof Error ? e.message : String(e)),
          );
        }
        keys.push({ kind: "expr", exprText, expr });
      } else {
        if (ord >= columns.length) {
          throw engineError("data_corrupted", "invalid index column ordinal");
        }
        keys.push({ kind: "column", column: ord });
      }
    }
    const iflags = readU8(buf, cur);
    // bit0 unique (v6), bit1 has_predicate (v27 — a partial index, indexes.md §9); rest reserved.
    if ((iflags & ~0b11) !== 0) {
      throw engineError("data_corrupted", "reserved index flag set");
    }
    const ikind = readU8(buf, cur); // v13: index_kind byte (0 = btree, 1 = GIN); v20: 2 = GiST
    if (ikind > 2) throw engineError("data_corrupted", "unsupported index kind");
    // A GIN/GiST index is single-column plain (this slice): an expression key on either is
    // structurally impossible in a valid file (spec/design/indexes.md §6).
    if (ikind !== 0 && !keys.every((k) => k.kind === "column")) {
      throw engineError("data_corrupted", "a non-btree index cannot have an expression key");
    }
    const hasPredicate = (iflags & 0b10) !== 0;
    // A partial index is B-tree only (indexes.md §9): bit1 with a GIN/GiST kind is corrupt.
    if (hasPredicate && ikind !== 0) {
      throw engineError("data_corrupted", "a non-btree index cannot be partial");
    }
    indexRoots.push(readU32(buf, cur));
    // v27: the partial-index predicate canonical text follows index_root_page (bit1 set) — re-parse
    // it (XX001 on failure, like a stored CHECK — spec/design/indexes.md §9).
    let predicate: { exprText: string; expr: Expr } | undefined;
    if (hasPredicate) {
      const exprText = readString(buf, cur);
      let expr: Expr;
      try {
        expr = parseExpression(exprText);
      } catch (e) {
        throw engineError(
          "data_corrupted",
          "stored index predicate does not parse: " + (e instanceof Error ? e.message : String(e)),
        );
      }
      predicate = { exprText, expr };
    }
    indexes.push({
      name: iname,
      keys,
      unique: (iflags & 0b01) !== 0,
      kind: ikind === 2 ? "gist" : ikind === 1 ? "gin" : "btree",
      predicate,
    });
  }
  // Foreign keys (v11): name + local ordinals + referenced table + referenced ordinals + the
  // actions byte, in the catalog's (lowercased-name ascending) order — a reader trusts the
  // order. The local ordinals index THIS table; the referenced ordinals index the PARENT (whose
  // entry may be decoded later, so they are not cross-checked here — the writer keeps them
  // valid; a structurally impossible FK is rejected below).
  const fkCount = readU16(buf, cur);
  const fks: ForeignKey[] = [];
  for (let i = 0; i < fkCount; i++) {
    const fname = readString(buf, cur);
    const lc = readU16(buf, cur);
    if (lc === 0) throw engineError("data_corrupted", "foreign key with no columns");
    const cols: number[] = [];
    for (let j = 0; j < lc; j++) {
      const ord = readU16(buf, cur);
      if (ord >= columns.length) {
        throw engineError("data_corrupted", "invalid foreign-key column ordinal");
      }
      cols.push(ord);
    }
    const refTable = readString(buf, cur);
    const rc = readU16(buf, cur);
    if (rc !== lc) {
      throw engineError(
        "data_corrupted",
        "foreign-key referencing/referenced column count mismatch",
      );
    }
    const refCols: number[] = [];
    for (let j = 0; j < rc; j++) refCols.push(readU16(buf, cur));
    const actions = readU8(buf, cur);
    if ((actions & ~0b1111) !== 0) {
      throw engineError("data_corrupted", "reserved foreign-key action bit set");
    }
    fks.push({
      name: fname,
      columns: cols,
      refTable,
      refColumns: refCols,
      onDelete: fkActionFromCode(actions & 0b11),
      onUpdate: fkActionFromCode((actions >> 2) & 0b11),
    });
  }
  // EXCLUDE constraints (v21): name + backing GiST index name + the (column ordinal, operator)
  // element vector, in name order (spec/design/gist.md §7/§8).
  const excCount = readU16(buf, cur);
  const exclusions: ExclusionConstraint[] = [];
  for (let i = 0; i < excCount; i++) {
    const ename = readString(buf, cur);
    const iname = readString(buf, cur);
    const ec = readU16(buf, cur);
    if (ec === 0) throw engineError("data_corrupted", "exclusion constraint with no elements");
    const elements: ExclusionElement[] = [];
    for (let j = 0; j < ec; j++) {
      const ord = readU16(buf, cur);
      if (ord >= columns.length) {
        throw engineError("data_corrupted", "invalid exclusion-constraint column ordinal");
      }
      const opb = readU8(buf, cur);
      if (opb > 1)
        throw engineError("data_corrupted", "unsupported exclusion-constraint operator code");
      elements.push({ column: ord, op: opb === 1 ? "equal" : "overlaps" });
    }
    exclusions.push({ name: ename, index: iname, elements });
  }
  const root = readU32(buf, cur);
  const rowCount = readI64(buf, cur);
  if (rowCount < 0n) throw engineError("data_corrupted", "negative table row count");
  if ((root === 0) !== (rowCount === 0n)) {
    throw engineError("data_corrupted", "table root and row count disagree");
  }
  return {
    table: { name, columns, pk, checks, indexes, fks, exclusions },
    root,
    rowCount,
    indexRoots,
  };
}

// readValueLazy reads one value lazily (spec/design/large-values.md §14): inline-plain and NULL
// decode as today, but an external/compressed form becomes an unfetched reference holding exactly
// the record's pointer fields — no chain read, no decompression. The scan layer resolves the
// references for the columns a query touches (resolveUnfetched); the commit path resolves the
// rest when a dirty leaf re-encodes (resolveForEncode); a value the static touched set missed
// self-resolves from the tyref + paging handles stamped here (resolveUnfetchedSelf — the B4
// demand-fault backstop, bplus-reshape.md §5). paging may be null for a DEAD handle (the open-time
// reachability walk discards the value right after chain-marking).
function readValueLazy(
  tyref: TypeRef,
  buf: Uint8Array,
  cur: Cursor,
  paging: SharedPaging | null,
): Value {
  const ty = tyref.cols[tyref.idx]!;
  const tag = readU8(buf, cur);
  // A present inline value (lazy-record.md §12, L3): a variable-length / structured body (the
  // isSpillable set — §6) is DEFERRED as an unfetched (form 0x00) referencing the shared page block —
  // FORM (a), zero-copy (§5a): keep the span as a SUBARRAY view of the faulted page block instead of
  // copying it. A Uint8Array subarray shares (and keeps alive under GC) the page block's ArrayBuffer,
  // so the leaf's one page block stays resident and is shared by every deferred value in it (the
  // scan-emit clone is then a view copy, never a byte copy) — resident leaf memory tracks ≈ pageSize,
  // the honest buffer-pool bound (§9). The block is read fresh per fault (BlockStore.readAt) and
  // never mutated after decode (copy-on-write commits write new pages), so the view is stable. A
  // fixed-width scalar is decoded eagerly (deferring it buys nothing — §6). resolveUnfetched
  // reconstructs a touched one from the span, byte-identically (readInlineBody in construct mode).
  if (tag === 0x00) {
    if (isSpillable(ty)) {
      const body = inlineBodySpan(ty, buf, cur);
      return {
        kind: "unfetched",
        ref: {
          form: 0x00,
          firstPage: 0,
          storedLen: 0,
          rawLen: 0,
          comp: body,
          ty: tyref,
          paging: null,
        },
      };
    }
    return readInlineBody(ty, buf, cur, "construct");
  }
  if (tag === 0x01) return nullValue();
  if (tag === TAG_EXTERNAL) {
    const first = readU32(buf, cur);
    const len = readU32(buf, cur);
    return {
      kind: "unfetched",
      ref: {
        form: TAG_EXTERNAL,
        firstPage: first,
        storedLen: len,
        rawLen: 0,
        comp: undefined,
        ty: tyref,
        paging,
      },
    };
  }
  if (tag === TAG_INLINE_COMP) {
    const rawLen = readU32(buf, cur);
    const compLen = readU16(buf, cur);
    const comp = take(buf, cur, compLen).slice(); // copy out of the borrowed page slice
    return {
      kind: "unfetched",
      ref: {
        form: TAG_INLINE_COMP,
        firstPage: 0,
        storedLen: 0,
        rawLen,
        comp,
        ty: tyref,
        paging: null,
      },
    };
  }
  if (tag === TAG_EXTERNAL_COMP) {
    const first = readU32(buf, cur);
    const stored = readU32(buf, cur);
    const rawLen = readU32(buf, cur);
    return {
      kind: "unfetched",
      ref: {
        form: TAG_EXTERNAL_COMP,
        firstPage: first,
        storedLen: stored,
        rawLen,
        comp: undefined,
        ty: tyref,
        paging,
      },
    };
  }
  throw engineError("data_corrupted", "invalid value presence tag");
}

// resolveUnfetched materializes an unfetched reference into its plain Value
// (spec/design/large-values.md §14): gather the overflow chain through `fetch` for an external
// form, decompress a compressed one, and reconstruct by column type. Decompression errors are
// data_corrupted, surfaced only when the value is actually touched.
export function resolveUnfetched(
  ty: ColType,
  ref: Unfetched,
  fetch: (page: number) => Uint8Array,
): Value {
  const sink: number[] = [];
  if (ref.form === 0x00) {
    // Inline-deferred (lazy-record.md §5b, L2): the bytes are already owned — no chain read, no
    // decompression. Re-run the decoder over the captured span in construct mode.
    return readInlineBody(ty, ref.comp!, { pos: 0 }, "construct");
  }
  if (ref.form === TAG_EXTERNAL) {
    return valueFromPayload(ty, readOverflowChain(ref.firstPage, ref.storedLen, fetch, sink));
  }
  if (ref.form === TAG_INLINE_COMP) {
    return valueFromPayload(ty, lz4Decompress(ref.comp!, ref.rawLen));
  }
  if (ref.form === TAG_EXTERNAL_COMP) {
    const comp = readOverflowChain(ref.firstPage, ref.storedLen, fetch, sink);
    return valueFromPayload(ty, lz4Decompress(comp, ref.rawLen));
  }
  throw engineError("data_corrupted", "invalid unfetched value form");
}

// resolveUnfetchedSelf resolves a deferred value FROM ITS OWN CARRIED HANDLES — the B4 demand-fault
// backstop (spec/design/bplus-reshape.md §5/§6): the evaluator's column access calls this when the
// static touched set missed a value, so a prediction miss is a deterministic on-demand fetch —
// never a NULL-fold, never wrong rows. The fetch is deliberately UNMETERED (metering it would make
// cost depend on prediction quality rather than the spec'd static set — §6); the touched set stays
// the cost basis + prefetch hint. A spill-run-file reload carries sentinel handles (spill.ts — it
// rides the sort output unread by contract), so touching one stays the loud pre-B4 poison.
export function resolveUnfetchedSelf(ref: Unfetched): Value {
  if (isSentinelTypeRef(ref.ty)) {
    throw new Error("BUG: unfetched large value escaped the storage layer (spill pass-through)");
  }
  const ty = ref.ty.cols[ref.ty.idx]!;
  if (ref.form === 0x00 || ref.form === TAG_INLINE_COMP) {
    // Inline forms own their bytes — no pager involved.
    const fetch = (): Uint8Array => {
      throw new Error("an inline deferred value reads no overflow pages");
    };
    return resolveUnfetched(ty, ref, fetch);
  }
  // A deferred external value is reachable only through a snapshot whose stores hold the paging
  // context, so a dead handle here is an internal wiring bug (the reachability walk's values never
  // escape chain-marking).
  const paging = ref.paging;
  if (paging === null) {
    throw new Error("a deferred external value carries no paging handle");
  }
  return resolveUnfetched(ty, ref, (p) => paging.readBlock(p));
}

// chainPages returns the page indices of the overflow chain carrying `length` payload bytes from
// `first`, following next_page hops and reading HEADERS only — no payload assembly, no
// decompression (spec/design/large-values.md §14). The open-time reachability walk marks live
// chains with this, so opening a file never materializes its large values.
function chainPages(first: number, length: number, fetch: (page: number) => Uint8Array): number[] {
  const out: number[] = [];
  let gathered = 0;
  let p = first;
  while (gathered < length) {
    if (p === 0)
      throw engineError("data_corrupted", "overflow chain ended before the value length");
    out.push(p);
    const pg = parsePage(fetch(p));
    if (pg.pageType !== PAGE_OVERFLOW)
      throw engineError("data_corrupted", "expected an overflow page");
    const n = pg.itemCount;
    if (n === 0 || n > pg.payload.length || gathered + n > length) {
      throw engineError("data_corrupted", "overflow page slab out of range");
    }
    gathered += n;
    p = pg.nextPage;
  }
  return out;
}

// markChains adds the overflow chain pages a lazily-decoded row references to `reached` (the
// free-list reachability walk), via the header-only chainPages hop.
function markChains(row: Row, fetch: (page: number) => Uint8Array, reached: Set<number>): void {
  for (const v of row) {
    // Inline forms (0x00 inline-deferred, 0x03 inline-compressed) live in the record — no chain.
    if (v.kind !== "unfetched" || v.ref.form === 0x00 || v.ref.form === TAG_INLINE_COMP) continue;
    for (const p of chainPages(v.ref.firstPage, v.ref.storedLen, fetch)) reached.add(p);
  }
}

// readValue reads one value via the value codec (inverse of encodeValue). The presence tag is read
// first: 0x00 an inline body, 0x01 NULL, 0x02 an external pointer (u32 first_page + u32 len) whose
// payload is gathered from the overflow chain via `fetch` and reconstructed by type (large-values.md
// §12). Pages visited while following a chain are pushed to `ovfOut` for the free-list walk.
function readValue(
  ty: ColType,
  buf: Uint8Array,
  cur: Cursor,
  fetch: ((page: number) => Uint8Array) | null,
  ovfOut: number[],
): Value {
  const tag = readU8(buf, cur);
  if (tag === 0x00) return readInlineBody(ty, buf, cur, "construct");
  if (tag === 0x01) return nullValue();
  if (tag === TAG_EXTERNAL) {
    const first = readU32(buf, cur);
    const len = readU32(buf, cur);
    if (fetch === null)
      throw engineError("data_corrupted", "external value with no overflow reader");
    return valueFromPayload(ty, readOverflowChain(first, len, fetch, ovfOut));
  }
  if (tag === TAG_INLINE_COMP) {
    const rawLen = readU32(buf, cur);
    const compLen = readU16(buf, cur);
    const comp = take(buf, cur, compLen);
    return valueFromPayload(ty, lz4Decompress(comp, rawLen));
  }
  if (tag === TAG_EXTERNAL_COMP) {
    const first = readU32(buf, cur);
    const stored = readU32(buf, cur);
    const rawLen = readU32(buf, cur);
    if (fetch === null)
      throw engineError("data_corrupted", "external value with no overflow reader");
    const comp = readOverflowChain(first, stored, fetch, ovfOut);
    return valueFromPayload(ty, lz4Decompress(comp, rawLen));
  }
  throw engineError("data_corrupted", "invalid value presence tag");
}

// DecodeMode selects whether the value decoder CONSTRUCTS each leaf Value ("construct") or merely
// ADVANCES the cursor past its body ("skip") — spec/design/lazy-record.md §6. Both modes run the
// identical cursor-advancing reads (the same length / tag / count reads and the same recursion), so a
// skip walk finds every column boundary identically to a construct decode by construction: the
// zero-drift property the lazy-record reshape rests on. "skip" omits only the expensive leaf
// construction (the UTF-8 decode, the Decimal, the JsonNode / Value[] tree) and the content
// validation bundled with it; fixed-width scalars are cheap and stay eager either way (§6). A
// "skip"-mode return is an unobserved placeholder — callers use only the advanced cursor (see
// inlineBodySpan). A plain string-literal union (no enum), so it erases under type-stripping.
type DecodeMode = "construct" | "skip";

// readInlineBody reads the present-value body (after a 0x00 tag) for any ColType: a scalar via
// readInlineScalar, or a composite via readCompositeBody (spec/design/composite.md §4). mode selects
// construct vs. skip (DecodeMode).
export function readInlineBody(ty: ColType, buf: Uint8Array, cur: Cursor, mode: DecodeMode): Value {
  if (ty.kind === "composite") return readCompositeBody(ty, buf, cur, mode);
  if (ty.kind === "array") return readArrayBody(ty, buf, cur, mode);
  if (ty.kind === "range") return readRangeBody(ty.elem, buf, cur, mode);
  return readInlineScalar(ty.scalar, buf, cur, mode);
}

// inlineBodySpan walks a present inline value body in "skip" mode and returns its byte span (a
// zero-copy subarray view) WITHOUT constructing the value (spec/design/lazy-record.md §6). The caller
// has already consumed the 0x00 present tag; this advances cur past the body exactly as readInlineBody
// in "construct" mode would — the same length reads, tag dispatch, and recursion — so the returned
// span equals the bytes a construct decode consumes, by construction (the zero-drift property). L2
// will use this to defer an inline value as its compact on-disk bytes; at L1 it is the seam,
// exercised by the cross-check test.
export function inlineBodySpan(ty: ColType, buf: Uint8Array, cur: Cursor): Uint8Array {
  const start = cur.pos;
  readInlineBody(ty, buf, cur, "skip");
  return buf.subarray(start, cur.pos);
}

// readRangeBody reads a range value's present BODY (after the 0x00 tag): inverse of encodeRangeBody
// (spec/design/ranges.md §4). Reads the flags byte; an EMPTY range stops there. Otherwise the
// finite lower bound (!LB_INF) then the finite upper bound (!UB_INF) are each read as the element's
// value-codec body (no presence tag). A reserved flag bit set is XX001. An infinite bound's
// inclusivity bit is canonically 0, but the body that produced the bytes already enforced that —
// rebuild the range value faithfully from the bits present.
export function readRangeBody(
  elem: ColType,
  buf: Uint8Array,
  cur: Cursor,
  mode: DecodeMode,
): Value {
  const flags = readU8(buf, cur);
  if ((flags & ~0x1f) !== 0)
    throw engineError("data_corrupted", "range flags has a reserved bit set");
  if ((flags & 0x01) !== 0) return mode === "skip" ? nullValue() : emptyRangeValue();
  const lbInf = (flags & 0x02) !== 0;
  const ubInf = (flags & 0x04) !== 0;
  // Each present bound is advanced past in both modes (the recursion is the cursor advance).
  const lower = lbInf ? null : readInlineBody(elem, buf, cur, mode);
  const upper = ubInf ? null : readInlineBody(elem, buf, cur, mode);
  if (mode === "skip") return nullValue();
  return rangeValue(lower, upper, (flags & 0x08) !== 0, (flags & 0x10) !== 0);
}

// readArrayBody reads an array value's present BODY (after the 0x00 tag): inverse of encodeArrayBody
// (spec/design/array.md §4). Reads ndim/flags/per-dim (len, lb), then the optional null bitmap and
// the present element bodies (row-major). Accepts ndim 0 (empty) through 6 (MAXDIM); a higher ndim or
// an element-count overflow is XX001.
function readArrayBody(ty: ColType, buf: Uint8Array, cur: Cursor, mode: DecodeMode): Value {
  if (ty.kind !== "array") throw engineError("data_corrupted", "readArrayBody on a non-array type");
  const ndim = readU8(buf, cur);
  const flags = readU8(buf, cur);
  if ((flags & ~0x01) !== 0)
    throw engineError("data_corrupted", "array flags has a reserved bit set");
  if (ndim === 0) return mode === "skip" ? nullValue() : emptyArray(); // empty array
  if (ndim > 6) throw engineError("data_corrupted", "array ndim exceeds the maximum of 6");
  // dims/lbounds/bitmap are small and structural (n drives the loop, bitmap drives null handling), so
  // they are read in both modes — not the expensive leaf construction §6 skips.
  const dims: number[] = new Array(ndim);
  const lbounds: number[] = new Array(ndim);
  let n = 1;
  for (let d = 0; d < ndim; d++) {
    dims[d] = readU32(buf, cur);
    lbounds[d] = readU32(buf, cur) | 0; // lower bound (i32 two's-complement)
    n *= dims[d]!;
    if (n > 0x7fffffff) throw engineError("data_corrupted", "array element count overflow");
  }
  const hasNulls = (flags & 0x01) !== 0;
  let bitmap: Uint8Array | null = null;
  if (hasNulls) bitmap = take(buf, cur, Math.ceil(n / 8));
  const elements: Value[] | null = mode === "construct" ? new Array(n) : null;
  for (let i = 0; i < n; i++) {
    const isNull = hasNulls && (bitmap![i >> 3]! & (0x80 >> (i % 8))) !== 0;
    if (isNull) {
      if (elements) elements[i] = nullValue();
    } else {
      const v = readInlineBody(ty.elem, buf, cur, mode); // advance in both modes
      if (elements) elements[i] = v;
    }
  }
  if (!elements) return nullValue();
  return { kind: "array", dims, lbounds, elements };
}

// readCompositeBody reads a composite value's present BODY (after the 0x00 tag): the null bitmap then
// each present field's body in declaration order (inverse of encodeCompositeBody,
// spec/design/composite.md §4). A field whose bitmap bit is set is NULL and consumes no body bytes;
// otherwise its body is read recursively (no per-field presence tag).
function readCompositeBody(ty: ColType, buf: Uint8Array, cur: Cursor, mode: DecodeMode): Value {
  if (ty.kind !== "composite")
    throw engineError("data_corrupted", "readCompositeBody on a non-composite type");
  const fields = ty.fields;
  const nbytes = Math.ceil(fields.length / 8);
  const bitmap = take(buf, cur, nbytes); // structural — drives null handling
  const vals: Value[] | null = mode === "construct" ? new Array(fields.length) : null;
  for (let i = 0; i < fields.length; i++) {
    const isNull = (bitmap[i >> 3]! & (0x80 >> (i % 8))) !== 0;
    if (isNull) {
      if (vals) vals[i] = nullValue();
    } else {
      const v = readInlineBody(fields[i]!.type, buf, cur, mode); // advance in both modes
      if (vals) vals[i] = v;
    }
  }
  if (!vals) return nullValue();
  return compositeValue(vals);
}

// readInlineScalar reads the present-value body of a SCALAR (after a 0x00 tag): a fixed-width
// integer, a u16 length + UTF-8 bytes for text, a single bool-byte, the decimal body, etc.
// (format.md *Value codec*).
function readInlineScalar(ty: ScalarType, buf: Uint8Array, cur: Cursor, mode: DecodeMode): Value {
  if (isText(ty)) {
    const n = readU16(buf, cur);
    const bytes = take(buf, cur, n);
    if (mode === "skip") return nullValue(); // skip: no decode, no UTF-8 validation (lazy-record.md §6)
    try {
      return textValue(UTF8_DECODE.decode(bytes));
    } catch {
      throw engineError("data_corrupted", "non-UTF-8 text value");
    }
  }
  if (isBool(ty)) {
    // Fixed-width (1 byte) — decoded eagerly even on the lazy path (§6); the validity check is cheap
    // and harmless in either mode.
    const b = readU8(buf, cur);
    if (b === 0x00) return boolValue(false);
    if (b === 0x01) return boolValue(true);
    throw engineError("data_corrupted", "invalid boolean value byte");
  }
  if (ty === "decimal") return decodeDecimalBody(buf, cur, mode);
  if (isBytea(ty)) {
    const n = readU16(buf, cur);
    const bytes = take(buf, cur, n);
    if (mode === "skip") return nullValue(); // skip: no copy
    // .slice() copies out of the page buffer so the value owns its bytes (no UTF-8 check).
    return byteaValue(bytes.slice());
  }
  if (isJson(ty)) {
    // json: verbatim text, length-prefixed exactly like text (spec/design/json.md §4).
    const n = readU16(buf, cur);
    const bytes = take(buf, cur, n);
    if (mode === "skip") return nullValue(); // skip: no decode, no UTF-8 validation
    try {
      return jsonValue(UTF8_DECODE.decode(bytes));
    } catch {
      throw engineError("data_corrupted", "non-UTF-8 json value");
    }
  }
  if (isJsonb(ty)) {
    // jsonb: the self-delimiting tagged-node tree (spec/design/json.md §2).
    const node = decodeJsonbBody(buf, cur, mode);
    if (mode === "skip") return nullValue(); // skip: tree walked, not built
    return jsonbValue(node);
  }
  if (isUuid(ty)) {
    // Fixed 16 raw bytes, no length prefix. Must branch before the integer path —
    // decodeInt would sign-flip and widthBytes is 16 there too. .slice() copies out.
    return uuidValue(take(buf, cur, 16).slice());
  }
  if (ty === "f64") {
    // 8 IEEE bytes, big-endian; bits preserved verbatim (a stored -0/NaN round-trips). DataView
    // needs the byteOffset within the page buffer (take() does not copy).
    const b = take(buf, cur, 8);
    return float64Value(new DataView(b.buffer, b.byteOffset, b.byteLength).getFloat64(0, false));
  }
  if (ty === "f32") {
    // 4 IEEE bytes, big-endian; getFloat32 yields the exact binary32 value as a JS number, so
    // float32Value's Math.fround is a no-op (the bits already are binary32).
    const b = take(buf, cur, 4);
    return float32Value(new DataView(b.buffer, b.byteOffset, b.byteLength).getFloat32(0, false));
  }
  if (isTimestamp(ty)) return timestampValue(readIntBody(ty, buf, cur));
  if (isTimestamptz(ty)) return timestamptzValue(readIntBody(ty, buf, cur));
  // A date is a 4-byte i32 day count, same order-preserving codec as i32 (spec/design/date.md).
  if (isDate(ty)) return dateValue(readIntBody(ty, buf, cur));
  if (isInterval(ty)) {
    // Fixed 16-byte body: i32 months + i32 days + i64 micros, big-endian (no sign-flip).
    const b = take(buf, cur, 16);
    const dv = new DataView(b.buffer, b.byteOffset, b.byteLength);
    return intervalValue({
      months: dv.getInt32(0, false),
      days: dv.getInt32(4, false),
      micros: dv.getBigInt64(8, false),
    });
  }
  return intValue(readIntBody(ty, buf, cur));
}

// readIntBody decodes a fixed-width integer body in place (decodeIntAt — no subarray view per
// value; the leaf-fault hot path).
function readIntBody(ty: ScalarType, buf: Uint8Array, cur: Cursor): bigint {
  const w = widthBytes(ty);
  if (cur.pos + w > buf.length) {
    throw engineError("data_corrupted", "unexpected end of page data");
  }
  const v = decodeIntAt(ty, buf, cur.pos);
  cur.pos += w;
  return v;
}

// decodeDecimalBody decodes a decimal value's body — flags (sign), u16 scale, u16 ndigits, then that
// many base-10^4 groups (format.md). Shared by the inline path and by external reconstruction (a
// spilled decimal's chain payload is exactly this body — large-values.md §12).
function decodeDecimalBody(buf: Uint8Array, cur: Cursor, mode: DecodeMode): Value {
  const flags = readU8(buf, cur);
  const scale = readU16(buf, cur);
  const ndigits = readU16(buf, cur);
  const groups: number[] | null = mode === "construct" ? new Array(ndigits) : null;
  for (let i = 0; i < ndigits; i++) {
    const g = readU16(buf, cur); // advance in both modes
    if (groups) groups[i] = g;
  }
  if (!groups) return nullValue(); // skip-mode placeholder (no Decimal built)
  return decimalValue(Decimal.fromCodec((flags & 1) !== 0, scale, groups));
}

// readOverflowChain gathers `length` bytes of an external value's payload by following its overflow
// chain from `first` (large-values.md §12): each page is page_type 4, carries itemCount payload bytes,
// and chains via nextPage (0 terminates). Every visited page is pushed to `visited` (the free-list
// reachability walk). `fetch` returns a page's full block by index.
function readOverflowChain(
  first: number,
  length: number,
  fetch: (page: number) => Uint8Array,
  visited: number[],
): Uint8Array {
  const out = new Uint8Array(length);
  let got = 0;
  let p = first;
  while (got < length) {
    if (p === 0)
      throw engineError("data_corrupted", "overflow chain ended before the value length");
    visited.push(p);
    const pg = parsePage(fetch(p));
    if (pg.pageType !== PAGE_OVERFLOW)
      throw engineError("data_corrupted", "expected an overflow page");
    const n = pg.itemCount;
    if (n === 0 || n > pg.payload.length || got + n > length) {
      throw engineError("data_corrupted", "overflow page slab out of range");
    }
    out.set(pg.payload.subarray(0, n), got);
    got += n;
    p = pg.nextPage;
  }
  return out;
}

// --- bounds-checked big-endian readers over a payload cursor ---

function take(buf: Uint8Array, cur: Cursor, n: number): Uint8Array {
  if (cur.pos + n > buf.length) {
    throw engineError("data_corrupted", "unexpected end of page data");
  }
  const s = buf.subarray(cur.pos, cur.pos + n);
  cur.pos += n;
  return s;
}

// The scalar readers index the buffer directly — no subarray view per read. A leaf decode
// runs these hundreds of times per page, so per-read view allocations dominate fault cost.

function readU8(buf: Uint8Array, cur: Cursor): number {
  if (cur.pos + 1 > buf.length) {
    throw engineError("data_corrupted", "unexpected end of page data");
  }
  return buf[cur.pos++]!;
}

function readU16(buf: Uint8Array, cur: Cursor): number {
  if (cur.pos + 2 > buf.length) {
    throw engineError("data_corrupted", "unexpected end of page data");
  }
  const v = (buf[cur.pos]! << 8) | buf[cur.pos + 1]!;
  cur.pos += 2;
  return v;
}

function readU32(buf: Uint8Array, cur: Cursor): number {
  if (cur.pos + 4 > buf.length) {
    throw engineError("data_corrupted", "unexpected end of page data");
  }
  const p = cur.pos;
  const v = ((buf[p]! << 24) | (buf[p + 1]! << 16) | (buf[p + 2]! << 8) | buf[p + 3]!) >>> 0;
  cur.pos += 4;
  return v;
}

// readI64 reads an 8-byte big-endian two's-complement i64 (the interval-micros / sequence-field
// encoding). Big-endian via DataView.
function readI64(buf: Uint8Array, cur: Cursor): bigint {
  const b = take(buf, cur, 8);
  return new DataView(b.buffer, b.byteOffset, b.byteLength).getBigInt64(0, false);
}

function readString(buf: Uint8Array, cur: Cursor): string {
  const n = readU16(buf, cur);
  const s = take(buf, cur, n);
  try {
    return UTF8_DECODE.decode(s);
  } catch {
    throw engineError("data_corrupted", "non-UTF-8 name");
  }
}
