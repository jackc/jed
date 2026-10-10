// Validated COW descriptor (format v33; v34 adds the live page count at meta offset 56). This module is host-independent: recovery hashes raw
// stored bytes before decoding any candidate catalog, tree, or free-list page.
import { crc32Update } from "./crc32.ts";
import { engineError } from "./errors.ts";

const TABLE_HI = new Uint32Array(256);
const TABLE_LO = new Uint32Array(256);
for (let i = 0; i < 256; i++) {
  let hi = (i << 24) >>> 0;
  let lo = 0;
  for (let bit = 0; bit < 8; bit++) {
    const top = hi >>> 31;
    hi = ((hi << 1) | (lo >>> 31)) >>> 0;
    lo = (lo << 1) >>> 0;
    if (top !== 0) {
      hi = (hi ^ 0x42f0e1eb) >>> 0;
      lo = (lo ^ 0xa9ea3693) >>> 0;
    }
  }
  TABLE_HI[i] = hi;
  TABLE_LO[i] = lo;
}

// CRC-64/ECMA-182: non-reflected, initial/final zero. Two u32 limbs avoid per-byte BigInt work.
export function crc64Update(previous: bigint, bytes: Uint8Array): bigint {
  let hi = Number(previous >> 32n) >>> 0;
  let lo = Number(previous & 0xffff_ffffn) >>> 0;
  for (const byte of bytes) {
    const index = (hi >>> 24) ^ byte;
    hi = (((hi << 8) | (lo >>> 24)) ^ TABLE_HI[index]) >>> 0;
    lo = ((lo << 8) ^ TABLE_LO[index]) >>> 0;
  }
  return (BigInt(hi) << 32n) | BigInt(lo);
}

export const manifestInlineCapacity = (pageSize: number): number =>
  Math.floor((pageSize - 64) / 12);
export const manifestOverflowCapacity = (pageSize: number): number =>
  Math.floor((pageSize - 32) / 12);
export type WrittenPage = { index: number; bytes: Uint8Array };
type Entry = { index: number; checksum: bigint };
export type CommitManifest = { entries: Entry[]; pages: WrittenPage[]; chainChecksum: bigint };
export type CommitMeta = {
  txid: bigint;
  rootPage: number;
  pageCount: number;
  freeListHead: number;
  // livePages is the exact live page count (v34 — meta offset 56, spec/design/memory.md §8.7).
  livePages: number;
  dirtyCount: number;
  overflowHead: number;
  overflowCount: number;
  chainChecksum: bigint;
  inline: Entry[];
};

export function metaChecksum(block: Uint8Array): number {
  return crc32Update(crc32Update(0, block.subarray(0, 32)), block.subarray(36));
}
function pageChecksum(block: Uint8Array): number {
  return crc32Update(crc32Update(0, block.subarray(0, 12)), block.subarray(16));
}
function view(block: Uint8Array): DataView {
  return new DataView(block.buffer, block.byteOffset, block.byteLength);
}
function zero(bytes: Uint8Array, start: number): boolean {
  for (let i = start; i < bytes.length; i++) if (bytes[i] !== 0) return false;
  return true;
}
function putEntry(dv: DataView, offset: number, entry: Entry): void {
  dv.setUint32(offset, entry.index, false);
  dv.setBigUint64(offset + 4, entry.checksum, false);
}
function getEntry(dv: DataView, offset: number): Entry {
  return { index: dv.getUint32(offset, false), checksum: dv.getBigUint64(offset + 4, false) };
}
function chainUpdate(crc: bigint, index: number, bytes: Uint8Array): bigint {
  const id = new Uint8Array(4);
  view(id).setUint32(0, index, false);
  return crc64Update(crc64Update(crc, id), bytes);
}

export function buildCommitManifest(
  pageSize: number,
  txid: bigint,
  written: WrittenPage[],
  overflowIds: number[],
): CommitManifest {
  const entries = written
    .map((pg) => ({ index: pg.index, checksum: crc64Update(0n, pg.bytes) }))
    .sort((a, b) => a.index - b.index);
  for (let i = 1; i < entries.length; i++) {
    if (entries[i].index === entries[i - 1].index)
      throw engineError("data_corrupted", "duplicate commit page");
  }
  const cap = manifestOverflowCapacity(pageSize);
  let cursor = Math.min(entries.length, manifestInlineCapacity(pageSize));
  const pages: WrittenPage[] = [];
  let chainChecksum = 0n;
  for (let ordinal = 0; ordinal < overflowIds.length; ordinal++) {
    const bytes = new Uint8Array(pageSize);
    const dv = view(bytes);
    const count = Math.min(cap, entries.length - cursor);
    bytes[0] = 8;
    dv.setUint32(4, count, false);
    dv.setUint32(8, overflowIds[ordinal + 1] ?? 0, false);
    dv.setBigUint64(16, txid, false);
    dv.setUint32(24, ordinal, false);
    for (let j = 0; j < count; j++) putEntry(dv, 32 + j * 12, entries[cursor++]);
    dv.setUint32(12, pageChecksum(bytes), false);
    const index = overflowIds[ordinal];
    pages.push({ index, bytes });
    chainChecksum = chainUpdate(chainChecksum, index, bytes);
  }
  if (cursor !== entries.length)
    throw engineError("data_corrupted", "commit manifest capacity exhausted");
  return { entries, pages, chainChecksum };
}

export function writeManifestMeta(bytes: Uint8Array, manifest?: CommitManifest): void {
  const dv = view(bytes);
  if (manifest !== undefined) {
    dv.setUint32(36, manifest.pages[0]?.index ?? 0, false);
    dv.setUint32(40, manifest.entries.length, false);
    dv.setUint32(44, manifest.pages.length, false);
    dv.setBigUint64(48, manifest.chainChecksum, false);
    const count = Math.min(manifest.entries.length, manifestInlineCapacity(bytes.length));
    for (let i = 0; i < count; i++) putEntry(dv, 64 + i * 12, manifest.entries[i]);
  }
  dv.setUint32(32, metaChecksum(bytes), false);
}

export function parseCommitMeta(
  block: Uint8Array,
  pageSize: number,
  physicalPages: number,
): CommitMeta | null {
  if (block.length !== pageSize || block.length < 64) return null;
  const dv = view(block);
  if (
    dv.getUint32(0, false) !== 0x4a454442 ||
    dv.getUint16(4, false) !== 34 ||
    dv.getUint16(6, false) !== 0 ||
    dv.getUint32(8, false) !== pageSize ||
    dv.getUint32(32, false) !== metaChecksum(block) ||
    dv.getUint32(60, false) !== 0
  )
    return null;
  const pageCount = dv.getUint32(24, false);
  const rootPage = dv.getUint32(20, false);
  const freeListHead = dv.getUint32(28, false);
  const livePages = dv.getUint32(56, false);
  const dirtyCount = dv.getUint32(40, false);
  const overflowHead = dv.getUint32(36, false);
  const overflowCount = dv.getUint32(44, false);
  const chainChecksum = dv.getBigUint64(48, false);
  const bodyPage = (p: number): boolean => p >= 2 && p < pageCount;
  if (
    pageCount < 3 ||
    pageCount > physicalPages ||
    !bodyPage(rootPage) ||
    (freeListHead !== 0 && !bodyPage(freeListHead)) ||
    // v34: the live pages are a subset of the body pages.
    livePages > pageCount - 2 ||
    dirtyCount > pageCount - 2 ||
    overflowCount > pageCount - 2 - dirtyCount ||
    (overflowCount === 0) !== (overflowHead === 0) ||
    (overflowHead !== 0 && !bodyPage(overflowHead)) ||
    (overflowCount === 0 && chainChecksum !== 0n)
  )
    return null;
  const inlineCount = Math.min(dirtyCount, manifestInlineCapacity(pageSize));
  if (
    dirtyCount - inlineCount > overflowCount * manifestOverflowCapacity(pageSize) ||
    !zero(block, 64 + inlineCount * 12)
  )
    return null;
  if (dirtyCount === 0 && overflowCount !== 0) return null;
  const inline: Entry[] = [];
  for (let i = 0; i < inlineCount; i++) inline.push(getEntry(dv, 64 + i * 12));
  return {
    txid: dv.getBigUint64(12, false),
    rootPage,
    pageCount,
    freeListHead,
    livePages,
    dirtyCount,
    overflowHead,
    overflowCount,
    chainChecksum,
    inline,
  };
}

// Null denotes an incomplete/corrupt candidate, permitting the previous meta. Actual host I/O
// exceptions propagate: inability to read a device must not silently select an older generation.
export function validateCommitMeta(
  meta: CommitMeta,
  pageSize: number,
  read: (index: number) => Uint8Array,
): Set<number> | null {
  if (meta.dirtyCount === 0) return new Set(); // fully flushed bootstrap/snapshot image
  const entries = meta.inline.slice();
  const manifestIds = new Set<number>();
  let next = meta.overflowHead;
  let chainChecksum = 0n;
  const cap = manifestOverflowCapacity(pageSize);
  for (let ordinal = 0; ordinal < meta.overflowCount; ordinal++) {
    if (next < 2 || next >= meta.pageCount || manifestIds.has(next)) return null;
    const index = next;
    manifestIds.add(index);
    const bytes = read(index);
    if (bytes.length !== pageSize) return null;
    const dv = view(bytes);
    const count = dv.getUint32(4, false);
    if (
      bytes[0] !== 8 ||
      bytes[1] !== 0 ||
      bytes[2] !== 0 ||
      bytes[3] !== 0 ||
      dv.getUint32(12, false) !== pageChecksum(bytes) ||
      dv.getBigUint64(16, false) !== meta.txid ||
      dv.getUint32(24, false) !== ordinal ||
      dv.getUint32(28, false) !== 0 ||
      count !== Math.min(cap, meta.dirtyCount - entries.length) ||
      !zero(bytes, 32 + count * 12)
    )
      return null;
    for (let i = 0; i < count; i++) entries.push(getEntry(dv, 32 + i * 12));
    chainChecksum = chainUpdate(chainChecksum, index, bytes);
    next = dv.getUint32(8, false);
  }
  if (next !== 0 || entries.length !== meta.dirtyCount || chainChecksum !== meta.chainChecksum)
    return null;
  let previous = 1;
  let rootCovered = false;
  let freeCovered = meta.freeListHead === 0;
  for (const entry of entries) {
    if (entry.index <= previous || entry.index >= meta.pageCount || manifestIds.has(entry.index))
      return null;
    const bytes = read(entry.index);
    if (
      bytes.length !== pageSize ||
      crc64Update(0n, bytes) !== entry.checksum ||
      bytes[0] < 1 ||
      bytes[0] > 7 ||
      pageChecksum(bytes) !== view(bytes).getUint32(12, false)
    )
      return null;
    if (entry.index === meta.rootPage) rootCovered = true;
    if (entry.index === meta.freeListHead) freeCovered = true;
    previous = entry.index;
  }
  if (!rootCovered || !freeCovered) return null;
  for (const entry of entries) manifestIds.add(entry.index);
  return manifestIds;
}
