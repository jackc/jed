// Validated COW descriptor (format v33; v34 adds the live page count at meta offset 56). This module is host-independent: recovery hashes raw
// stored bytes before decoding any candidate catalog, tree, or free-list page.
import { crc32Update } from "./crc32.ts";
import { engineError } from "./errors.ts";
import { ioSteps, runSync, type Steps } from "./execution.ts";
import { NO_TAG } from "./cap.ts";

// PAGE_IO_WINDOW bounds the write side's concurrent page work (ts-runtime.md §11.3): pages encoded at
// once (and encoded pages awaiting their write), and dependencies recovery reads ahead. Physical
// scheduling state, never SQL cost.
export const PAGE_IO_WINDOW = 16;

// ManifestPageRead is one page of a recovery read window: its bytes, or the error reading it raised,
// reported only if validation reaches that page.
export type ManifestPageRead = { ok: true; bytes: Uint8Array } | { ok: false; error: unknown };

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

// The inline manifest's byte range in a meta page: after the 64-byte header, or — in an encrypted
// meta — after the catalog-root and free-list tags (offset 96) and before the 40-byte reserve
// (spec/fileformat/encrypted.md §6).
function inlineArea(pageSize: number, encrypted: boolean): [number, number] {
  return encrypted ? [96, pageSize - 40] : [64, pageSize];
}
export const manifestInlineCapacity = (pageSize: number, encrypted = false): number => {
  const [lo, hi] = inlineArea(pageSize, encrypted);
  return Math.floor((hi - lo) / 12);
};
export const manifestOverflowCapacity = (pageSize: number): number =>
  Math.floor((pageSize - 32) / 12);
export type WrittenPage = { index: number; bytes: Uint8Array };
type Entry = { index: number; checksum: bigint };
// ManifestEntry is one manifest record: a page id and the CRC-64 of its complete encoded bytes.
export type ManifestEntry = Entry;

// manifestEntriesOf digests written pages for a manifest.
export function manifestEntriesOf(pages: WrittenPage[]): ManifestEntry[] {
  return pages.map((pg) => ({ index: pg.index, checksum: crc64Update(0n, pg.bytes) }));
}
export type CommitManifest = { entries: Entry[]; pages: WrittenPage[]; chainChecksum: bigint };
export type CommitMeta = {
  // suite is the encryption suite (meta offset 6 — spec/fileformat/encrypted.md §6): 0 plaintext,
  // 1 the encrypted variant, whose rootTag/freeListTag are the tags at offsets 64/80 (zero otherwise).
  suite: number;
  txid: bigint;
  rootPage: number;
  rootTag: Uint8Array;
  freeListTag: Uint8Array;
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
export function pageChecksum(block: Uint8Array): number {
  return crc32Update(crc32Update(0, block.subarray(0, 12)), block.subarray(16));
}
function view(block: Uint8Array): DataView {
  return new DataView(block.buffer, block.byteOffset, block.byteLength);
}
function zero(bytes: Uint8Array, start: number): boolean {
  return zeroRange(bytes, start, bytes.length);
}
function zeroRange(bytes: Uint8Array, start: number, end: number): boolean {
  for (let i = start; i < end; i++) if (bytes[i] !== 0) return false;
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

// buildCommitManifest encodes the manifest of a commit's entries: its body and free-list pages, never
// the manifest itself, and under batched durability also the pending set (validated-cow.md §4).
export function buildCommitManifest(
  pageSize: number,
  txid: bigint,
  dirty: ManifestEntry[],
  overflowIds: number[],
  encrypted = false,
): CommitManifest {
  const entries = dirty.slice().sort((a, b) => a.index - b.index);
  for (let i = 1; i < entries.length; i++) {
    if (entries[i].index === entries[i - 1].index)
      throw engineError("data_corrupted", "duplicate commit page");
  }
  const cap = manifestOverflowCapacity(pageSize);
  let cursor = Math.min(entries.length, manifestInlineCapacity(pageSize, encrypted));
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

export function writeManifestMeta(
  bytes: Uint8Array,
  manifest?: CommitManifest,
  encrypted = false,
): void {
  const dv = view(bytes);
  if (manifest !== undefined) {
    dv.setUint32(36, manifest.pages[0]?.index ?? 0, false);
    dv.setUint32(40, manifest.entries.length, false);
    dv.setUint32(44, manifest.pages.length, false);
    dv.setBigUint64(48, manifest.chainChecksum, false);
    const [lo] = inlineArea(bytes.length, encrypted);
    const count = Math.min(
      manifest.entries.length,
      manifestInlineCapacity(bytes.length, encrypted),
    );
    for (let i = 0; i < count; i++) putEntry(dv, lo + i * 12, manifest.entries[i]);
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
    dv.getUint16(4, false) !== 50 || // format_version (format.ts FORMAT_VERSION)
    dv.getUint32(8, false) !== pageSize ||
    dv.getUint32(32, false) !== metaChecksum(block) ||
    dv.getUint32(60, false) !== 0
  )
    return null;
  // Offsets 6/7: the encryption suite and its reserve (encrypted.md §6) — 0, 0 in plaintext; the
  // encrypted variant (suite 1, reserve 40) needs a page of at least 1024 bytes.
  let suite: number;
  if (block[6] === 0 && block[7] === 0) suite = 0;
  else if (block[6] === 1 && block[7] === 40 && pageSize >= 1024) suite = 1;
  else return null;
  const encrypted = suite !== 0;
  const first = encrypted ? 4 : 2;
  const pageCount = dv.getUint32(24, false);
  const rootPage = dv.getUint32(20, false);
  const freeListHead = dv.getUint32(28, false);
  const livePages = dv.getUint32(56, false);
  const dirtyCount = dv.getUint32(40, false);
  const overflowHead = dv.getUint32(36, false);
  const overflowCount = dv.getUint32(44, false);
  const chainChecksum = dv.getBigUint64(48, false);
  const bodyPage = (p: number): boolean => p >= first && p < pageCount;
  if (
    pageCount <= first ||
    pageCount > physicalPages ||
    !bodyPage(rootPage) ||
    (freeListHead !== 0 && !bodyPage(freeListHead)) ||
    // v34: the live pages are a subset of the body pages.
    livePages > pageCount - first ||
    dirtyCount > pageCount - first ||
    overflowCount > pageCount - first - dirtyCount ||
    (overflowCount === 0) !== (overflowHead === 0) ||
    (overflowHead !== 0 && !bodyPage(overflowHead)) ||
    (overflowCount === 0 && chainChecksum !== 0n)
  )
    return null;
  const [lo, hi] = inlineArea(pageSize, encrypted);
  const inlineCount = Math.min(dirtyCount, manifestInlineCapacity(pageSize, encrypted));
  if (
    dirtyCount - inlineCount > overflowCount * manifestOverflowCapacity(pageSize) ||
    !zeroRange(block, lo + inlineCount * 12, hi)
  )
    return null;
  if (dirtyCount === 0 && overflowCount !== 0) return null;
  const rootTag = encrypted ? block.slice(64, 80) : NO_TAG;
  const freeListTag = encrypted ? block.slice(80, 96) : NO_TAG;
  if (freeListHead === 0 && !zeroRange(freeListTag, 0, freeListTag.length)) return null;
  const inline: Entry[] = [];
  for (let i = 0; i < inlineCount; i++) inline.push(getEntry(dv, lo + i * 12));
  return {
    suite,
    txid: dv.getBigUint64(12, false),
    rootPage,
    rootTag,
    freeListTag,
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

// manifestChainSteps is the manifest-overflow chain a valid meta names, in chain order, as stored —
// what an encrypted meta's tag covers (spec/fileformat/encrypted.md §6). Reads only the pages the
// meta counts; a malformed chain was already refused by validateCommitMetaSteps.
export function* manifestChainSteps(
  meta: CommitMeta,
  read: (index: number) => Steps<Uint8Array>,
): Steps<WrittenPage[]> {
  const out: WrittenPage[] = [];
  let next = meta.overflowHead;
  for (let ordinal = 0; ordinal < meta.overflowCount; ordinal++) {
    const bytes = yield* read(next);
    out.push({ index: next, bytes });
    next = view(bytes).getUint32(8, false);
  }
  return out;
}

// Null denotes an incomplete/corrupt candidate, permitting the previous meta. Actual host I/O
// exceptions propagate: inability to read a device must not silently select an older generation.
export function validateCommitMeta(
  meta: CommitMeta,
  pageSize: number,
  read: (index: number) => Uint8Array,
): Set<number> | null {
  return runSync(validateCommitMetaSteps(meta, pageSize, (index) => ioSteps(() => read(index))));
}

// validateCommitMetaSteps is validateCommitMeta over an awaitable page read. window, when it returns
// a non-null array, reads up to PAGE_IO_WINDOW listed dependencies ahead (ts-runtime.md §11.3);
// results and errors are consumed in manifest order, so selection matches the sequential walk.
export function* validateCommitMetaSteps(
  meta: CommitMeta,
  pageSize: number,
  read: (index: number) => Steps<Uint8Array>,
  window?: (indices: number[]) => Steps<ManifestPageRead[] | null>,
): Steps<Set<number> | null> {
  if (meta.dirtyCount === 0) return new Set(); // fully flushed bootstrap/snapshot image
  // An encrypted meta (spec/fileformat/encrypted.md §6): body pages start past the key block, and a
  // listed page is ciphertext, so only its digest is checked.
  const encrypted = meta.suite !== 0;
  const first = encrypted ? 4 : 2;
  const entries = meta.inline.slice();
  const manifestIds = new Set<number>();
  let next = meta.overflowHead;
  let chainChecksum = 0n;
  const cap = manifestOverflowCapacity(pageSize);
  for (let ordinal = 0; ordinal < meta.overflowCount; ordinal++) {
    if (next < first || next >= meta.pageCount || manifestIds.has(next)) return null;
    const index = next;
    manifestIds.add(index);
    const bytes = yield* read(index);
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
  let readAhead = window;
  let ahead: ManifestPageRead[] | null = null;
  let aheadStart = 0;
  for (let k = 0; k < entries.length; k++) {
    const entry = entries[k]!;
    if (
      entry.index < first ||
      entry.index <= previous ||
      entry.index >= meta.pageCount ||
      manifestIds.has(entry.index)
    )
      return null;
    if (readAhead !== undefined && (ahead === null || k - aheadStart >= ahead.length)) {
      // Read ahead only pages the range check admits; a later entry the sequential walk never
      // reaches is read at most once and its error discarded.
      const indices: number[] = [];
      for (let j = k; j < entries.length && indices.length < PAGE_IO_WINDOW; j++) {
        const index = entries[j]!.index;
        if (index < first || index >= meta.pageCount) break;
        indices.push(index);
      }
      ahead = yield* readAhead(indices);
      aheadStart = k;
      if (ahead === null) readAhead = undefined; // a synchronous stack reads lazily
    }
    let bytes: Uint8Array;
    const prefetched = ahead?.[k - aheadStart];
    if (prefetched !== undefined) {
      if (!prefetched.ok) throw prefetched.error;
      bytes = prefetched.bytes;
    } else bytes = yield* read(entry.index);
    if (
      bytes.length !== pageSize ||
      crc64Update(0n, bytes) !== entry.checksum ||
      (!encrypted &&
        (bytes[0] < 1 || bytes[0] > 7 || pageChecksum(bytes) !== view(bytes).getUint32(12, false)))
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

// resolutionDigest is a commit's commit_status digest (spec/pageservice/README.md §3.5): the CRC-64 of
// its meta page, with — in the encrypted variant (byte 6 nonzero) — the CRC field (32..36) and the
// 40-byte reserve zeroed, so the meta a client sent and the meta its owner resealed for the slot it
// wrote resolve alike, a successor owner seeding its window from disk included (encrypted.md §14).
export function resolutionDigest(meta: Uint8Array): bigint {
  if (meta.length < 8 || meta[6] === 0) return crc64Update(0n, meta);
  const m = meta.slice();
  m.fill(0, 32, 36);
  m.fill(0, m.length - 40);
  return crc64Update(0n, m);
}
