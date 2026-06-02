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

import { type Column, type Table, primaryKeyIndex } from "./catalog.ts";
import { decodeInt, encodeNullable } from "./encoding.ts";
import { engineError } from "./errors.ts";
import { Database } from "./executor.ts";
import type { Row } from "./storage.ts";
import { type ScalarType, widthBytes } from "./types.ts";
import { type Value, intValue, nullValue } from "./value.ts";

const FORMAT_VERSION = 1; // on-disk format version
const PAGE_HEADER = 12; // bytes of the catalog/data page header
const PAGE_CATALOG = 1; // page_type for a catalog page
const PAGE_DATA = 2; // page_type for a data page
const ROOT_PAGE = 2; // catalog root (pages 0,1 are the meta slots)

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
    default:
      return undefined;
  }
}

// crc32Ieee is CRC-32/IEEE (reflected, poly 0xEDB88320, init/final 0xFFFFFFFF) — the
// standard zlib CRC32, hand-rolled so no dependency is needed. Pinned by the vector
// crc32("123456789") === 0xCBF43926. `>>> 0` keeps the result an unsigned 32-bit value.
export function crc32Ieee(data: Uint8Array): number {
  let crc = 0xffffffff;
  for (const b of data) {
    crc ^= b;
    for (let i = 0; i < 8; i++) {
      const mask = -(crc & 1); // 0xFFFFFFFF if low bit set, else 0
      crc = (crc >>> 1) ^ (0xedb88320 & mask);
    }
  }
  return (crc ^ 0xffffffff) >>> 0;
}

// encodeValue is the value codec (format.md): a presence tag plus, when present, the
// integer bytes for the column type. Reuses the key encoding behind a named seam so
// storage and key encodings can diverge when non-integer types land.
function encodeValue(ty: ScalarType, v: Value): Uint8Array {
  // boolean is expression-only this slice; no column is boolean, so a stored value is
  // only ever NULL or an integer (spec/design/types.md §1).
  if (v.kind === "null") return encodeNullable(ty, null);
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

// encodeRecord builds one record: key_len(u16) | key | payload(each column value).
function encodeRecord(table: Table, key: Uint8Array, row: Row): Uint8Array {
  const w = new ByteWriter();
  w.u16(key.length);
  w.bytes(key);
  for (let i = 0; i < table.columns.length; i++) {
    w.bytes(encodeValue(table.columns[i]!.type, row[i]!));
  }
  return w.toBytes();
}

// tableEntryBytes builds one table's catalog entry (format.md).
function tableEntryBytes(table: Table, rootDataPage: number): Uint8Array {
  const w = new ByteWriter();
  const nameB = UTF8.encode(table.name);
  w.u16(nameB.length);
  w.bytes(nameB);
  w.u16(table.columns.length);
  for (const col of table.columns) {
    const cn = UTF8.encode(col.name);
    w.u16(cn.length);
    w.bytes(cn);
    w.u8(typeCodeForScalar(col.type));
    let flags = 0;
    if (col.primaryKey) flags |= 0b01;
    if (col.notNull) flags |= 0b10;
    w.u8(flags);
  }
  w.u32(rootDataPage);
  return w.toBytes();
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

// toImage serializes the whole database to one on-disk image (format.md). pageSize is
// recorded in the meta page; txid is written into both meta slots.
export function toImage(db: Database, pageSize: number, txid: bigint): Uint8Array {
  const ps = pageSize;
  if (ps < PAGE_HEADER + 36) {
    throw engineError("feature_not_supported", "page size too small for the format");
  }
  const capacity = ps - PAGE_HEADER;

  // Tables in ascending lowercased-name order (no map-iteration order leak).
  const keys = [...db.tables.keys()].sort();

  // Per-table record bytes, in key order.
  const records: Uint8Array[][] = keys.map((k) =>
    db.stores.get(k)!.entriesInKeyOrder().map((e) => encodeRecord(db.tables.get(k)!, e.key, e.row)),
  );

  // Catalog grouping depends only on entry sizes (independent of root_data_page).
  const entrySizes = keys.map((k) => tableEntryBytes(db.tables.get(k)!, 0).length);
  const catGroups = pack(entrySizes, capacity);
  const numCatPages = catGroups.length;

  // Assign data chains after the catalog; record each table's root page.
  let nextIndex = ROOT_PAGE + numCatPages;
  const rootDataPage: number[] = new Array(keys.length).fill(0);
  const dataGroups: number[][][] = new Array(keys.length);
  for (let ti = 0; ti < keys.length; ti++) {
    const recs = records[ti]!;
    if (recs.length === 0) {
      dataGroups[ti] = [];
      continue;
    }
    const groups = pack(recs.map((r) => r.length), capacity);
    rootDataPage[ti] = nextIndex;
    nextIndex += groups.length;
    dataGroups[ti] = groups;
  }
  const pageCount = nextIndex;

  const image = new Uint8Array(pageCount * ps);
  const dv = new DataView(image.buffer);

  // Meta: both slots hold the current meta (a fresh whole-image commit has no distinct
  // prior version — format.md).
  writeMeta(image, dv, ps, 0, pageSize, txid, ROOT_PAGE, pageCount);
  writeMeta(image, dv, ps, 1, pageSize, txid, ROOT_PAGE, pageCount);

  // Catalog pages.
  for (let gi = 0; gi < catGroups.length; gi++) {
    const group = catGroups[gi]!;
    const index = ROOT_PAGE + gi;
    const next = gi + 1 < catGroups.length ? index + 1 : 0;
    const parts = group.map((ti) => tableEntryBytes(db.tables.get(keys[ti]!)!, rootDataPage[ti]!));
    writePage(image, dv, ps, index, PAGE_CATALOG, group.length, next, concat(parts));
  }

  // Data pages, one chain per non-empty table.
  for (let ti = 0; ti < keys.length; ti++) {
    const groups = dataGroups[ti]!;
    for (let gi = 0; gi < groups.length; gi++) {
      const group = groups[gi]!;
      const index = rootDataPage[ti]! + gi;
      const next = gi + 1 < groups.length ? index + 1 : 0;
      const parts = group.map((ri) => records[ti]![ri]!);
      writePage(image, dv, ps, index, PAGE_DATA, group.length, next, concat(parts));
    }
  }

  return image;
}

// loadDatabase reconstructs a database from an on-disk image (inverse of toImage).
// Throws a structured data_corrupted (XX001) error for malformed input.
export function loadDatabase(image: Uint8Array): Database {
  if (image.length < 12) {
    throw engineError("data_corrupted", "image smaller than a meta header");
  }
  const dv = new DataView(image.buffer, image.byteOffset, image.byteLength);
  const pageSize = dv.getUint32(8, false);
  if (pageSize < PAGE_HEADER + 36 || image.length < pageSize * 2) {
    throw engineError("data_corrupted", "invalid page size");
  }
  const mt = selectMeta(image, dv, pageSize);

  const db = new Database();
  let catPage = mt.rootPage;
  while (catPage !== 0) {
    const pg = readPage(image, dv, pageSize, catPage);
    if (pg.pageType !== PAGE_CATALOG) {
      throw engineError("data_corrupted", "expected a catalog page");
    }
    const cur = { pos: 0 };
    for (let i = 0; i < pg.itemCount; i++) {
      const { table, root } = decodeTableEntry(pg.payload, cur);
      const colTypes = table.columns.map((c) => c.type);
      const hasPK = primaryKeyIndex(table) >= 0;
      db.putTable(table);
      readDataChain(image, dv, pageSize, root, colTypes, hasPK, table.name, db);
    }
    catPage = pg.nextPage;
  }
  return db;
}

// readDataChain reads every record in a table's data-page chain into its store. For a
// table with no primary key, the keys are synthetic int64 rowids; advance the store's
// rowid counter past the largest so future inserts don't collide (format.md). No format
// change — keys are stored verbatim.
function readDataChain(
  image: Uint8Array,
  dv: DataView,
  ps: number,
  root: number,
  colTypes: ScalarType[],
  hasPK: boolean,
  name: string,
  db: Database,
): void {
  const store = db.stores.get(name.toLowerCase())!;
  let dp = root;
  while (dp !== 0) {
    const pg = readPage(image, dv, ps, dp);
    if (pg.pageType !== PAGE_DATA) {
      throw engineError("data_corrupted", "expected a data page");
    }
    const cur = { pos: 0 };
    for (let i = 0; i < pg.itemCount; i++) {
      const { key, row } = decodeRecord(colTypes, pg.payload, cur);
      if (!hasPK && key.length === widthBytes("int64")) {
        store.bumpRowidTo(decodeInt("int64", key) + 1n);
      }
      if (!store.insert(key, row)) {
        throw engineError("data_corrupted", "duplicate key in data page");
      }
    }
    dp = pg.nextPage;
  }
}

// writeMeta writes a meta slot's bytes (and its CRC) into image. Reserved bytes are left
// zero (the image is zero-initialized) and are covered by the CRC over [off, off+32).
function writeMeta(
  image: Uint8Array,
  dv: DataView,
  ps: number,
  slot: number,
  pageSize: number,
  txid: bigint,
  root: number,
  pageCount: number,
): void {
  const off = slot * ps;
  image[off] = 0x41; // 'A'
  image[off + 1] = 0x42; // 'B'
  image[off + 2] = 0x44; // 'D'
  image[off + 3] = 0x42; // 'B'
  dv.setUint16(off + 4, FORMAT_VERSION, false);
  dv.setUint32(off + 8, pageSize, false);
  dv.setBigUint64(off + 12, txid, false);
  dv.setUint32(off + 20, root, false);
  dv.setUint32(off + 24, pageCount, false);
  const crc = crc32Ieee(image.subarray(off, off + 32));
  dv.setUint32(off + 32, crc, false);
}

// writePage writes a catalog/data page's header and payload into image.
function writePage(
  image: Uint8Array,
  dv: DataView,
  ps: number,
  index: number,
  pageType: number,
  itemCount: number,
  nextPage: number,
  payload: Uint8Array,
): void {
  const off = index * ps;
  image[off] = pageType;
  dv.setUint32(off + 4, itemCount, false);
  dv.setUint32(off + 8, nextPage, false);
  image.set(payload, off + PAGE_HEADER);
}

// meta holds a validated meta slot's salient fields.
type Meta = { txid: bigint; rootPage: number };

// readMeta validates one meta slot; null if it is not a valid meta.
function readMeta(image: Uint8Array, dv: DataView, ps: number, slot: number): Meta | null {
  const off = slot * ps;
  if (off + ps > image.length) return null;
  if (!(image[off] === 0x41 && image[off + 1] === 0x42 && image[off + 2] === 0x44 && image[off + 3] === 0x42)) {
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
  return { txid: dv.getBigUint64(off + 12, false), rootPage: dv.getUint32(off + 20, false) };
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
  return {
    pageType: image[off]!,
    itemCount: dv.getUint32(off + 4, false),
    nextPage: dv.getUint32(off + 8, false),
    payload: image.subarray(off + PAGE_HEADER, off + ps),
  };
}

type Cursor = { pos: number };

function decodeTableEntry(buf: Uint8Array, cur: Cursor): { table: Table; root: number } {
  const name = readString(buf, cur);
  const colCount = readU16(buf, cur);
  const columns: Column[] = [];
  for (let i = 0; i < colCount; i++) {
    const cname = readString(buf, cur);
    const tc = readU8(buf, cur);
    const ty = scalarForTypeCode(tc);
    if (ty === undefined) {
      throw engineError("data_corrupted", "unknown type code");
    }
    const flags = readU8(buf, cur);
    columns.push({
      name: cname,
      type: ty,
      primaryKey: (flags & 0b01) !== 0,
      notNull: (flags & 0b10) !== 0,
    });
  }
  const root = readU32(buf, cur);
  return { table: { name, columns }, root };
}

function decodeRecord(colTypes: ScalarType[], buf: Uint8Array, cur: Cursor): { key: Uint8Array; row: Row } {
  const keyLen = readU16(buf, cur);
  const key = take(buf, cur, keyLen).slice(); // copy out of the borrowed page slice
  const row: Row = new Array(colTypes.length);
  for (let i = 0; i < colTypes.length; i++) {
    row[i] = readValue(colTypes[i]!, buf, cur);
  }
  return { key, row };
}

// readValue reads one value via the value codec (inverse of encodeValue).
function readValue(ty: ScalarType, buf: Uint8Array, cur: Cursor): Value {
  const tag = readU8(buf, cur);
  if (tag === 0x00) return nullValue();
  if (tag === 0x01) return intValue(decodeInt(ty, take(buf, cur, widthBytes(ty))));
  throw engineError("data_corrupted", "invalid value presence tag");
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

function readU8(buf: Uint8Array, cur: Cursor): number {
  return take(buf, cur, 1)[0]!;
}

function readU16(buf: Uint8Array, cur: Cursor): number {
  const s = take(buf, cur, 2);
  return (s[0]! << 8) | s[1]!;
}

function readU32(buf: Uint8Array, cur: Cursor): number {
  const s = take(buf, cur, 4);
  return ((s[0]! << 24) | (s[1]! << 16) | (s[2]! << 8) | s[3]!) >>> 0;
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
