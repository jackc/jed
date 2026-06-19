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
// big-endian via DataView (never host order); CRC-32 hand-rolled (>>> 0 for unsigned).

import type { Expr } from "./ast.ts";
import { type CheckConstraint, type ColField, type ColType, type Column, type CompositeField, type CompositeType, type DefaultExpr, type FkAction, type ForeignKey, type IndexDef, type Table, pkIndices, resolveColType } from "./catalog.ts";
import { parseExpression } from "./parser.ts";
import { Decimal } from "./decimal.ts";
import { decodeInt, decodeIntAt, encodeNullable } from "./encoding.ts";
import { engineError } from "./errors.ts";
import { Database, Snapshot } from "./executor.ts";
import { lz4Compress, lz4Decompress } from "./lz4.ts";
import { onDiskRef, residentRef } from "./pmap.ts";
import type { Child, PNode } from "./pmap.ts";
import type { SharedPaging } from "./paging.ts";
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
  scalarT,
  typeScalar,
  widthBytes,
} from "./types.ts";
import {
  type Unfetched,
  type Value,
  boolValue,
  byteaValue,
  emptyArray,
  compositeValue,
  decimalValue,
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
} from "./value.ts";

const FORMAT_VERSION = 11; // on-disk format version (11 = FOREIGN KEY constraints: a per-table catalog foreign-key list after the index list, spec/design/constraints.md §6). 10 = array (T[]) columns: type_code 15 + an element-type descriptor in the catalog, spec/design/array.md §3, and the compact array value body, §4; 9 = composite (row) types; 8 = per-column expression-default flag; 7 = per-page crc32. The bump from 10 was atomic across Rust/Go/TS + the Ruby golden reference (every .jed golden's version byte + CRC changed together).
const PAGE_HEADER = 16; // bytes of the catalog/B-tree/overflow page header (v7: 12-byte v6 header + a 4-byte per-page crc32 at offset 12)
const INTERIOR_RESERVE = 12; // bytes reserved inside RECORD_MAX for a two-key interior node's 3 child pointers (4·3) — independent of PAGE_HEADER (format.md "Why the record cap")
const PAGE_CATALOG = 1; // page_type for a catalog page
const PAGE_LEAF = 2; // page_type for a B-tree leaf node
const PAGE_INTERIOR = 3; // page_type for a B-tree interior node
const PAGE_OVERFLOW = 4; // page_type for an out-of-line value slab (large-values.md §12)
const ROOT_PAGE = 2; // catalog root of a fresh empty db (relocatable thereafter)
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
    case "int16":
      return 1;
    case "int32":
      return 2;
    case "int64":
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
    case "float64":
      return 12;
    case "float32":
      return 13;
    case "date":
      return 16;
  }
}

// scalarForTypeCode is the inverse; undefined for an unknown code.
function scalarForTypeCode(code: number): ScalarType | undefined {
  switch (code) {
    case 1:
      return "int16";
    case 2:
      return "int32";
    case 3:
      return "int64";
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
      return "float64";
    case 13:
      return "float32";
    case 16:
      return "date";
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

// crc32Update folds data into a running CRC-32/IEEE register (reflected, poly 0xEDB88320)
// WITHOUT the final XOR, so it composes: crc32Update(crc32Update(0xFFFFFFFF, a), b) over a split
// buffer equals folding a‖b. Both crc32Ieee and the split pageCrc build on it. `>>> 0` keeps the
// running value an unsigned 32-bit number (feeding it back through `^` is bit-identical anyway).
function crc32Update(crc: number, data: Uint8Array): number {
  for (const b of data) {
    crc ^= b;
    for (let i = 0; i < 8; i++) {
      const mask = -(crc & 1); // 0xFFFFFFFF if low bit set, else 0
      crc = (crc >>> 1) ^ (0xedb88320 & mask);
    }
  }
  return crc >>> 0;
}

// crc32Ieee is CRC-32/IEEE (reflected, poly 0xEDB88320, init/final 0xFFFFFFFF) — the
// standard zlib CRC32, hand-rolled so no dependency is needed. Pinned by the vector
// crc32("123456789") === 0xCBF43926.
export function crc32Ieee(data: Uint8Array): number {
  return (crc32Update(0xffffffff, data) ^ 0xffffffff) >>> 0;
}

// pageCrc is the per-page checksum (v7, format.md *Page header*): CRC-32/IEEE over a body page's
// bytes EXCLUDING its own 4-byte crc32 field at [12,16) — i.e. [0,12) then [16,pageSize), covering
// the header, payload, and zero-fill tail. makePage writes it; parsePage/readPage re-verify it
// (mismatch → XX001). page is one full page (pageSize bytes).
function pageCrc(page: Uint8Array): number {
  const c = crc32Update(0xffffffff, page.subarray(0, 12));
  return (crc32Update(c, page.subarray(PAGE_HEADER)) ^ 0xffffffff) >>> 0;
}

// encodeValue is the value codec (format.md): a 1-byte presence tag (0x01 = NULL), then the type's
// present-value body. A scalar dispatches to encodeScalar; a COMPOSITE value (spec/design/composite.md
// §4) is the shared presence tag then a body of `null-bitmap ‖ each present field's value-codec body`
// (no per-field tag — the bitmap carries presence): see encodeCompositeBody. Recurses for nested
// composites.
function encodeValue(ty: ColType, v: Value): Uint8Array {
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
function encodeArrayBody(elem: ColType, a: { dims: number[]; lbounds: number[]; elements: Value[] }): Uint8Array {
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
    // Timestamps store their int64 microsecond instant via the same fixed-width codec as
    // int64 (spec/design/timestamp.md §6).
    return encodeNullable(ty, v.micros);
  }
  if (v.kind === "date") {
    // A date stores its int32 day count via the same fixed-width (4-byte) order-preserving codec
    // as int32 (spec/design/date.md).
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
  if (v.kind === "float64") {
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
  if (v.kind === "float32") {
    // 4 IEEE bytes, big-endian. v.value is already Math.fround'd (binary32), so setFloat32 stores it
    // without further rounding loss. NaN is canonicalized to 0x7FC00000 (see float64 above).
    const out = new Uint8Array(1 + 4);
    out[0] = 0x00; // present
    const dv = new DataView(out.buffer);
    if (Number.isNaN(v.value)) dv.setUint32(1, 0x7fc00000, false);
    else dv.setFloat32(1, v.value, false); // big-endian
    return out;
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
  const s = ty.scalar;
  return isText(s) || isBytea(s) || s === "decimal";
}

// recordMaxFor is the largest a single record may serialize to and still satisfy the B-tree split
// contract — RECORD_MAX = (C-12)/2 where C = capacity is the page payload (format.md "Why the record
// cap"). The spill planner reduces a record to ≤ this by externalizing values.
function recordMaxFor(capacity: number): number {
  return Math.max(0, Math.floor((capacity - INTERIOR_RESERVE) / 2));
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
function planDispositions(colTypes: ColType[], key: Uint8Array, row: Row, capacity: number): RecordPlan {
  const inline = colTypes.map((ty, i) => encodeValue(ty, row[i]!).length);
  const plan: RecordPlan = {
    disp: new Array<ValueDisp>(colTypes.length).fill("inline"),
    comp: new Array<Uint8Array | null>(colTypes.length).fill(null),
    size: 0,
    compressUnits: 0,
  };
  const cur = inline.slice();
  let size = 2 + key.length + inline.reduce((a, b) => a + b, 0);
  const max = recordMaxFor(capacity);
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
export function recordSize(colTypes: ColType[], key: Uint8Array, row: Row, capacity: number): number {
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
      if (v.ref.form === TAG_EXTERNAL) {
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
  if (ty.kind === "composite" && v.kind === "composite") return encodeCompositeBody(ty.fields, v.fields);
  // An array's payload is its body (the ndim/flags/dims header + bitmap + element bodies); a large
  // array spills through the same overflow + LZ4 path (spec/design/array.md §4).
  if (ty.kind === "array" && v.kind === "array") return encodeArrayBody(ty.elem, v);
  if (v.kind === "text") return UTF8.encode(v.text);
  if (v.kind === "bytea") return v.bytes;
  if (v.kind === "decimal" && ty.kind === "scalar") return encodeScalar(ty.scalar, v).subarray(1); // strip the presence tag
  throw engineError("data_corrupted", "only spillable values are externalized");
}

// valueFromPayload reconstructs a value from the P(v) content gathered from its overflow chain
// (inverse of valuePayload) — large-values.md §12.
function valueFromPayload(ty: ColType, payload: Uint8Array): Value {
  if (ty.kind === "composite") {
    // A composite's payload is its body (bitmap + present-field bodies); decode it with a fresh
    // cursor (spec/design/composite.md §4).
    return readCompositeBody(ty, payload, { pos: 0 });
  }
  if (ty.kind === "array") {
    // An array's payload is its body; decode it with a fresh cursor (spec/design/array.md §4).
    return readArrayBody(ty, payload, { pos: 0 });
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
  if (s === "decimal") return decodeDecimalBody(payload, { pos: 0 });
  throw engineError("data_corrupted", "a non-spillable type was stored external");
}

// OverflowPageOut is one overflow page produced while serializing a record's external value.
type OverflowPageOut = { index: number; itemCount: number; nextPage: number; payload: Uint8Array };

// encodeRecord builds one record (key_len(u16) | key | payload), spilling over-large values out-of-
// line per the disposition plan (large-values.md §12). For each externalized value, allocate overflow
// page(s) via `take`, append them to `ovf`, and write a tag|first_page|len pointer instead of the
// inline body. capacity is the page payload (the slab size + the spill-plan input). Shared by the
// whole-image (serializeNode) and incremental (serializeDirty) writers, which differ only in `take`.
function encodeRecord(
  colTypes: ColType[],
  key: Uint8Array,
  row: Row,
  capacity: number,
  take: () => number,
  ovf: OverflowPageOut[],
): Uint8Array {
  const plan = planDispositions(colTypes, key, row, capacity);
  const w = new ByteWriter();
  w.u16(key.length);
  w.bytes(key);
  for (let i = 0; i < colTypes.length; i++) {
    switch (plan.disp[i]) {
      case "external": {
        const payload = valuePayload(colTypes[i]!, row[i]!);
        const first = writeOverflowChain(payload, capacity, take, ovf);
        w.u8(TAG_EXTERNAL);
        w.u32(first);
        w.u32(payload.length);
        break;
      }
      case "inlineComp": {
        const rawLen = valuePayload(colTypes[i]!, row[i]!).length;
        const comp = plan.comp[i]!;
        w.u8(TAG_INLINE_COMP);
        w.u32(rawLen);
        w.u16(comp.length);
        w.bytes(comp);
        break;
      }
      case "externalComp": {
        // The chain carries the COMPRESSED block (its page count follows comp size).
        const rawLen = valuePayload(colTypes[i]!, row[i]!).length;
        const comp = plan.comp[i]!;
        const first = writeOverflowChain(comp, capacity, take, ovf);
        w.u8(TAG_EXTERNAL_COMP);
        w.u32(first);
        w.u32(comp.length);
        w.u32(rawLen);
        break;
      }
      default:
        w.bytes(encodeValue(colTypes[i]!, row[i]!));
    }
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
    ovf.push({ index: indices[j]!, itemCount: hi - lo, nextPage, payload: payload.subarray(lo, hi) });
  }
  return indices[0]!;
}

// tableEntryBytes builds one table's catalog entry (format.md). indexRoots is each
// index's tree root page, parallel to table.indexes.
function tableEntryBytes(table: Table, rootDataPage: number, indexRoots: number[]): Uint8Array {
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
    w.u8(flags);
    // A decimal column appends its typmod (precision, scale) — only for type_code 6, so
    // non-decimal entries are byte-unchanged (format.md). precision 0 = unconstrained numeric.
    if (s === "decimal") {
      w.u16(col.decimal ? col.decimal.precision : 0);
      w.u16(col.decimal ? col.decimal.scale : 0);
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
    w.u16(idx.columns.length);
    for (const c of idx.columns) w.u16(c);
    w.u8(idx.unique ? 1 : 0);
    w.u32(indexRoots[k]!);
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
  w.u32(rootDataPage);
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
    } else {
      w.u8(typeCodeForScalar(f.type.scalar));
    }
    w.u8(f.notNull ? 0b1 : 0);
    if (f.type.kind === "scalar" && f.type.scalar === "decimal") {
      w.u16(f.decimal ? f.decimal.precision : 0);
      w.u16(f.decimal ? f.decimal.scale : 0);
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
    let fty;
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
    fields.push({ name: fname, type: fty, decimal, notNull });
  }
  return { name, fields };
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
// (the writer's working snapshot at commit) or a Database (serializing its committed snapshot —
// the form callers/tests holding a Database use).
export function toImage(src: Database | Snapshot, pageSize: number, txid: bigint): Uint8Array {
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
  // Index records are the key alone — no value columns, so they encode against an empty colTypes.
  const indexColTypes: ColType[] = [];
  let nextIndex = ROOT_PAGE;
  for (let ti = 0; ti < keys.length; ti++) {
    const store = snap.stores.get(keys[ti]!)!;
    const root = store.treeRoot();
    if (root !== null) {
      const r = serializeNode(root, store.columnTypes(), capacity, nextIndex, body);
      rootDataPage[ti] = r.index;
      nextIndex = r.next;
    }
    // The table's index trees follow its data tree, in catalog (name) order
    // (spec/fileformat/format.md "From-scratch image").
    for (const idx of snap.tables.get(keys[ti]!)!.indexes) {
      let ir = 0;
      const iroot = snap.indexStore(idx.name.toLowerCase()).treeRoot();
      if (iroot !== null) {
        const r = serializeNode(iroot, indexColTypes, capacity, nextIndex, body);
        ir = r.index;
        nextIndex = r.next;
      }
      indexRoots[ti]!.push(ir);
    }
  }

  // The catalog chain follows the data; its head is the relocatable root_page. Each entry is
  // kind-tagged (v9): composite-type entries (kind 1) first in lowercased-name order, then table
  // entries (kind 0) — spec/fileformat/format.md.
  const catRoot = nextIndex;
  const catEntries: Uint8Array[] = [];
  for (const ct of snap.compositeTypesSorted()) {
    catEntries.push(concat([Uint8Array.of(1), compositeTypeEntryBytes(ct)]));
  }
  for (let ti = 0; ti < keys.length; ti++) {
    const t = snap.tables.get(keys[ti]!)!;
    catEntries.push(concat([Uint8Array.of(0), tableEntryBytes(t, rootDataPage[ti]!, indexRoots[ti]!)]));
  }
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
type BodyPage = { index: number; pageType: number; itemCount: number; nextPage: number; payload: Uint8Array };

// serializeNode serializes one node and its subtree post-order, appending each to `body`, and
// returns this node's assigned page index and the next free index. A leaf's payload is its records;
// an interior's is its N+1 child pointers (big-endian u32) then its N records (format.md). A node
// whose payload would exceed the page is an oversized record (over RECORD_MAX) → feature_not_supported.
function serializeNode(
  n: PNode,
  colTypes: ColType[],
  capacity: number,
  nextIndex: number,
  body: BodyPage[],
): { index: number; next: number } {
  const childPages: number[] = [];
  for (const c of n.children) {
    // Whole-image serialize renumbers pages from scratch and runs only on a fully-resident in-memory
    // database (create's empty image, the golden generator) — a paged file commits incrementally via
    // serializeDirty. An OnDisk child would carry a page id from a different layout, so it must not
    // appear here.
    if (c.node === null) throw engineError("data_corrupted", "whole-image serialize hit an OnDisk leaf");
    const r = serializeNode(c.node, colTypes, capacity, nextIndex, body);
    childPages.push(r.index);
    nextIndex = r.next;
  }
  const index = nextIndex;
  nextIndex++;

  const w = new ByteWriter();
  let pageType = PAGE_LEAF;
  if (n.children.length > 0) {
    pageType = PAGE_INTERIOR;
    for (const cp of childPages) w.u32(cp);
  }
  // Encode records, spilling over-large values to overflow pages allocated after this node's index
  // (post-order traversal + column order → deterministic, golden-pinnable layout).
  const ovf: OverflowPageOut[] = [];
  const take = (): number => nextIndex++;
  for (let i = 0; i < n.keys.length; i++) {
    w.bytes(encodeRecord(colTypes, n.keys[i]!, n.vals[i]!, capacity, take, ovf));
  }
  const payload = w.toBytes();
  if (payload.length > capacity) {
    throw engineError("feature_not_supported", "a record larger than the per-row limit is not supported");
  }
  body.push({ index, pageType, itemCount: n.keys.length, nextPage: 0, payload });
  for (const o of ovf) {
    body.push({ index: o.index, pageType: PAGE_OVERFLOW, itemCount: o.itemCount, nextPage: o.nextPage, payload: o.payload });
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
  // freeRemaining is the free-list entries this commit did not consume — the new free-list (P6.2).
  // file.ts stores it back on the handle for the next commit (spec/fileformat/format.md *Reclamation*).
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

  constructor(free: number[], next: number) {
    this.free = free;
    this.next = next;
  }

  take(): number {
    if (this.cursor < this.free.length) return this.free[this.cursor++]!;
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
): IncrementalWrite {
  const ps = pageSize;
  const capacity = ps - PAGE_HEADER;

  const keys = [...snap.tables.keys()].sort();

  // Allocate from the free-list first (reclaiming dead pages), then extend the file.
  const alloc = new PageAlloc(free, startPage);

  const pages: { index: number; bytes: Uint8Array }[] = [];
  const rootDataPage: number[] = new Array(keys.length).fill(0);
  const indexRoots: number[][] = keys.map(() => []);
  const indexColTypes: ColType[] = [];
  for (let ti = 0; ti < keys.length; ti++) {
    const store = snap.stores.get(keys[ti]!)!;
    const root = store.treeRoot();
    if (root !== null) {
      rootDataPage[ti] = serializeDirty(root, store.columnTypes(), capacity, ps, alloc, pages, paging);
    }
    // The table's index trees follow its data tree, in catalog (name) order — only their
    // dirty nodes are written, like any tree (spec/fileformat/format.md "Allocation &
    // incremental commit").
    for (const idx of snap.tables.get(keys[ti]!)!.indexes) {
      let ir = 0;
      const iroot = snap.indexStore(idx.name.toLowerCase()).treeRoot();
      if (iroot !== null) {
        ir = serializeDirty(iroot, indexColTypes, capacity, ps, alloc, pages, paging);
      }
      indexRoots[ti]!.push(ir);
    }
  }

  // The catalog chain is rewritten to fresh pages every commit (table roots move). Allocate its page
  // indices up front — they may be reused free pages, hence not contiguous — so each page can point at
  // the next (`pack` always returns ≥ 1 group, so catPages is non-empty). Entries are kind-tagged
  // (v9): composite-type entries (kind 1, name order) then table entries (kind 0) —
  // spec/fileformat/format.md.
  const catEntries: Uint8Array[] = [];
  for (const ct of snap.compositeTypesSorted()) {
    catEntries.push(concat([Uint8Array.of(1), compositeTypeEntryBytes(ct)]));
  }
  for (let ti = 0; ti < keys.length; ti++) {
    const t = snap.tables.get(keys[ti]!)!;
    catEntries.push(concat([Uint8Array.of(0), tableEntryBytes(t, rootDataPage[ti]!, indexRoots[ti]!)]));
  }
  const entrySizes = catEntries.map((e) => e.length);
  const catGroups = pack(entrySizes, capacity);
  const catPages = catGroups.map(() => alloc.take());
  const catRoot = catPages[0]!;
  for (let gi = 0; gi < catGroups.length; gi++) {
    const group = catGroups[gi]!;
    const nextPage = gi + 1 < catGroups.length ? catPages[gi + 1]! : 0;
    const parts = group.map((ei) => catEntries[ei]!);
    pages.push({ index: catPages[gi]!, bytes: makePage(ps, PAGE_CATALOG, group.length, nextPage, concat(parts)) });
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
  if (paging === null) throw engineError("data_corrupted", "unfetched large value with no pager at commit");
  const fetch = (p: number): Uint8Array => paging.readBlock(p);
  return row.map((v, i) => (v.kind === "unfetched" ? resolveUnfetched(colTypes[i]!, v.ref, fetch) : v));
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
    childPages.push(c.node === null ? c.page : serializeDirty(c.node, colTypes, capacity, ps, alloc, pages, paging));
  }
  const w = new ByteWriter();
  let pageType = PAGE_LEAF;
  if (n.children.length > 0) {
    pageType = PAGE_INTERIOR;
    for (const cp of childPages) w.u32(cp);
  }
  // Encode records, spilling over-large values to overflow pages drawn from the same allocator
  // (free-list first, then high-water — large-values.md §12). A dirty node may carry rows the
  // lazy load left unfetched (a sibling row's mutation dirtied them): resolve those through the
  // pager first — unmetered commit work, large-values.md §14 — so the re-encode re-plans the
  // resident row exactly as an eager writer would (chains are rewritten fresh; sharing an
  // unchanged chain is the deferred byte-layout follow-on).
  const ovf: OverflowPageOut[] = [];
  const take = (): number => alloc.take();
  for (let i = 0; i < n.keys.length; i++) {
    w.bytes(encodeRecord(colTypes, n.keys[i]!, resolveForEncode(n.vals[i]!, colTypes, paging), capacity, take, ovf));
  }
  const payload = w.toBytes();
  if (payload.length > capacity) {
    throw engineError("feature_not_supported", "a record larger than the per-row limit is not supported");
  }
  const index = alloc.take();
  n.page = index;
  pages.push({ index, bytes: makePage(ps, pageType, n.keys.length, 0, payload) });
  for (const o of ovf) {
    pages.push({ index: o.index, bytes: makePage(ps, PAGE_OVERFLOW, o.itemCount, o.nextPage, o.payload) });
  }
  return index;
}

// loadDatabase reconstructs a database from an on-disk image (inverse of toImage).
// Throws a structured data_corrupted (XX001) error for malformed input.
export function loadDatabase(image: Uint8Array): Database {
  if (image.length < 12) {
    throw engineError("data_corrupted", "image smaller than a meta header");
  }
  const dv = new DataView(image.buffer, image.byteOffset, image.byteLength);
  const pageSize = dv.getUint32(8, false);
  if (!pageSizeValid(pageSize) || image.length < pageSize * 2) {
    throw engineError("data_corrupted", "invalid page size");
  }
  const mt = selectMeta(image, dv, pageSize);

  // Build the committed snapshot from the image, then wrap it in a fresh handle that adopts the
  // file's serialization parameters (spec/design/api.md §2).
  const snap = new Snapshot(mt.txid);
  // Reconstruct the free-list (P6.2): collect every page reachable from the committed root — the
  // catalog chain plus each table's B-tree nodes — as we load it; the rest of [2, pageCount) is dead
  // space the next incremental commit may reuse (spec/fileformat/format.md *Reclamation*).
  const reached = new Set<number>();
  let catPage = mt.rootPage;
  while (catPage !== 0) {
    reached.add(catPage);
    const pg = readPage(image, dv, pageSize, catPage);
    if (pg.pageType !== PAGE_CATALOG) {
      throw engineError("data_corrupted", "expected a catalog page");
    }
    const cur = { pos: 0 };
    for (let i = 0; i < pg.itemCount; i++) {
      // Each catalog entry is kind-tagged (v9): 1 = a composite-type entry (registered now; its
      // nested refs are validated after the full walk), 0 = a table entry.
      const kind = readU8(pg.payload, cur);
      if (kind === 1) {
        snap.putType(decodeCompositeTypeEntry(pg.payload, cur));
        continue;
      }
      if (kind !== 0) throw engineError("data_corrupted", "unknown catalog entry kind");
      const { table, root, indexRoots } = decodeTableEntry(pg.payload, cur);
      const hasPK = pkIndices(table).length > 0;
      snap.putTable(table, pageSize);
      // The store resolved each column's ColType from the (types-first) catalog at putTable; the
      // codec reads it back rather than re-walking the type catalog (spec/design/composite.md §3).
      const store = snap.stores.get(table.name.toLowerCase())!;
      const colTypes = store.columnTypes();
      if (root !== 0) {
        const t = readTree(image, dv, pageSize, root, colTypes, reached);
        store.setTree(t.node, t.length);
        // No-PK keys are synthetic int64 rowids — advance the counter past the largest (the last
        // entry in key order) so future inserts don't collide.
        if (!hasPK && t.length > 0) {
          const entries = store.entriesInKeyOrder();
          store.bumpRowidTo(decodeInt("int64", entries[entries.length - 1]!.key) + 1n);
        }
      }
      // The table's index trees (v5): zero-column stores of entry keys
      // (spec/design/indexes.md §3), reachable pages included in the walk.
      for (let k = 0; k < table.indexes.length; k++) {
        const istore = new TableStore(pageSize - PAGE_HEADER, []);
        if (indexRoots[k]! !== 0) {
          const t = readTree(image, dv, pageSize, indexRoots[k]!, [], reached);
          istore.setTree(t.node, t.length);
        }
        snap.putIndexStore(table.indexes[k]!.name.toLowerCase(), istore);
      }
    }
    catPage = pg.nextPage;
  }
  // Two-pass: validate the composite-type catalog (existence + acyclicity) now that every type
  // entry has been read (spec/design/composite.md §3); a bad reference is XX001.
  snap.validateCompositeTypes();
  const db = new Database();
  db.pageSize = pageSize;
  db.pageCount = mt.pageCount; // the on-disk high-water for the next incremental commit
  // The free-list: every body page [2, pageCount) the committed root does not reach (P6.2). Ascending
  // by construction, so the allocator reuses lowest-first.
  for (let p = ROOT_PAGE; p < mt.pageCount; p++) {
    if (!reached.has(p)) db.freePages.push(p);
  }
  db.committed = snap;
  return db;
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

// collectLeafOverflow walks a table's on-disk B-tree, reading each leaf and adding the overflow chain
// pages its records reference to `reached` (large-values.md §12). Interior separators are skipped here
// — readSkeletonNode already collected their chains. Used only for tables with spillable columns during
// the paged-open free-list reconstruction; it decodes each leaf lazily and follows its chains by
// HEADERS only (chainPages — large-values.md §14), so opening a file never materializes or
// decompresses a large value.
function collectLeafOverflow(paging: SharedPaging, pageIdx: number, colTypes: ColType[], reached: Set<number>): void {
  const pg = parsePage(paging.readBlock(pageIdx));
  if (pg.pageType === PAGE_LEAF) {
    const fetch = (p: number): Uint8Array => paging.readBlock(p);
    const cur = { pos: 0 };
    for (let i = 0; i < pg.itemCount; i++) {
      const { row } = decodeRecordLazy(colTypes, pg.payload, cur);
      markChains(row, fetch, reached);
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

// loadDatabasePaged opens a file-backed database demand-paged (spec/design/pager.md, P6.4b): it loads
// only the interior B-tree skeleton resident, leaving each leaf an OnDisk page faulted through the
// bounded buffer pool on access — so the resident set is bounded by the pool, not the file size. The
// inverse of an incremental commit, reading pages through the pager instead of a whole image. (This
// slice reads every leaf page once to count its rows for length; an O(skeleton) open needs a
// per-subtree row count in the format — a deferred follow-on, pager.md §6. Memory is already bounded.)
export function loadDatabasePaged(paging: SharedPaging): Database {
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
  // Reconstruct the free-list (P6.2) from the pages the skeleton load marks reachable — every interior
  // node, plus each leaf's page id (recorded without retaining the leaf).
  const reached = new Set<number>();
  let catPage = mt.rootPage;
  while (catPage !== 0) {
    reached.add(catPage);
    const pg = parsePage(paging.readBlock(catPage));
    if (pg.pageType !== PAGE_CATALOG) throw engineError("data_corrupted", "expected a catalog page");
    const cur = { pos: 0 };
    for (let i = 0; i < pg.itemCount; i++) {
      // Each catalog entry is kind-tagged (v9): 1 = a composite-type entry (registered now; its
      // nested refs are validated after the full walk), 0 = a table entry.
      const kind = readU8(pg.payload, cur);
      if (kind === 1) {
        snap.putType(decodeCompositeTypeEntry(pg.payload, cur));
        continue;
      }
      if (kind !== 0) throw engineError("data_corrupted", "unknown catalog entry kind");
      const { table, root, indexRoots } = decodeTableEntry(pg.payload, cur);
      const hasPK = pkIndices(table).length > 0;
      snap.putTable(table, pageSize);
      const store = snap.stores.get(table.name.toLowerCase())!;
      store.attachPaging(paging);
      // The store resolved each column's ColType from the (types-first) catalog at putTable
      // (spec/design/composite.md §3).
      const colTypes = store.columnTypes();
      if (root !== 0) {
        const t = readSkeleton(paging, root, colTypes, reached);
        // The skeleton leaves leaves OnDisk (unread), so their records' overflow chains are invisible
        // to the reachability walk above. For a table with spillable columns, read the leaves now to
        // collect those live chains — else the free-list would reclaim still-referenced overflow pages
        // (large-values.md §12; default open is this paged path). Dead chains still leak until the
        // next open, matching the P6.2 orphan model.
        if (anySpillable(colTypes)) collectLeafOverflow(paging, root, colTypes, reached);
        store.setTree(t.node, t.length);
        if (!hasPK && t.length > 0) {
          // No-PK rowid reconstruction faults the leaves to find the largest key; only for keyless
          // tables (most have a PK), and bounded by the pool.
          const entries = store.entriesInKeyOrder();
          store.bumpRowidTo(decodeInt("int64", entries[entries.length - 1]!.key) + 1n);
        }
      }
      // The table's index trees (v5): zero-column demand-paged stores of entry keys
      // (spec/design/indexes.md §3); no spillable columns, so no overflow collection is
      // ever needed.
      for (let k = 0; k < table.indexes.length; k++) {
        const istore = new TableStore(pageSize - PAGE_HEADER, []);
        istore.attachPaging(paging);
        if (indexRoots[k]! !== 0) {
          const t = readSkeleton(paging, indexRoots[k]!, [], reached);
          istore.setTree(t.node, t.length);
        }
        snap.putIndexStore(table.indexes[k]!.name.toLowerCase(), istore);
      }
    }
    catPage = pg.nextPage;
  }

  // Two-pass: validate the composite-type catalog (existence + acyclicity) — XX001 on a bad
  // reference (spec/design/composite.md §3).
  snap.validateCompositeTypes();

  const db = new Database();
  db.pageSize = pageSize;
  db.pageCount = mt.pageCount;
  for (let p = ROOT_PAGE; p < mt.pageCount; p++) {
    if (!reached.has(p)) db.freePages.push(p);
  }
  db.committed = snap;
  db.paging = paging;
  return db;
}

// readSkeleton reads a table's on-disk B-tree (rooted at root) into a demand-paged skeleton: interior
// nodes resident, each leaf left OnDisk. Returns the root node and the total row count. A table whose
// root is itself a single leaf has no interior parent to hold an OnDisk reference, so the root leaf is
// faulted resident (spec/design/pager.md §1/§4).
function readSkeleton(
  paging: SharedPaging,
  root: number,
  colTypes: ColType[],
  reached: Set<number>,
): { node: PNode; length: number } {
  const r = readSkeletonNode(paging, root, colTypes, reached);
  if (r.child.node !== null) return { node: r.child.node, length: r.length };
  return { node: paging.faultLeaf(r.child.page, colTypes), length: r.length };
}

// readSkeletonNode reads one B-tree node through the pager, once: a leaf becomes an OnDisk child (its
// rows counted from the header, then dropped — not retained); an interior node becomes a resident child
// with its children resolved recursively. Returns the child reference and the subtree's row count.
function readSkeletonNode(
  paging: SharedPaging,
  pageIdx: number,
  colTypes: ColType[],
  reached: Set<number>,
): { child: Child; length: number } {
  reached.add(pageIdx);
  const pg = parsePage(paging.readBlock(pageIdx));
  if (pg.pageType === PAGE_LEAF) {
    return { child: onDiskRef(pageIdx), length: pg.itemCount };
  }
  if (pg.pageType === PAGE_INTERIOR) {
    const n = pg.itemCount;
    const cur = { pos: 0 };
    const children: Child[] = [];
    let total = 0;
    for (let i = 0; i < n + 1; i++) {
      const cp = readU32(pg.payload, cur);
      const r = readSkeletonNode(paging, cp, colTypes, reached);
      children.push(r.child);
      total += r.length;
    }
    const keys: Uint8Array[] = [];
    const vals: Row[] = [];
    const weights: number[] = [];
    // Separators decode lazily like leaves (large-values.md §14): an external value stays an
    // unfetched reference; its chain is marked reachable by headers only.
    const fetch = (p: number): Uint8Array => paging.readBlock(p);
    for (let i = 0; i < n; i++) {
      const { key, row, weight } = decodeRecordLazy(colTypes, pg.payload, cur);
      weights.push(weight);
      markChains(row, fetch, reached);
      keys.push(key);
      vals.push(row);
    }
    total += n;
    return { child: residentRef({ keys, vals, weights, children, page: pageIdx }), length: total };
  }
  throw engineError("data_corrupted", "expected a B-tree node page");
}

// readTree reads a table's on-disk B-tree (rooted at pageIdx) into an in-memory tree, returning the
// root node and the total row count (spec/fileformat/format.md). An interior node's payload is its
// N+1 child pointers then its N records; we recurse the pointers, then read the separators. Weights
// are recomputed from the value codec (the exact size the writer used), so the loaded tree is ready
// for further size-driven splits.
function readTree(
  image: Uint8Array,
  dv: DataView,
  ps: number,
  pageIdx: number,
  colTypes: ColType[],
  reached: Set<number>,
): { node: PNode; length: number } {
  reached.add(pageIdx);
  const capacity = ps - PAGE_HEADER;
  const pg = readPage(image, dv, ps, pageIdx);
  const fetch = (p: number): Uint8Array => pageBlock(image, ps, p);
  if (pg.pageType === PAGE_LEAF) {
    const n = pg.itemCount;
    const keys: Uint8Array[] = [];
    const vals: Row[] = [];
    const weights: number[] = [];
    const cur = { pos: 0 };
    for (let i = 0; i < n; i++) {
      const { key, row, ovf } = decodeRecord(colTypes, pg.payload, cur, fetch);
      weights.push(recordSize(colTypes, key, row, capacity));
      for (const p of ovf) reached.add(p);
      keys.push(key);
      vals.push(row);
    }
    return { node: { keys, vals, weights, children: [], page: pageIdx }, length: n };
  }
  if (pg.pageType === PAGE_INTERIOR) {
    const n = pg.itemCount;
    const cur = { pos: 0 };
    const children: Child[] = [];
    let total = 0;
    for (let i = 0; i < n + 1; i++) {
      const cp = readU32(pg.payload, cur);
      const r = readTree(image, dv, ps, cp, colTypes, reached);
      // The in-memory load is fully resident (no pager to fault from); the demand-paged file load
      // (loadDatabasePaged) is a separate path that leaves leaf children OnDisk.
      children.push(residentRef(r.node));
      total += r.length;
    }
    const keys: Uint8Array[] = [];
    const vals: Row[] = [];
    const weights: number[] = [];
    for (let i = 0; i < n; i++) {
      const { key, row, ovf } = decodeRecord(colTypes, pg.payload, cur, fetch);
      weights.push(recordSize(colTypes, key, row, capacity));
      for (const p of ovf) reached.add(p);
      keys.push(key);
      vals.push(row);
    }
    total += n;
    return { node: { keys, vals, weights, children, page: pageIdx }, length: total };
  }
  throw engineError("data_corrupted", "expected a B-tree node page");
}

// metaPage is one meta slot's full pageSize bytes (the 36-byte header + its CRC, zero-padded): its
// only content. toImage copies it into both slots; an incremental commit pwrites it to the alternate
// slot (file.ts). Single-sources the meta byte layout (spec/fileformat/format.md). Reserved bytes are
// left zero and are covered by the CRC over [0, 32).
export function metaPage(pageSize: number, txid: bigint, root: number, pageCount: number): Uint8Array {
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
  dv.setUint32(32, crc32Ieee(p.subarray(0, 32)), false);
  return p;
}

// makePage is a catalog/B-tree page's full pageSize bytes (header + payload, zero-padded). toImage
// copies it into the image; an incremental commit pwrites it directly (file.ts). Single-sources the
// page byte layout.
function makePage(
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

// writeMeta writes a meta slot into image (the whole-image path; metaPage is the single source).
function writeMeta(
  image: Uint8Array,
  ps: number,
  slot: number,
  pageSize: number,
  txid: bigint,
  root: number,
  pageCount: number,
): void {
  image.set(metaPage(pageSize, txid, root, pageCount), slot * ps);
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
// next free page an incremental commit appends at (P6.1 part B).
type Meta = { txid: bigint; rootPage: number; pageCount: number };

// readMeta validates one meta slot; null if it is not a valid meta.
function readMeta(image: Uint8Array, dv: DataView, ps: number, slot: number): Meta | null {
  const off = slot * ps;
  if (off + ps > image.length) return null;
  if (!(image[off] === 0x4a && image[off + 1] === 0x45 && image[off + 2] === 0x44 && image[off + 3] === 0x42)) {
    return null;
  }
  if (dv.getUint16(off + 4, false) !== FORMAT_VERSION) return null;
  if (
    image[off + 6] !== 0 ||
    image[off + 7] !== 0 ||
    image[off + 28] !== 0 ||
    image[off + 29] !== 0 ||
    image[off + 30] !== 0 ||
    image[off + 31] !== 0
  ) {
    return null;
  }
  if (crc32Ieee(image.subarray(off, off + 32)) !== dv.getUint32(off + 32, false)) return null;
  return {
    txid: dv.getBigUint64(off + 12, false),
    rootPage: dv.getUint32(off + 20, false),
    pageCount: dv.getUint32(off + 24, false),
  };
}

// parseMeta validates a standalone meta block; null if it is not a valid meta. Shared by the
// demand-paged loader (which reads meta slots 0/1 as individual blocks).
function parseMeta(block: Uint8Array): Meta | null {
  if (block.length < 36) return null;
  const dv = new DataView(block.buffer, block.byteOffset, block.byteLength);
  if (!(block[0] === 0x4a && block[1] === 0x45 && block[2] === 0x44 && block[3] === 0x42)) return null;
  if (dv.getUint16(4, false) !== FORMAT_VERSION) return null;
  if (
    block[6] !== 0 ||
    block[7] !== 0 ||
    block[28] !== 0 ||
    block[29] !== 0 ||
    block[30] !== 0 ||
    block[31] !== 0
  ) {
    return null;
  }
  if (crc32Ieee(block.subarray(0, 32)) !== dv.getUint32(32, false)) return null;
  return {
    txid: dv.getBigUint64(12, false),
    rootPage: dv.getUint32(20, false),
    pageCount: dv.getUint32(24, false),
  };
}

// selectMeta picks the valid slot with the highest txid (tie → slot 0); the lone valid
// slot on a torn write; error if neither is valid (format.md).
function selectMeta(image: Uint8Array, dv: DataView, ps: number): Meta {
  const a = readMeta(image, dv, ps, 0);
  const b = readMeta(image, dv, ps, 1);
  if (a && b) return b.txid > a.txid ? b : a;
  if (a) return a;
  if (b) return b;
  throw engineError("data_corrupted", "no valid meta page");
}

// Page is a parsed page: header fields + a borrowed payload slice.
type Page = { pageType: number; itemCount: number; nextPage: number; payload: Uint8Array };

function readPage(image: Uint8Array, dv: DataView, ps: number, index: number): Page {
  const off = index * ps;
  if (off + ps > image.length) {
    throw engineError("data_corrupted", "page index out of range");
  }
  // Verify the per-page checksum (v7) before trusting any header field (format.md *Page header*).
  if (pageCrc(image.subarray(off, off + ps)) !== dv.getUint32(off + 12, false)) {
    throw engineError("data_corrupted", "page checksum mismatch (corrupted page)");
  }
  return {
    pageType: image[off]!,
    itemCount: dv.getUint32(off + 4, false),
    nextPage: dv.getUint32(off + 8, false),
    payload: image.subarray(off + PAGE_HEADER, off + ps),
  };
}

// pageBlock returns one page's full block, copied out of a whole image — the overflow-chain fetch for
// the in-memory load path (readTree, large-values.md §12).
function pageBlock(image: Uint8Array, ps: number, index: number): Uint8Array {
  const off = index * ps;
  if (off + ps > image.length) throw engineError("data_corrupted", "page index out of range");
  return image.slice(off, off + ps);
}

// parsePage parses one standalone page block (header + payload). The single-block reader the
// demand-paged loader and fault path use (a page read through the pager is exactly one block);
// readPage slices it out of a whole image.
function parsePage(block: Uint8Array): Page {
  if (block.length < PAGE_HEADER) throw engineError("data_corrupted", "page shorter than its header");
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
export function decodeLeafNode(block: Uint8Array, page: number, colTypes: ColType[]): PNode {
  const pg = parsePage(block);
  if (pg.pageType !== PAGE_LEAF) throw engineError("data_corrupted", "demand-paged a non-leaf page");
  const keys: Uint8Array[] = [];
  const vals: Row[] = [];
  const weights: number[] = [];
  const cur = { pos: 0 };
  for (let i = 0; i < pg.itemCount; i++) {
    const { key, row, weight } = decodeRecordLazy(colTypes, pg.payload, cur);
    weights.push(weight);
    keys.push(key);
    vals.push(row);
  }
  return { keys, vals, weights, children: [], page };
}

type Cursor = { pos: number };

// decodeTableEntry decodes one catalog table entry: the Table (its pk list, checks, and
// index definitions included), its root_data_page, and each index's root page (parallel
// to table.indexes).
function decodeTableEntry(
  buf: Uint8Array,
  cur: Cursor,
): { table: Table; root: number; indexRoots: number[] } {
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
        primaryKey: false,
        notNull: (cflags & 0b10) !== 0,
        default: null,
        defaultExpr: null,
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
        primaryKey: false,
        notNull: (cflags & 0b10) !== 0,
        default: null,
        defaultExpr: null,
      });
      continue;
    }
    const ty = scalarForTypeCode(tc);
    if (ty === undefined) {
      throw engineError("data_corrupted", "unknown type code");
    }
    const flags = readU8(buf, cur);
    // bit0 was the primary_key flag through v4; v5 retired it (the pk list below is the
    // authority) and reserves it as must-be-zero.
    if ((flags & 0b01) !== 0) {
      throw engineError("data_corrupted", "reserved column flag bit0 set");
    }
    // A decimal column carries its typmod (precision, scale); precision 0 = unconstrained.
    let decimal: DecimalTypmod | null = null;
    if (ty === "decimal") {
      const precision = readU16(buf, cur);
      const scale = readU16(buf, cur);
      if (precision !== 0) decimal = { precision, scale };
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
    const colDefault = (flags & 0b100) !== 0 ? readValue({ kind: "scalar", scalar: ty }, buf, cur, null, []) : null;
    let colDefaultExpr: DefaultExpr | null = null;
    if ((flags & 0b1000) !== 0) {
      const exprText = readString(buf, cur);
      let expr: Expr;
      try {
        expr = parseExpression(exprText);
      } catch (e) {
        throw engineError("data_corrupted", "stored default expression does not parse: " + String(e));
      }
      colDefaultExpr = { exprText, expr };
    }
    columns.push({
      name: cname,
      type: scalarT(ty),
      decimal,
      primaryKey: false, // set from the pk list below
      notNull: (flags & 0b10) !== 0,
      default: colDefault,
      defaultExpr: colDefaultExpr,
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
    const cols: number[] = [];
    for (let j = 0; j < kc; j++) {
      const ord = readU16(buf, cur);
      if (ord >= columns.length) {
        throw engineError("data_corrupted", "invalid index column ordinal");
      }
      cols.push(ord);
    }
    const iflags = readU8(buf, cur);
    if ((iflags & ~0b01) !== 0) {
      throw engineError("data_corrupted", "reserved index flag set");
    }
    indexRoots.push(readU32(buf, cur));
    indexes.push({ name: iname, columns: cols, unique: (iflags & 0b01) !== 0 });
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
      throw engineError("data_corrupted", "foreign-key referencing/referenced column count mismatch");
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
  const root = readU32(buf, cur);
  return { table: { name, columns, pk, checks, indexes, fks }, root, indexRoots };
}

// readValueLazy reads one value lazily (spec/design/large-values.md §14): inline-plain and NULL
// decode as today, but an external/compressed form becomes an unfetched reference holding exactly
// the record's pointer fields — no chain read, no decompression. The scan layer resolves the
// references for the columns a query touches (resolveUnfetched); the commit path resolves the
// rest when a dirty leaf re-encodes (resolveForEncode).
function readValueLazy(ty: ColType, buf: Uint8Array, cur: Cursor): Value {
  const tag = readU8(buf, cur);
  // A composite's inline body has no nested overflow pointers (its fields are inline —
  // composite.md §4), so it is read eagerly even in the lazy path.
  if (tag === 0x00) return readInlineBody(ty, buf, cur);
  if (tag === 0x01) return nullValue();
  if (tag === TAG_EXTERNAL) {
    const first = readU32(buf, cur);
    const len = readU32(buf, cur);
    return { kind: "unfetched", ref: { form: TAG_EXTERNAL, firstPage: first, storedLen: len, rawLen: 0, comp: undefined } };
  }
  if (tag === TAG_INLINE_COMP) {
    const rawLen = readU32(buf, cur);
    const compLen = readU16(buf, cur);
    const comp = take(buf, cur, compLen).slice(); // copy out of the borrowed page slice
    return { kind: "unfetched", ref: { form: TAG_INLINE_COMP, firstPage: 0, storedLen: 0, rawLen, comp } };
  }
  if (tag === TAG_EXTERNAL_COMP) {
    const first = readU32(buf, cur);
    const stored = readU32(buf, cur);
    const rawLen = readU32(buf, cur);
    return { kind: "unfetched", ref: { form: TAG_EXTERNAL_COMP, firstPage: first, storedLen: stored, rawLen, comp: undefined } };
  }
  throw engineError("data_corrupted", "invalid value presence tag");
}

// decodeRecordLazy decodes one record (readValueLazy per column) and returns {key, row, weight},
// where the weight is the bytes the record occupies on the page — exactly the recordSize the
// writer split on, read off the cursor instead of re-planned (a re-plan would need the unfetched
// bytes).
function decodeRecordLazy(
  colTypes: ColType[],
  buf: Uint8Array,
  cur: Cursor,
): { key: Uint8Array; row: Row; weight: number } {
  const start = cur.pos;
  const keyLen = readU16(buf, cur);
  const key = take(buf, cur, keyLen).slice(); // copy out of the borrowed page slice
  const row: Row = new Array(colTypes.length);
  for (let i = 0; i < colTypes.length; i++) {
    row[i] = readValueLazy(colTypes[i]!, buf, cur);
  }
  return { key, row, weight: cur.pos - start };
}

// resolveUnfetched materializes an unfetched reference into its plain Value
// (spec/design/large-values.md §14): gather the overflow chain through `fetch` for an external
// form, decompress a compressed one, and reconstruct by column type. Decompression errors are
// data_corrupted, surfaced only when the value is actually touched.
export function resolveUnfetched(ty: ColType, ref: Unfetched, fetch: (page: number) => Uint8Array): Value {
  const sink: number[] = [];
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

// chainPages returns the page indices of the overflow chain carrying `length` payload bytes from
// `first`, following next_page hops and reading HEADERS only — no payload assembly, no
// decompression (spec/design/large-values.md §14). The open-time reachability walk marks live
// chains with this, so opening a file never materializes its large values.
function chainPages(first: number, length: number, fetch: (page: number) => Uint8Array): number[] {
  const out: number[] = [];
  let gathered = 0;
  let p = first;
  while (gathered < length) {
    if (p === 0) throw engineError("data_corrupted", "overflow chain ended before the value length");
    out.push(p);
    const pg = parsePage(fetch(p));
    if (pg.pageType !== PAGE_OVERFLOW) throw engineError("data_corrupted", "expected an overflow page");
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
    if (v.kind !== "unfetched" || v.ref.form === TAG_INLINE_COMP) continue;
    for (const p of chainPages(v.ref.firstPage, v.ref.storedLen, fetch)) reached.add(p);
  }
}

// decodeRecord decodes one record {key, row} and the overflow chain pages any external value followed
// (for the free-list reachability walk — large-values.md §12). `fetch` reads a page block by index,
// used to follow overflow chains; null is only valid where no value can be external (a default).
function decodeRecord(
  colTypes: ColType[],
  buf: Uint8Array,
  cur: Cursor,
  fetch: ((page: number) => Uint8Array) | null,
): { key: Uint8Array; row: Row; ovf: number[] } {
  const keyLen = readU16(buf, cur);
  const key = take(buf, cur, keyLen).slice(); // copy out of the borrowed page slice
  const row: Row = new Array(colTypes.length);
  const ovf: number[] = [];
  for (let i = 0; i < colTypes.length; i++) {
    row[i] = readValue(colTypes[i]!, buf, cur, fetch, ovf);
  }
  return { key, row, ovf };
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
  if (tag === 0x00) return readInlineBody(ty, buf, cur);
  if (tag === 0x01) return nullValue();
  if (tag === TAG_EXTERNAL) {
    const first = readU32(buf, cur);
    const len = readU32(buf, cur);
    if (fetch === null) throw engineError("data_corrupted", "external value with no overflow reader");
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
    if (fetch === null) throw engineError("data_corrupted", "external value with no overflow reader");
    const comp = readOverflowChain(first, stored, fetch, ovfOut);
    return valueFromPayload(ty, lz4Decompress(comp, rawLen));
  }
  throw engineError("data_corrupted", "invalid value presence tag");
}

// readInlineBody reads the present-value body (after a 0x00 tag) for any ColType: a scalar via
// readInlineScalar, or a composite via readCompositeBody (spec/design/composite.md §4).
function readInlineBody(ty: ColType, buf: Uint8Array, cur: Cursor): Value {
  if (ty.kind === "composite") return readCompositeBody(ty, buf, cur);
  if (ty.kind === "array") return readArrayBody(ty, buf, cur);
  return readInlineScalar(ty.scalar, buf, cur);
}

// readArrayBody reads an array value's present BODY (after the 0x00 tag): inverse of encodeArrayBody
// (spec/design/array.md §4). Reads ndim/flags/per-dim (len, lb), then the optional null bitmap and
// the present element bodies (row-major). Accepts ndim 0 (empty) through 6 (MAXDIM); a higher ndim or
// an element-count overflow is XX001.
function readArrayBody(ty: ColType, buf: Uint8Array, cur: Cursor): Value {
  if (ty.kind !== "array") throw engineError("data_corrupted", "readArrayBody on a non-array type");
  const ndim = readU8(buf, cur);
  const flags = readU8(buf, cur);
  if ((flags & ~0x01) !== 0) throw engineError("data_corrupted", "array flags has a reserved bit set");
  if (ndim === 0) return emptyArray(); // empty array
  if (ndim > 6) throw engineError("data_corrupted", "array ndim exceeds the maximum of 6");
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
  const elements: Value[] = new Array(n);
  for (let i = 0; i < n; i++) {
    const isNull = hasNulls && (bitmap![i >> 3]! & (0x80 >> (i % 8))) !== 0;
    elements[i] = isNull ? nullValue() : readInlineBody(ty.elem, buf, cur);
  }
  return { kind: "array", dims, lbounds, elements };
}

// readCompositeBody reads a composite value's present BODY (after the 0x00 tag): the null bitmap then
// each present field's body in declaration order (inverse of encodeCompositeBody,
// spec/design/composite.md §4). A field whose bitmap bit is set is NULL and consumes no body bytes;
// otherwise its body is read recursively (no per-field presence tag).
function readCompositeBody(ty: ColType, buf: Uint8Array, cur: Cursor): Value {
  if (ty.kind !== "composite") throw engineError("data_corrupted", "readCompositeBody on a non-composite type");
  const fields = ty.fields;
  const nbytes = Math.ceil(fields.length / 8);
  const bitmap = take(buf, cur, nbytes);
  const vals: Value[] = new Array(fields.length);
  for (let i = 0; i < fields.length; i++) {
    const isNull = (bitmap[i >> 3]! & (0x80 >> (i % 8))) !== 0;
    vals[i] = isNull ? nullValue() : readInlineBody(fields[i]!.type, buf, cur);
  }
  return compositeValue(vals);
}

// readInlineScalar reads the present-value body of a SCALAR (after a 0x00 tag): a fixed-width
// integer, a u16 length + UTF-8 bytes for text, a single bool-byte, the decimal body, etc.
// (format.md *Value codec*).
function readInlineScalar(ty: ScalarType, buf: Uint8Array, cur: Cursor): Value {
  if (isText(ty)) {
    const n = readU16(buf, cur);
    const bytes = take(buf, cur, n);
    try {
      return textValue(UTF8_DECODE.decode(bytes));
    } catch {
      throw engineError("data_corrupted", "non-UTF-8 text value");
    }
  }
  if (isBool(ty)) {
    const b = readU8(buf, cur);
    if (b === 0x00) return boolValue(false);
    if (b === 0x01) return boolValue(true);
    throw engineError("data_corrupted", "invalid boolean value byte");
  }
  if (ty === "decimal") return decodeDecimalBody(buf, cur);
  if (isBytea(ty)) {
    const n = readU16(buf, cur);
    // .slice() copies out of the page buffer so the value owns its bytes (no UTF-8 check).
    return byteaValue(take(buf, cur, n).slice());
  }
  if (isUuid(ty)) {
    // Fixed 16 raw bytes, no length prefix. Must branch before the integer path —
    // decodeInt would sign-flip and widthBytes is 16 there too. .slice() copies out.
    return uuidValue(take(buf, cur, 16).slice());
  }
  if (ty === "float64") {
    // 8 IEEE bytes, big-endian; bits preserved verbatim (a stored -0/NaN round-trips). DataView
    // needs the byteOffset within the page buffer (take() does not copy).
    const b = take(buf, cur, 8);
    return float64Value(new DataView(b.buffer, b.byteOffset, b.byteLength).getFloat64(0, false));
  }
  if (ty === "float32") {
    // 4 IEEE bytes, big-endian; getFloat32 yields the exact binary32 value as a JS number, so
    // float32Value's Math.fround is a no-op (the bits already are binary32).
    const b = take(buf, cur, 4);
    return float32Value(new DataView(b.buffer, b.byteOffset, b.byteLength).getFloat32(0, false));
  }
  if (isTimestamp(ty)) return timestampValue(readIntBody(ty, buf, cur));
  if (isTimestamptz(ty)) return timestamptzValue(readIntBody(ty, buf, cur));
  // A date is a 4-byte int32 day count, same order-preserving codec as int32 (spec/design/date.md).
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
function decodeDecimalBody(buf: Uint8Array, cur: Cursor): Value {
  const flags = readU8(buf, cur);
  const scale = readU16(buf, cur);
  const ndigits = readU16(buf, cur);
  const groups: number[] = new Array(ndigits);
  for (let i = 0; i < ndigits; i++) groups[i] = readU16(buf, cur);
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
    if (p === 0) throw engineError("data_corrupted", "overflow chain ended before the value length");
    visited.push(p);
    const pg = parsePage(fetch(p));
    if (pg.pageType !== PAGE_OVERFLOW) throw engineError("data_corrupted", "expected an overflow page");
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

function readString(buf: Uint8Array, cur: Cursor): string {
  const n = readU16(buf, cur);
  const s = take(buf, cur, n);
  try {
    return UTF8_DECODE.decode(s);
  } catch {
    throw engineError("data_corrupted", "non-UTF-8 name");
  }
}
