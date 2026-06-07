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
import { Decimal } from "./decimal.ts";
import { decodeInt, encodeNullable } from "./encoding.ts";
import { engineError } from "./errors.ts";
import { Database, Snapshot } from "./executor.ts";
import type { PNode } from "./pmap.ts";
import type { Row } from "./storage.ts";
import {
  type DecimalTypmod,
  type ScalarType,
  isBool,
  isBytea,
  isText,
  isTimestamp,
  isTimestamptz,
  isUuid,
  widthBytes,
} from "./types.ts";
import {
  type Value,
  boolValue,
  byteaValue,
  decimalValue,
  intValue,
  nullValue,
  textValue,
  timestampValue,
  timestamptzValue,
  uuidValue,
} from "./value.ts";

const FORMAT_VERSION = 2; // on-disk format version (2 = page-backed CoW B-tree, P6.1)
const PAGE_HEADER = 12; // bytes of the catalog/B-tree page header
const PAGE_CATALOG = 1; // page_type for a catalog page
const PAGE_LEAF = 2; // page_type for a B-tree leaf node
const PAGE_INTERIOR = 3; // page_type for a B-tree interior node
const ROOT_PAGE = 2; // catalog root of a fresh empty db (relocatable thereafter)

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

// encodeValue is the value codec (format.md): a 1-byte presence tag (0x01 = NULL), then the
// type's present-value body. Integers reuse the order-preserving key encoding; text is
// where the seam diverges — a stored text value needs no ordering, so it is a compact u16
// byte-length + UTF-8 bytes (collation C, verbatim). A text value whose UTF-8 length exceeds
// 0xFFFF is unsupported; in practice it also exceeds a page and is caught by the
// oversized-item rule in packing (0A000), so the u16 write is sound for every supported page
// size (spec/fileformat/format.md). boolean is a single bool-byte body — 0x00 false, 0x01
// true (types.md §9).
function encodeValue(ty: ScalarType, v: Value): Uint8Array {
  if (v.kind === "null") return encodeNullable(ty, null);
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

// recordSize is the on-disk size of a record (key_len(u16) | key | each column value) — the weight
// the page-backed B-tree splits on (format.md). It must equal encodeRecord(...).length, so in-memory
// node boundaries match serialized page boundaries; computed from the value codec to stay in lockstep.
export function recordSize(colTypes: ScalarType[], key: Uint8Array, row: Row): number {
  let n = 2 + key.length;
  for (let i = 0; i < colTypes.length; i++) {
    n += encodeValue(colTypes[i]!, row[i]!).length;
  }
  return n;
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
    if (col.default !== null) flags |= 0b100;
    w.u8(flags);
    // A decimal column appends its typmod (precision, scale) — only for type_code 6, so
    // non-decimal entries are byte-unchanged (format.md). precision 0 = unconstrained numeric.
    if (col.type === "decimal") {
      w.u16(col.decimal ? col.decimal.precision : 0);
      w.u16(col.decimal ? col.decimal.scale : 0);
    }
    // A column with a DEFAULT (flags bit2) appends its pre-evaluated default value via the same
    // value codec rows use — AFTER the typmod, presence-gated, so a column without a default is
    // byte-unchanged (format.md). A DEFAULT NULL is one 0x01.
    if (col.default !== null) {
      w.bytes(encodeValue(col.type, col.default));
    }
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

// toImage serializes a snapshot's whole state to one on-disk image (format.md). pageSize is
// recorded in the meta page; txid is written into both meta slots. Accepts a Snapshot directly
// (the writer's working snapshot at commit) or a Database (serializing its committed snapshot —
// the form callers/tests holding a Database use).
export function toImage(src: Database | Snapshot, pageSize: number, txid: bigint): Uint8Array {
  const snap = src instanceof Snapshot ? src : src.committed;
  const ps = pageSize;
  if (ps < PAGE_HEADER + 36) {
    throw engineError("feature_not_supported", "page size too small for the format");
  }
  const capacity = ps - PAGE_HEADER;

  // Tables in ascending lowercased-name order (no map-iteration order leak).
  const keys = [...snap.tables.keys()].sort();

  // Serialize each table's B-tree post-order, body pages allocated from page 2. Each BodyPage is
  // (index, pageType, itemCount, payload); children precede their parent so parent child-pointers
  // reference already-allocated pages (format.md).
  const body: BodyPage[] = [];
  const rootDataPage: number[] = new Array(keys.length).fill(0);
  let nextIndex = ROOT_PAGE;
  for (let ti = 0; ti < keys.length; ti++) {
    const root = snap.stores.get(keys[ti]!)!.treeRoot();
    if (root !== null) {
      const r = serializeNode(root, snap.tables.get(keys[ti]!)!, capacity, nextIndex, body);
      rootDataPage[ti] = r.index;
      nextIndex = r.next;
    }
  }

  // The catalog chain follows the data; its head is the relocatable root_page.
  const catRoot = nextIndex;
  const entrySizes = keys.map((k) => tableEntryBytes(snap.tables.get(k)!, 0).length);
  const catGroups = pack(entrySizes, capacity);
  const pageCount = catRoot + catGroups.length;

  const image = new Uint8Array(pageCount * ps);

  // Meta: both slots hold the current meta (a fresh from-scratch image has no distinct prior
  // version; slot alternation is the live incremental-commit path — format.md).
  writeMeta(image, ps, 0, pageSize, txid, catRoot, pageCount);
  writeMeta(image, ps, 1, pageSize, txid, catRoot, pageCount);

  // B-tree node pages.
  for (const bp of body) {
    writePage(image, ps, bp.index, bp.pageType, bp.itemCount, 0, bp.payload);
  }

  // Catalog chain.
  for (let gi = 0; gi < catGroups.length; gi++) {
    const group = catGroups[gi]!;
    const index = catRoot + gi;
    const next = gi + 1 < catGroups.length ? index + 1 : 0;
    const parts = group.map((ti) => tableEntryBytes(snap.tables.get(keys[ti]!)!, rootDataPage[ti]!));
    writePage(image, ps, index, PAGE_CATALOG, group.length, next, concat(parts));
  }

  return image;
}

// BodyPage is one serialized B-tree node awaiting write: its index, type, key count, payload.
type BodyPage = { index: number; pageType: number; itemCount: number; payload: Uint8Array };

// serializeNode serializes one node and its subtree post-order, appending each to `body`, and
// returns this node's assigned page index and the next free index. A leaf's payload is its records;
// an interior's is its N+1 child pointers (big-endian u32) then its N records (format.md). A node
// whose payload would exceed the page is an oversized record (over RECORD_MAX) → feature_not_supported.
function serializeNode(
  n: PNode,
  table: Table,
  capacity: number,
  nextIndex: number,
  body: BodyPage[],
): { index: number; next: number } {
  const childPages: number[] = [];
  for (const c of n.children) {
    const r = serializeNode(c, table, capacity, nextIndex, body);
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
  for (let i = 0; i < n.keys.length; i++) {
    w.bytes(encodeRecord(table, n.keys[i]!, n.vals[i]!));
  }
  const payload = w.toBytes();
  if (payload.length > capacity) {
    throw engineError("feature_not_supported", "a record larger than the per-row limit is not supported");
  }
  body.push({ index, pageType, itemCount: n.keys.length, payload });
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
): IncrementalWrite {
  const ps = pageSize;
  const capacity = ps - PAGE_HEADER;

  const keys = [...snap.tables.keys()].sort();

  // Allocate from the free-list first (reclaiming dead pages), then extend the file.
  const alloc = new PageAlloc(free, startPage);

  const pages: { index: number; bytes: Uint8Array }[] = [];
  const rootDataPage: number[] = new Array(keys.length).fill(0);
  for (let ti = 0; ti < keys.length; ti++) {
    const root = snap.stores.get(keys[ti]!)!.treeRoot();
    if (root !== null) {
      rootDataPage[ti] = serializeDirty(root, snap.tables.get(keys[ti]!)!, capacity, ps, alloc, pages);
    }
  }

  // The catalog chain is rewritten to fresh pages every commit (table roots move). Allocate its page
  // indices up front — they may be reused free pages, hence not contiguous — so each page can point at
  // the next (`pack` always returns ≥ 1 group, so catPages is non-empty).
  const entrySizes = keys.map((k) => tableEntryBytes(snap.tables.get(k)!, 0).length);
  const catGroups = pack(entrySizes, capacity);
  const catPages = catGroups.map(() => alloc.take());
  const catRoot = catPages[0]!;
  for (let gi = 0; gi < catGroups.length; gi++) {
    const group = catGroups[gi]!;
    const nextPage = gi + 1 < catGroups.length ? catPages[gi + 1]! : 0;
    const parts = group.map((ti) => tableEntryBytes(snap.tables.get(keys[ti]!)!, rootDataPage[ti]!));
    pages.push({ index: catPages[gi]!, bytes: makePage(ps, PAGE_CATALOG, group.length, nextPage, concat(parts)) });
  }

  return { pages, rootPage: catRoot, pageCount: alloc.next, freeRemaining: alloc.remaining() };
}

// serializeDirty assigns a page to one dirty node (and its dirty descendants) post-order, appending
// each as a full pageSize page to `pages`, and returns this node's page index. A clean node (already
// persisted, page !== 0) short-circuits: its whole subtree is on disk unchanged (copy-on-write only
// rebuilds the modified path), so nothing is written and its existing page is returned. The node's
// set-once page id is stored here. Page indices come from the allocator (free-list first, then the
// high-water). Mirrors serializeNode for the byte layout.
function serializeDirty(
  n: PNode,
  table: Table,
  capacity: number,
  ps: number,
  alloc: PageAlloc,
  pages: { index: number; bytes: Uint8Array }[],
): number {
  if (n.page !== 0) {
    return n.page;
  }
  const childPages: number[] = [];
  for (const c of n.children) {
    childPages.push(serializeDirty(c, table, capacity, ps, alloc, pages));
  }
  const w = new ByteWriter();
  let pageType = PAGE_LEAF;
  if (n.children.length > 0) {
    pageType = PAGE_INTERIOR;
    for (const cp of childPages) w.u32(cp);
  }
  for (let i = 0; i < n.keys.length; i++) {
    w.bytes(encodeRecord(table, n.keys[i]!, n.vals[i]!));
  }
  const payload = w.toBytes();
  if (payload.length > capacity) {
    throw engineError("feature_not_supported", "a record larger than the per-row limit is not supported");
  }
  const index = alloc.take();
  n.page = index;
  pages.push({ index, bytes: makePage(ps, pageType, n.keys.length, 0, payload) });
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
  if (pageSize < PAGE_HEADER + 36 || image.length < pageSize * 2) {
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
      const { table, root } = decodeTableEntry(pg.payload, cur);
      const colTypes = table.columns.map((c) => c.type);
      const hasPK = primaryKeyIndex(table) >= 0;
      snap.putTable(table, pageSize);
      if (root !== 0) {
        const t = readTree(image, dv, pageSize, root, colTypes);
        collectNodePages(t.node, reached);
        const store = snap.stores.get(table.name.toLowerCase())!;
        store.setTree(t.node, t.length);
        // No-PK keys are synthetic int64 rowids — advance the counter past the largest (the last
        // entry in key order) so future inserts don't collide.
        if (!hasPK && t.length > 0) {
          const entries = store.entriesInKeyOrder();
          store.bumpRowidTo(decodeInt("int64", entries[entries.length - 1]!.key) + 1n);
        }
      }
    }
    catPage = pg.nextPage;
  }
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

// collectNodePages records the on-disk page index of node and every descendant (a loaded tree, all
// pages set) into reached — the live set the free-list reconstruction subtracts from
// [2, pageCount) (P6.2).
function collectNodePages(n: PNode, reached: Set<number>): void {
  reached.add(n.page);
  for (const c of n.children) collectNodePages(c, reached);
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
  colTypes: ScalarType[],
): { node: PNode; length: number } {
  const pg = readPage(image, dv, ps, pageIdx);
  if (pg.pageType === PAGE_LEAF) {
    const n = pg.itemCount;
    const keys: Uint8Array[] = [];
    const vals: Row[] = [];
    const weights: number[] = [];
    const cur = { pos: 0 };
    for (let i = 0; i < n; i++) {
      const { key, row } = decodeRecord(colTypes, pg.payload, cur);
      weights.push(recordSize(colTypes, key, row));
      keys.push(key);
      vals.push(row);
    }
    return { node: { keys, vals, weights, children: [], page: pageIdx }, length: n };
  }
  if (pg.pageType === PAGE_INTERIOR) {
    const n = pg.itemCount;
    const cur = { pos: 0 };
    const children: PNode[] = [];
    let total = 0;
    for (let i = 0; i < n + 1; i++) {
      const cp = readU32(pg.payload, cur);
      const r = readTree(image, dv, ps, cp, colTypes);
      children.push(r.node);
      total += r.length;
    }
    const keys: Uint8Array[] = [];
    const vals: Row[] = [];
    const weights: number[] = [];
    for (let i = 0; i < n; i++) {
      const { key, row } = decodeRecord(colTypes, pg.payload, cur);
      weights.push(recordSize(colTypes, key, row));
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
    // A decimal column carries its typmod (precision, scale); precision 0 = unconstrained.
    let decimal: DecimalTypmod | null = null;
    if (ty === "decimal") {
      const precision = readU16(buf, cur);
      const scale = readU16(buf, cur);
      if (precision !== 0) decimal = { precision, scale };
    }
    // The default value follows the typmod, present iff flags bit2 (same value codec as rows).
    // Absent → no bytes consumed (format.md).
    const colDefault = (flags & 0b100) !== 0 ? readValue(ty, buf, cur) : null;
    columns.push({
      name: cname,
      type: ty,
      decimal,
      primaryKey: (flags & 0b01) !== 0,
      notNull: (flags & 0b10) !== 0,
      default: colDefault,
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
  if (tag === 0x00) {
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
    if (ty === "decimal") {
      // flags (sign), u16 scale, u16 ndigits, then that many base-10^4 groups.
      const flags = readU8(buf, cur);
      const scale = readU16(buf, cur);
      const ndigits = readU16(buf, cur);
      const groups: number[] = new Array(ndigits);
      for (let i = 0; i < ndigits; i++) groups[i] = readU16(buf, cur);
      return decimalValue(Decimal.fromCodec((flags & 1) !== 0, scale, groups));
    }
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
    if (isTimestamp(ty)) return timestampValue(decodeInt(ty, take(buf, cur, widthBytes(ty))));
    if (isTimestamptz(ty)) return timestamptzValue(decodeInt(ty, take(buf, cur, widthBytes(ty))));
    return intValue(decodeInt(ty, take(buf, cur, widthBytes(ty))));
  }
  if (tag === 0x01) return nullValue();
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
