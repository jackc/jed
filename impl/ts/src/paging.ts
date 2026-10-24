// Shared paging context for a file-backed database (spec/design/pager.md §2/§3): the open pager plus
// the bounded leaf BufferPool, shared by every table store and snapshot of one database. Page ids are
// file-global (one page space per file), so there is exactly ONE pool and one pager per database; a
// TableStore/Snapshot clone shares the same SharedPaging reference.
//
// The read path faults a clean leaf through faultLeaf: a pool hit returns the cached node, a miss
// reads the page through the pager, decodes it (the node codec, format.ts) and caches it, evicting
// under CLOCK when full. No pins (pager.md §4): eviction only drops the cache entry, and a clean leaf
// is immutable so any node still referenced stays alive (GC) and a re-load is a harmless duplicate.
//
// Not a §8 byte contract (pager.md §3): the pool changes WHEN a page is resident, never WHAT a query
// observes — so each core realizes it idiomatically (like P5.3's per-core concurrency). JS is
// single-threaded, so unlike the Rust/Go cores this needs no lock between the read and commit paths.

import type { BlockStore } from "./blockstore.ts";
import { type Cap, capOf, NO_TAG } from "./cap.ts";
import type { ColType } from "./catalog.ts";
import type { PageCodec } from "./crypto.ts";
import type { PNode } from "./pmap.ts";
import { BufferPool } from "./bufferpool.ts";
import { runSync, type Steps } from "./execution.ts";
import { engineError } from "./errors.ts";
import { decodeLeafNode, sameLeafShape } from "./format.ts";
import type { ManifestEntry } from "./commit_manifest.ts";
import type { BatchMirror, CommitFault, Pager, PageRead, PageTransform } from "./pager.ts";

// DEFAULT_CACHE_BYTES is the default memory budget for the resident leaf cache, in bytes (256 MiB) — the
// OpenOptions.cacheBytes default (spec/design/pager.md §3, api.md §2.1). Sized so the dominant case —
// a RAM-sized database (CLAUDE.md §9) — stays fully cache-resident under the default; stated in bytes
// so the budget does not silently scale with a file's page size. Converted to a leaf-page capacity by
// cacheLeaves.
export const DEFAULT_CACHE_BYTES = 256 * 1024 * 1024;

// cacheLeaves converts a byte budget to a resident-leaf-page capacity for a file of pageSize bytes:
// max(1, floor(cacheBytes / pageSize)) (pager.md §3). The max(1, …) floor keeps one leaf resident even
// when cacheBytes < pageSize — the minimum to walk a root→leaf path. The divisor is clamped to ≥ 1 so a
// malformed pageSize = 0 cannot divide by zero (the loader rejects it separately as corrupt — format.ts).
export function cacheLeaves(cacheBytes: number, pageSize: number): number {
  return Math.max(1, Math.floor(cacheBytes / Math.max(1, pageSize)));
}

// SharedPaging is one database's pager + leaf buffer pool, shared (reference) by all its stores and
// snapshots.
export class SharedPaging {
  private pager: Pager;
  private pool: BufferPool;
  // codec is an encrypted database's page codec (spec/fileformat/encrypted.md): every body page read
  // through readBody* is opened, and every commit page sealed, under its keys. null for a plaintext
  // file and every in-memory or temp store.
  private readonly pageCodec: PageCodec | null;
  // pageCap is the capacities the file's variant implies (format.md *Page model*, encrypted.md §1).
  private readonly pageCap: Cap;
  // prefetched holds opened body pages a storage rewrite read ahead through the awaitable read
  // (prefetchSteps), so the synchronous from-scratch serializer finds them without a read under an
  // asynchronous codec or device (spec/design/ts-runtime.md §11.1). null outside a rewrite.
  private prefetched: Map<number, Uint8Array> | null = null;
  // batches is set when the store fetches several pages in one request (a page client over the stream
  // transport): scans read ahead (prefetchLeaves).
  readonly batches: boolean;

  constructor(pager: Pager, capacity: number, codec: PageCodec | null = null) {
    this.pager = pager;
    this.pool = new BufferPool(capacity);
    this.batches = pager.batchesReads();
    this.pageCodec = codec;
    this.pageCap = capOf(pager.pageSize, codec !== null);
    if (codec !== null) pager.setEncrypted();
  }

  // trustsMeta reports whether the pager trusts the meta it is served (a page client's — pager.ts).
  trustsMeta(): boolean {
    return this.pager.trustMeta;
  }

  // codec is the database's page codec, when it is encrypted.
  codec(): PageCodec | null {
    return this.pageCodec;
  }

  // cap is the database's page capacities (plaintext or encrypted layout).
  cap(): Cap {
    return this.pageCap;
  }

  // openBody returns the plaintext block of the body page index whose stored bytes are stored and
  // whose pointer binds tag: the stored bytes of a plaintext file, or an encrypted page checked
  // against the tag and opened (encrypted.md §4).
  openBody(index: number, tag: Uint8Array, stored: Uint8Array): Uint8Array {
    return this.pageCodec === null ? stored : this.pageCodec.openBody(index, stored, tag);
  }

  // readBody / readBodySteps read the body page a (page, tag) pointer names on a query's read path;
  // readBodyPageSteps on the write side (commit, recovery, maintenance — readPageSteps).
  readBody(index: number, tag: Uint8Array = NO_TAG): Uint8Array {
    return runSync(this.readBodySteps(index, tag));
  }

  *readBodySteps(index: number, tag: Uint8Array = NO_TAG): Steps<Uint8Array> {
    const hit = this.prefetched?.get(index);
    if (hit !== undefined) return hit;
    return yield* this.openBodySteps(index, tag, yield* this.pager.readBlockSteps(index));
  }

  *readBodyPageSteps(index: number, tag: Uint8Array = NO_TAG): Steps<Uint8Array> {
    const hit = this.prefetched?.get(index);
    if (hit !== undefined) return hit;
    return yield* this.openBodySteps(index, tag, yield* this.pager.readPageSteps(index));
  }

  // prefetchSteps reads the body page a (page, tag) pointer names through the awaitable read and keeps
  // it opened until endPrefetch (a storage rewrite under an asynchronous codec, shared.ts).
  *prefetchSteps(index: number, tag: Uint8Array): Steps<Uint8Array> {
    if (this.prefetched === null) this.prefetched = new Map();
    const map = this.prefetched;
    const hit = map.get(index);
    if (hit !== undefined) return hit;
    const bytes = yield* this.readBodyPageSteps(index, tag);
    map.set(index, bytes);
    return bytes;
  }

  // endPrefetch drops every page prefetchSteps kept.
  endPrefetch(): void {
    this.prefetched = null;
  }

  // openBodySteps is openBody as a continuation step: an asynchronous codec's open is awaited
  // (crypto.ts PageCodec).
  *openBodySteps(index: number, tag: Uint8Array, stored: Uint8Array): Steps<Uint8Array> {
    const codec = this.pageCodec;
    if (codec === null) return stored;
    return yield* codec.openBodySteps(index, stored, tag);
  }

  // faultLeaf faults the clean leaf at page to a resident node, through the buffer pool: a hit returns
  // the cached node, a miss reads + decodes the page (with this table's colTypes) and caches it,
  // evicting under CLOCK if full. A page id belongs to exactly one table, so caching by global page id
  // with a caller-supplied decoder is consistent (pager.md §4). tag is the tag the leaf's parent binds
  // in an encrypted file.
  faultLeaf(page: number, colTypes: ColType[], tag: Uint8Array = NO_TAG): PNode {
    return runSync(this.faultLeafSteps(page, colTypes, tag));
  }

  *faultLeafSteps(page: number, colTypes: ColType[], tag: Uint8Array = NO_TAG): Steps<PNode> {
    // Materialize any external value by following its overflow chain through the pager (the leaf
    // block holds only the pointer — spec/design/large-values.md §12).
    // Lazy decode (spec/design/large-values.md §14): an external/compressed value stays an
    // unfetched reference — no chain read, no decompression. The scan layer resolves the
    // columns a query touches through readBlock below; `this` is stamped into each deferred
    // external value so a touched-set miss self-resolves (the B4 demand-fault backstop,
    // format.ts resolveUnfetchedSelf).
    const paging = this;
    const node = yield* this.pool.getOrLoadSteps(page, function* () {
      return decodeLeafNode(
        yield* paging.readBodySteps(page, tag),
        page,
        colTypes,
        paging,
        paging.pageCodec === null ? undefined : tag,
      );
    });
    // The pool is keyed by page alone, so a pointer into another tree's leaf can return a node
    // decoded for that tree's columns. Its directories were validated against those columns, not
    // these, so serving it would index regions that do not exist (storage.md §7).
    if (!sameLeafShape(node.packed!.colTypes, colTypes))
      throw engineError("data_corrupted", "leaf page belongs to a different tree");
    return node;
  }

  // prefetchLeaves reads ahead (spec/design/page-service.md §5.3): it brings the leaves at pages that
  // are not resident into the pool with one batched store read, decoded as a fault would decode them.
  // At most a quarter of the pool is read ahead at once, so read-ahead cannot evict the leaf the scan
  // is on. A failure is ignored: the fault that follows reads the page again and reports it.
  prefetchLeaves(pages: { page: number; tag: Uint8Array }[], colTypes: ColType[]): void {
    if (!this.batches) return;
    const limit = Math.max(Math.floor(this.pool.capacity / 4), 1);
    const missing: { page: number; tag: Uint8Array }[] = [];
    for (const p of pages) {
      if (missing.length === limit) break;
      if (!this.pool.contains(p.page)) missing.push(p);
    }
    if (missing.length < 2) return;
    let blocks: Uint8Array[];
    try {
      blocks = this.pager.readBlocks(missing.map((p) => p.page));
    } catch {
      return;
    }
    missing.forEach(({ page, tag }, i) => {
      try {
        this.pool.getOrLoad(page, () =>
          decodeLeafNode(
            this.openBody(page, tag, blocks[i]!),
            page,
            colTypes,
            this,
            this.pageCodec === null ? undefined : tag,
          ),
        );
      } catch {
        // The fault reports it.
      }
    });
  }

  // readBlock reads one page through the pager — the demand-paged loader reads the meta, catalog, and
  // interior skeleton this way (format.ts loadEnginePaged).
  readBlock(index: number): Uint8Array {
    return this.pager.readBlock(index);
  }

  readBlockSteps(index: number): Steps<Uint8Array> {
    return this.pager.readBlockSteps(index);
  }

  // The write side's page reads and writes (ts-runtime.md §11): inline over a synchronous storage
  // stack, awaitable when the page transform or device is asynchronous (Pager has the details).
  readPageSteps(index: number): Steps<Uint8Array> {
    return this.pager.readPageSteps(index);
  }
  readWindowSteps(indices: number[]): Steps<PageRead[] | null> {
    return this.pager.readWindowSteps(indices);
  }
  // writePagesSteps writes pages in order, dropping each rewritten page's stale pool entry once its
  // write completed (invalidate).
  writePagesSteps(pages: { index: number; bytes: Uint8Array }[]): Steps<void> {
    return this.pager.writePagesSteps(pages, (index) => this.pool.invalidate(index));
  }
  reserveSteps(minPages: number): Steps<void> {
    return this.pager.reserveSteps(minPages);
  }
  syncSteps(): Steps<void> {
    return this.pager.syncSteps();
  }
  flushPendingSteps(): Steps<void> {
    return this.pager.flushPendingSteps();
  }
  beginDurableCommitSteps(): Steps<void> {
    return this.pager.beginDurableCommitSteps();
  }
  settleTruncationSteps(pageCount: number, lowered: boolean, durable: boolean): Steps<void> {
    return this.pager.settleTruncationSteps(pageCount, lowered, durable);
  }
  // writeSideAsync reports whether the write side awaits: an asynchronous device or page transform, or
  // an asynchronous page codec (ts-runtime.md §11.4).
  writeSideAsync(): boolean {
    return this.pager.writeSideAsync() || (this.pageCodec?.async ?? false);
  }
  // setPageTransform installs the page codec (the seam of ts-runtime.md §11.5).
  setPageTransform(transform: PageTransform | null): void {
    this.pager.setPageTransform(transform);
  }

  // reserve / writeBlock / sync / close drive the commit write path (file.ts persistImpl) and handle
  // release. reserve preallocates file growth in chunks ahead of the high-water (spec/design/pager.md §7).
  reserve(minPages: number): void {
    this.pager.reserve(minPages);
  }

  // settleTruncation records a commit's high-water and shortens the file after the barrier that covers
  // it (pager.ts settleTruncation).
  settleTruncation(pageCount: number, lowered: boolean, durable: boolean): void {
    this.pager.settleTruncation(pageCount, lowered, durable);
  }

  writeBlock(index: number, bytes: Uint8Array): void {
    this.pager.writeBlock(index, bytes);
  }

  // invalidate drops any stale pool entry for a rewritten page (bufferpool.ts invalidate). The commit
  // write path calls it after writeBlock: a no-op unless a reclaim domain reused a freed page id, in
  // which case the pool's prior decode of that page must be evicted (the pool is private here, so the
  // persist path reaches BufferPool.invalidate only through this delegator).
  invalidate(page: number): void {
    this.pool.invalidate(page);
  }

  // invalidateAll drops the whole pool (a page client's wholesale invalidation, README.md §3.2).
  invalidateAll(): void {
    this.pool.invalidateAll();
  }

  sync(): void {
    this.pager.sync();
  }

  refreshAllocatedPages(): void {
    this.pager.refreshAllocatedPages();
  }

  adoptCommit(
    txid: bigint,
    checksum: number,
    needsSync: boolean,
    slot: number,
    bothSlots: boolean,
  ): void {
    this.pager.adoptCommit(txid, checksum, needsSync, slot, bothSlots);
  }

  // Batched durability (spec/design/validated-cow.md §4; Pager has the details).
  setBatchMirror(mirror: BatchMirror | null): void {
    this.pager.setBatchMirror(mirror);
  }
  setFloorSlot(slot: number, txid: bigint): void {
    this.pager.setFloorSlot(slot, txid);
  }
  floorMetaSlot(): number {
    return this.pager.floorMetaSlot();
  }
  floorMetaTxid(): bigint {
    return this.pager.floorMetaTxid();
  }
  commitSlot(): number {
    return this.pager.commitSlot();
  }
  hasPending(): boolean {
    return this.pager.hasPending();
  }
  pendingEntries(): ManifestEntry[] {
    return this.pager.pendingEntries();
  }
  recordCommit(
    txid: bigint,
    checksum: number,
    slot: number,
    durable: boolean,
    dirty: ManifestEntry[],
  ): void {
    this.pager.recordCommit(txid, checksum, slot, durable, dirty);
  }
  batchCapReached(): boolean {
    return this.pager.batchCapReached();
  }
  flushPending(): void {
    this.pager.flushPending();
  }
  batchFlushed(): boolean {
    return this.pager.batchFlushed();
  }
  revoke(error: Error): void {
    this.pager.revoke(error);
  }

  checkDurableCommit(): void {
    this.pager.checkDurableCommit();
  }
  commitRequiresReopen(): boolean {
    return this.pager.commitRequiresReopen();
  }
  beginDurableCommit(): void {
    this.pager.beginDurableCommit();
  }
  finishDurableCommit(txid: bigint, checksum: number): void {
    this.pager.finishDurableCommit(txid, checksum);
  }
  abandonDurableCommit(): void {
    this.pager.abandonDurableCommit();
  }
  cachedValidation(meta: Uint8Array): Set<number> | null {
    return this.pager.cachedValidation(meta);
  }
  cacheValidation(meta: Uint8Array, pages: Set<number>): void {
    this.pager.cacheValidation(meta, pages);
  }
  physicalPages(): number {
    return this.pager.physicalPages();
  }

  // swapStore / skipsSync / poison reach the pager for host compaction (spec/design/api.md §2.6): the
  // old file is closed by swapping in a ClosedBlockStore (and swapped back if the rename fails), the
  // rewrite applies the handle's fsync=off setting, and a pager whose file was replaced refuses commits.
  swapStore(store: BlockStore): BlockStore {
    return this.pager.swapStore(store);
  }
  skipsSync(): boolean {
    return this.pager.skipsSync();
  }
  // migrateStore moves the pager's bytes into store and adopts it (a spilling temp domain,
  // spec/design/temp-tables.md §6 — Pager.migrateStore).
  migrateStore(store: BlockStore): void {
    this.pager.migrateStore(store);
  }
  poison(): void {
    this.pager.poison();
  }

  // capacity is the pool's leaf-page capacity — carried over to the replacement pool when compaction
  // reopens the database (spec/design/api.md §2.6).
  capacity(): number {
    return this.pool.capacity;
  }

  // setCapacity re-bounds the pool to capacity leaves, evicting until the resident set fits (a spilled
  // temp domain, spec/design/temp-tables.md §6).
  setCapacity(capacity: number): void {
    this.pool.setCapacity(capacity);
  }

  // armFault arms a one-shot commit fault on the backing pager — the fault-injection seam
  // (spec/design/storage.md §7), used by the crash-recovery tests. Testing only.
  armFault(fault: CommitFault): void {
    this.pager.armFault(fault);
  }

  close(): void {
    this.pager.close();
  }

  // pageSize is fixed into the file's meta header (format.md) — the block width the loader reads at.
  pageSize(): number {
    return this.pager.pageSize;
  }

  // residentLeaves is the number of leaf pages currently resident in the pool — the bound the
  // demand-paging tests assert stays below the budget even for a database far larger than it. P6.4c
  // promotes it to the public memory-budget surface.
  residentLeaves(): number {
    return this.pool.resident();
  }
}
