// Block-device pager — the host-independent storage policy (spec/design/pager.md) above the
// BlockStore host seam (spec/design/hosts.md §3). It composes a BlockStore kept open for the handle's
// life, expressing the rest of the core's page-level operations (readBlock/writeBlock/reserve/sync)
// over the host's byte device — converting a page index to a byte offset (index × pageSize), owning
// the 1 MiB preallocation chunk, and deciding which durability barrier each step needs. The host (the
// Node `fs` backing, FileBlockStore in blockstore.ts) is the only per-platform code below; everything
// here is identical across hosts (hosts.md §1).
//
// The whole-image load and the commit route through readBlock/writeBlock; the bounded buffer pool +
// lazy node loading that make the resident set bounded (P6.4b) read through this same readBlock. The
// fault-injection seam (spec/design/storage.md §7) lives here, not in the host — it tests the commit
// recipe, which is host-independent (hosts.md §3).

import { type BlockStore, FailingBlockStore } from "./blockstore.ts";
import {
  type ManifestEntry,
  manifestInlineCapacity,
  type ManifestPageRead,
  PAGE_IO_WINDOW,
} from "./commit_manifest.ts";
import { type EngineError, engineError } from "./errors.ts";
import { deviceSteps, ioSteps, runSync, type Steps } from "./execution.ts";

// PREALLOC_CHUNK_BYTES is the MAXIMUM file-growth step — ~1 MiB worth of pages. The file grows
// geometrically (≈doubling its current size — reserve), so a small database's file stays proportional
// to its data instead of jumping to a fixed 1 MiB; this caps a step so a large database never
// over-reserves more than ~1 MiB of slack. Real, durably-allocated zero blocks let a steady-state
// commit write its body into already-allocated space, so the per-commit fdatasync (Pager.sync) carries
// no ext4 metadata-journaling for a file-size change — the durable-commit win (spec/design/pager.md §7,
// TODO.md). Each allocating fsync (Pager.reserve) amortizes across the pages it reserves.
const PREALLOC_CHUNK_BYTES = 1024 * 1024;

// PREALLOC_FLOOR_BYTES is the MINIMUM file-growth step — 16 KiB worth of pages. It floors the geometric
// growth so a fresh load does not fsync every page or two while the file is still tiny; above it the
// doubling does the amortizing. Denominated in bytes (not pages) so it scales with pageSize like the
// cap — at a 64 KiB page size it bottoms out at a single page, at 256 B it is 64 pages, reserving the
// same ~16 KiB either way.
const PREALLOC_FLOOR_BYTES = 16 * 1024;

// preallocChunkPages is the preallocation CAP in pages for a file of pageSize bytes: max(1,
// 1 MiB / pageSize).
function preallocChunkPages(pageSize: number): number {
  return Math.max(1, Math.floor(PREALLOC_CHUNK_BYTES / Math.max(1, pageSize)));
}

// preallocFloorPages is the preallocation FLOOR in pages for a file of pageSize bytes: max(1,
// 16 KiB / pageSize). Always ≤ preallocChunkPages (16 KiB ≤ 1 MiB), so reserve's clamp is well-formed.
function preallocFloorPages(pageSize: number): number {
  return Math.max(1, Math.floor(PREALLOC_FLOOR_BYTES / Math.max(1, pageSize)));
}

// growthStep is the pages reserve adds in one step growing from pages: the current size, floored at
// preallocFloorPages and capped at preallocChunkPages (pager.md §7).
export function growthStep(pages: number, pageSize: number): number {
  return Math.min(preallocChunkPages(pageSize), Math.max(preallocFloorPages(pageSize), pages));
}

// FaultPoint selects a point in the commit write sequence at which the fault-injection seam
// (spec/design/storage.md §7) simulates a crash. Pages 0/1 are always the meta slots and every
// body/catalog page is ≥ 2 (format.md), so "meta_write" is identified by the page index, never by
// counting body pages. Testing only.
export type FaultPoint = "body_write" | "meta_write" | "sync";

// CommitFault is a one-shot crash/tear the pager simulates at a chosen commit point (storage.md §7).
// Testing only. Not a §8 byte contract: like the buffer pool (pager.md §3), the seam is per-core
// internal machinery; the cross-core contract is the recovery outcome.
export interface CommitFault {
  point: FaultPoint;
  // For body_write/sync: the 1-based ordinal (nth body-page write / nth sync since arming). Ignored
  // for meta_write.
  n?: number;
  // For a write point: the count of leading page bytes to write before failing (a torn page); omitted
  // or negative means write nothing (a clean crash before the page lands). Ignored for sync.
  tearBytes?: number;
}

// injectedCrash is the error an armed fault throws to abort persistImpl mid-commit — a simulated
// crash, reported as an ordinary I/O failure so the commit path rolls back exactly as a real write
// error would (spec/design/storage.md §7). Testing only.
function injectedCrash(): EngineError {
  return engineError("io_error", "injected commit crash (fault injection)");
}

// PageTransform is the page-codec seam (ts-runtime.md §11.5): it sits above the byte device and below
// the page cache (hosts.md §6), transforming each page as it crosses the seam, keyed by page index.
// decode returns exactly the logical page bytes encode received, so page CRCs, manifest digests, and
// the cache all see plaintext pages (validated-cow.md §5). A transform may offer synchronous forms,
// asynchronous forms, or both; one with an async form puts the pager's write side in the asynchronous
// mode. Only an identity transform exists, in tests: no codec or format is selected (encryption.md).
export interface PageTransform {
  encode?(index: number, page: Uint8Array): Uint8Array;
  decode?(index: number, stored: Uint8Array): Uint8Array;
  encodeAsync?(index: number, page: Uint8Array): Promise<Uint8Array>;
  decodeAsync?(index: number, stored: Uint8Array): Promise<Uint8Array>;
}

// PageRead is one page of a read window (commit_manifest.ts ManifestPageRead).
export type PageRead = ManifestPageRead;
export { PAGE_IO_WINDOW };

// deviceWritesAsync reports whether a byte device offers an asynchronous write-side operation, which
// puts its pager's write side in the asynchronous mode (ts-runtime.md §11.4).
function deviceWritesAsync(store: BlockStore): boolean {
  return (
    store.writeAtAsync !== undefined ||
    store.syncAsync !== undefined ||
    store.setSizeAsync !== undefined
  );
}

function syncCodecMissing(): EngineError {
  return engineError(
    "feature_not_supported",
    "this page codec is asynchronous; use the asynchronous runtime",
  );
}

// BatchState is one reading of a BatchMirror: its generation, the floor's slot and txid, and the
// pending set in page order (null when it outgrew the mirror).
export type BatchState = {
  generation: number;
  floorSlot: number;
  floorTxid: bigint;
  entries: ManifestEntry[] | null;
};

// BatchMirror is where a page-service owner's batched-durability facts are mirrored for its service
// (spec/design/validated-cow.md §4, page-service.md §6): the floor and the pending set. The service
// flushes a pending batch before granting a synchronous client's begin_write, names it to a relaxed
// one, and records each client commit — moving the floor, or adding a relaxed commit's pages to the
// pending set; the pager reconciles with the mirror, when its generation changed, before it reads its
// own batch state. Under a mirror the service, not the meta a reload selects, decides where an adopted
// client commit stands in the batch.
export interface BatchMirror {
  batchGeneration(): number;
  batchState(): BatchState;
  // publishBatch replaces the mirrored batch, returning its new generation.
  publishBatch(floorSlot: number, floorTxid: bigint, entries: ManifestEntry[]): number;
  // batchFlushed records a flush: the newest meta (newest, or a newer published one) is the floor.
  batchFlushed(newest: bigint): void;
}

// A block device: fixed-size pages addressed by index, over a BlockStore host kept open for the
// handle's life. One page at a time (storage.md §2); the demand-paging buffer pool (P6.4b) faults
// pages in through readBlock.
export class Pager {
  private store: BlockStore;
  pageSize: number;
  // allocatedPages is the number of pages physically allocated on disk — the file length in pages,
  // which the chunked preallocation (reserve) runs ahead of the committed high-water. A commit whose
  // pages all fall below this never grows the file (storage.md §9). Distinct from the committed
  // logical pageCount the meta records: the slack pages in [pageCount, allocatedPages) are
  // unreferenced trailing zeros (no byte-contract impact — past the high-water).
  private allocatedPages: number;
  // fault is the armed one-shot commit fault — the fault-injection seam (spec/design/storage.md §7),
  // null unless a test armed one with armFault. Production never arms one, so the checks in
  // writeBlock/sync are a single null branch. bodyWrites/syncs count body-page writes (index ≥ 2) and
  // sync() calls since arming, driving "body_write" / "sync".
  private fault: CommitFault | null = null;
  private bodyWrites = 0;
  private syncs = 0;
  private durableCommitActive = false;
  // poisoned refuses every later commit: the storage this pager reads is gone or unknown (a failed
  // compaction after its rename, api.md §2.6), so the handle must be closed and reopened.
  private poisoned = false;
  private adoptedNeedsSync = false;
  private acknowledgedMeta = "";
  private validatedMeta: Uint8Array | null = null;
  private validatedPages: Set<number> | null = null;
  // trustMeta is set on a page client's pager: the owner validated the meta it serves (or wrote it), so
  // the client adopts it without re-reading its manifest pages (spec/design/page-service.md §5.3).
  trustMeta = false;
  // Batched durability (spec/design/validated-cow.md §4). floorSlot is the meta slot holding the
  // newest durable meta; every file commit writes the other one. pending is the CRC-64 of every body
  // page (never a meta or manifest page) written by a commit after the floor — what a pending commit's
  // manifest lists besides its own pages. Empty under synchronous = on. publishedMeta is the identity
  // of the last meta this pager wrote or selected, so a selection of a different one is an adoption;
  // publishedTxid is its txid, and floorTxid the floor's. mirrorGeneration is the mirror's generation
  // this pager last read or wrote.
  private floorSlot = 1;
  private floorTxid = 0n;
  private pending = new Map<number, bigint>();
  private publishedMeta = "";
  private publishedTxid = 0n;
  private batchMirror: BatchMirror | null = null;
  private mirrorGeneration = Number.NaN;
  // shrinkPending is set when a commit of the current batch lowered the high-water (format.md *Trailing
  // truncation*) and the file has not been shortened yet: the shrink waits for the barrier that makes
  // the newest generation durable. shrinkPages is that newest generation's high-water.
  private shrinkPending = false;
  private shrinkPages = 0;
  // transform is the page codec (null: pages cross the seam unchanged). writeAsync is the write-side
  // mode (ts-runtime.md §11.4): whether the transform or device offers an asynchronous operation. When
  // false every write-side step runs inline, exactly as the synchronous path.
  private transform: PageTransform | null = null;
  private writeAsync = false;
  // encrypted is set when the file is unlocked or created as the encrypted variant (setEncrypted).
  private encrypted = false;

  private constructor(store: BlockStore, pageSize: number, allocatedPages: number) {
    this.store = store;
    this.pageSize = pageSize;
    this.allocatedPages = allocatedPages;
    this.refreshMode();
  }

  // setPageTransform installs the page codec (null removes it). Set before any page is read through
  // it: the bootstrap header fromStore reads is the only page that bypasses the transform.
  setPageTransform(transform: PageTransform | null): void {
    this.transform = transform;
    this.refreshMode();
  }

  // writeSideAsync reports the write-side mode (ts-runtime.md §11.4).
  writeSideAsync(): boolean {
    return this.writeAsync;
  }

  private refreshMode(): void {
    const t = this.transform;
    this.writeAsync =
      (t !== null && (t.encodeAsync !== undefined || t.decodeAsync !== undefined)) ||
      deviceWritesAsync(this.store);
  }

  // armFault arms a one-shot commit fault (the fault-injection seam, spec/design/storage.md §7) and
  // resets the since-arm counters, so the next commit's body-write / meta-write / sync sequence
  // triggers it. The fault auto-disarms when it fires. Testing only.
  armFault(fault: CommitFault): void {
    this.fault = fault;
    this.bodyWrites = 0;
    this.syncs = 0;
  }

  // faultOnWrite is the fault-injection seam's write hook (storage.md §7): null-fault → no-op. If an
  // armed fault targets this write it optionally performs a torn partial write, then disarms and
  // throws an injected-crash error so persistImpl aborts mid-commit.
  private faultOnWrite(index: number, bytes: Uint8Array): void {
    const f = this.fault;
    if (f === null) return;
    let hit = false;
    if (f.point === "meta_write") {
      hit = index < 2;
    } else if (f.point === "body_write" && index >= 2) {
      this.bodyWrites++;
      hit = this.bodyWrites === f.n;
    }
    if (!hit) return;
    this.fault = null; // one-shot
    const tear = f.tearBytes ?? -1;
    if (tear >= 0) {
      const k = Math.min(tear, bytes.length);
      this.store.writeAt(index * this.pageSize, bytes.subarray(0, k));
    }
    throw injectedCrash();
  }

  private encodeSync(index: number, page: Uint8Array): Uint8Array {
    const t = this.transform;
    if (t === null) return page;
    if (t.encode === undefined) throw syncCodecMissing();
    return t.encode(index, page);
  }

  private decodeSync(index: number, stored: Uint8Array): Uint8Array {
    const t = this.transform;
    if (t === null) return stored;
    if (t.decode === undefined) throw syncCodecMissing();
    return t.decode(index, stored);
  }

  // decodeSteps decodes one stored page through the transform, awaiting its async form when offered.
  private *decodeSteps(index: number, stored: Uint8Array): Steps<Uint8Array> {
    const t = this.transform!;
    if (t.decodeAsync === undefined) return this.decodeSync(index, stored);
    const decode = t.decodeAsync.bind(t);
    return yield* ioSteps(
      () => this.decodeSync(index, stored),
      () => decode(index, stored),
    );
  }

  // fromStore adopts an already-open store as the byte backing, reading the page size from its meta
  // header (offset 8, format.md). The host layer (file.ts) opens the host — mapping a missing path to
  // 58P01 — and hands it here wrapped in a BlockStore. A store smaller than a meta header, or a zero
  // page size, is XX001.
  static fromStore(store: BlockStore): Pager {
    return runSync(Pager.fromStoreSteps(store));
  }

  // fromStoreSteps is fromStore with an awaitable header read on a device in the asynchronous mode.
  static *fromStoreSteps(store: BlockStore): Steps<Pager> {
    const size = store.size();
    if (size < 12) {
      throw engineError("data_corrupted", "database file smaller than a meta header");
    }
    const header = yield* deviceSteps(
      () => store.readAt(0, 12),
      deviceWritesAsync(store) && store.readAtAsync !== undefined
        ? () => store.readAtAsync!(0, 12)
        : undefined,
    );
    const pageSize = new DataView(header.buffer, header.byteOffset, header.byteLength).getUint32(
      8,
      false,
    );
    if (pageSize < 256 || pageSize > 65536 || (pageSize & (pageSize - 1)) !== 0) {
      throw engineError("data_corrupted", "zero page size in meta header");
    }
    // The allocation high-water is the current file length in pages — already past the committed
    // pageCount if a prior session preallocated slack (reused for free on this session's growth).
    const allocatedPages = Math.floor(size / pageSize);
    return new Pager(store, pageSize, allocatedPages);
  }

  // readBlock reads one page (block index) — random access, the demand-paging read path (P6.4b).
  // Converts the page index to a byte offset for the host's readAt.
  readBlock(index: number): Uint8Array {
    return runSync(this.readBlockSteps(index));
  }

  *readBlockSteps(index: number): Steps<Uint8Array> {
    const store = this.store;
    const offset = index * this.pageSize;
    const length = this.pageSize;
    const stored = yield* ioSteps(
      () => store.readAt(offset, length),
      store.readAtAsync === undefined ? undefined : () => store.readAtAsync!(offset, length),
    );
    if (this.transform === null) return stored;
    return yield* this.decodeSteps(index, stored);
  }

  // readBlocks reads several pages in one store request when the store batches reads (a page client,
  // spec/design/page-service.md §5.3), page by page otherwise.
  readBlocks(indices: number[]): Uint8Array[] {
    const store = this.store;
    if (store.batchesReads?.() !== true || store.readPages === undefined) {
      return indices.map((index) => this.readBlock(index));
    }
    return store.readPages(indices).map((stored, i) => this.decodeSync(indices[i]!, stored));
  }

  // batchesReads reports whether the store fetches several pages in one request.
  batchesReads(): boolean {
    return this.store.batchesReads?.() === true;
  }

  // readPageSteps is the write side's page read (commit, recovery, maintenance — ts-runtime.md §11):
  // inline in the synchronous mode, otherwise each stage awaits its asynchronous form when offered.
  // Unlike a query's data read it never adds a task yield before a synchronous device read.
  *readPageSteps(index: number): Steps<Uint8Array> {
    const store = this.store;
    const offset = index * this.pageSize;
    const length = this.pageSize;
    if (!this.writeAsync) return this.decodeSync(index, store.readAt(offset, length));
    const stored = yield* deviceSteps(
      () => store.readAt(offset, length),
      store.readAtAsync === undefined ? undefined : () => store.readAtAsync!(offset, length),
    );
    if (this.transform === null) return stored;
    return yield* this.decodeSteps(index, stored);
  }

  // readWindowSteps reads the pages indices names concurrently, one window of read-ahead for recovery
  // (ts-runtime.md §11.3), capturing each page's error instead of throwing it. Null in the synchronous
  // mode, where the caller reads lazily one page at a time.
  *readWindowSteps(indices: number[]): Steps<PageRead[] | null> {
    if (!this.writeAsync) return null;
    const store = this.store;
    const t = this.transform;
    const ps = this.pageSize;
    const one = async (index: number): Promise<Uint8Array> => {
      const offset = index * ps;
      const stored =
        store.readAtAsync === undefined
          ? store.readAt(offset, ps)
          : await store.readAtAsync(offset, ps);
      if (t === null) return stored;
      return t.decodeAsync === undefined
        ? this.decodeSync(index, stored)
        : await t.decodeAsync(index, stored);
    };
    const settle = (index: number): PageRead => {
      try {
        return { ok: true, bytes: runSync(this.readPageSteps(index)) };
      } catch (error) {
        return { ok: false, error };
      }
    };
    return yield* ioSteps<PageRead[]>(
      () => indices.map(settle),
      () =>
        Promise.all(
          indices.map((index) =>
            one(index).then(
              (bytes): PageRead => ({ ok: true, bytes }),
              (error: unknown): PageRead => ({ ok: false, error }),
            ),
          ),
        ),
    );
  }

  // writeBlock writes one page (bytes) at block index. Overwrites in place — persistImpl always
  // reserves the high-water first, so the target is already-allocated space (a reused free page, or a
  // preallocated slot past the old high-water). bytes is one page wide.
  writeBlock(index: number, bytes: Uint8Array): void {
    const stored = this.encodeSync(index, bytes);
    this.faultOnWrite(index, stored);
    this.store.writeAt(index * this.pageSize, stored);
  }

  // writePagesSteps writes pages in order — the commit's write sequence (validated-cow.md §1). In the
  // asynchronous mode it encodes one bounded window at a time (ts-runtime.md §11.3), then issues that
  // window's writes strictly in order, each awaited before the next. afterWrite runs once a page's
  // write completed (the page-cache invalidation).
  *writePagesSteps(
    pages: { index: number; bytes: Uint8Array }[],
    afterWrite: (index: number) => void,
  ): Steps<void> {
    if (!this.writeAsync) {
      for (const pg of pages) {
        this.writeBlock(pg.index, pg.bytes);
        afterWrite(pg.index);
      }
      return;
    }
    const store = this.store;
    for (let start = 0; start < pages.length; start += PAGE_IO_WINDOW) {
      const window = pages.slice(start, start + PAGE_IO_WINDOW);
      const encoded = yield* this.encodeWindowSteps(window);
      for (let k = 0; k < window.length; k++) {
        const index = window[k]!.index;
        const stored = encoded[k]!;
        this.faultOnWrite(index, stored);
        const offset = index * this.pageSize;
        yield* deviceSteps(
          () => store.writeAt(offset, stored),
          store.writeAtAsync === undefined ? undefined : () => store.writeAtAsync!(offset, stored),
        );
        afterWrite(index);
      }
    }
  }

  private *encodeWindowSteps(window: { index: number; bytes: Uint8Array }[]): Steps<Uint8Array[]> {
    const t = this.transform;
    if (t === null) return window.map((pg) => pg.bytes);
    if (t.encodeAsync === undefined) return window.map((pg) => this.encodeSync(pg.index, pg.bytes));
    const encode = t.encodeAsync.bind(t);
    return yield* ioSteps(
      () => window.map((pg) => this.encodeSync(pg.index, pg.bytes)),
      () => Promise.all(window.map((pg) => encode(pg.index, pg.bytes))),
    );
  }

  // reserve ensures the file has at least minPages physically-allocated pages, growing it GEOMETRICALLY
  // when short: each step adds the current size (≈doubling), floored at preallocFloorPages and capped at
  // preallocChunkPages (1 MiB). So a small database's file stays proportional to its data (no fixed
  // 1 MiB minimum) while a large one still grows in 1 MiB chunks — the physical size stays bounded by
  // ≈2× the committed high-water. persistImpl calls it before each commit's body write with the new
  // committed high-water, so that write — and almost every commit's — lands entirely in
  // already-allocated space and its data-only sync (Pager.sync) pays no metadata journaling
  // (spec/design/pager.md §7). The preallocation policy is host-independent and stays here; the durable
  // grow itself — real zero blocks + a full fsync — is the host's setSize, the metadata barrier
  // (hosts.md §2.1/§3). Crash-safe: the preallocated pages are unreferenced zeros past the committed
  // pageCount, so a crash before the next commit publishes simply ignores them.
  reserve(minPages: number): void {
    runSync(this.reserveSteps(minPages));
  }

  *reserveSteps(minPages: number): Steps<void> {
    if (minPages <= this.allocatedPages) return;
    const floor = preallocFloorPages(this.pageSize);
    const chunk = preallocChunkPages(this.pageSize);
    let target = this.allocatedPages;
    while (target < minPages) {
      target += Math.min(chunk, Math.max(floor, target));
      if (target > 0xffff_ffff) {
        target = 0xffff_ffff; // saturate rather than exceed the u32 page ceiling (matches Rust/Go)
        break;
      }
    }
    yield* this.setSizeSteps(target * this.pageSize);
    this.allocatedPages = target;
  }

  private *setSizeSteps(bytes: number): Steps<void> {
    const store = this.store;
    yield* deviceSteps(
      () => store.setSize(bytes),
      this.writeAsync && store.setSizeAsync !== undefined
        ? () => store.setSizeAsync!(bytes)
        : undefined,
    );
  }

  // trimSlack shortens the file after a commit lowered the committed high-water to pageCount (format.md
  // *Trailing truncation*): a file longer than pageCount plus one growth step — the step reserve would
  // add growing from pageCount — is cut to exactly that, so the next growth after a shrink is already
  // allocated (pager.md §7). Best effort: the commit is already durable, so a failed shrink leaves the
  // file longer and the allocation high-water unchanged, which is harmless. Shrinking needs no barrier
  // (hosts.md §2.1).
  trimSlack(pageCount: number): void {
    runSync(this.trimSlackSteps(pageCount));
  }

  *trimSlackSteps(pageCount: number): Steps<void> {
    const target = Math.min(0xffff_ffff, pageCount + growthStep(pageCount, this.pageSize));
    if (this.allocatedPages <= target) return;
    try {
      yield* this.setSizeSteps(target * this.pageSize);
      this.allocatedPages = target;
    } catch {
      // A failed shrink leaves the file longer; nothing depends on it.
    }
  }

  // Refresh the physical high-water after another process's commit grew the file (a page-service
  // owner adopting a client commit, or a client adopting the owner's meta): the pager's allocation
  // cache must learn the file's current length so it never tries to regrow from a stale value.
  refreshAllocatedPages(): void {
    this.allocatedPages = Math.floor(this.store.size() / this.pageSize);
  }

  // sync is the host durability barrier. Validated COW uses one final sync for body, descriptor,
  // and meta together (format.md). A recovered generation also syncs once before its first write.
  // The native host uses its data-integrity primitive so an overwrite into the
  // preallocated region (reserve) flushes only the data, never a file-size/inode-timestamp metadata
  // journal (spec/design/pager.md §7).
  sync(): void {
    runSync(this.syncSteps());
  }

  *syncSteps(): Steps<void> {
    if (this.fault !== null && this.fault.point === "sync") {
      this.syncs++;
      if (this.syncs === this.fault.n) {
        this.fault = null; // one-shot
        throw injectedCrash();
      }
    }
    const store = this.store;
    yield* deviceSteps(
      () => store.sync(),
      this.writeAsync && store.syncAsync !== undefined ? () => store.syncAsync!() : undefined,
    );
  }

  // A recovered descriptor can represent a complete but unacknowledged transaction. Make that
  // generation durable before any successor overwrites its fallback or relies on inherited pages.
  // slot is the selected meta's slot and bothSlots whether the other slot carries the same txid: an
  // adopted generation is the floor once stabilized, in slot txid & 1 when both slots carry it
  // (create, compaction), and nothing is pending (validated-cow.md §3–§4).
  adoptCommit(
    txid: bigint,
    checksum: number,
    needsSync: boolean,
    slot: number,
    bothSlots: boolean,
  ): void {
    const identity = `${txid}:${checksum}`;
    if (identity !== this.acknowledgedMeta) this.adoptedNeedsSync = needsSync;
    if (identity !== this.publishedMeta) {
      this.publishedMeta = identity;
      this.publishedTxid = txid;
      if (this.batchMirror !== null) {
        // A page-service owner adopting its clients' commits: the service recorded where each stands
        // in the batch (durable, or pending under a relaxed client), so the mirror decides.
        this.reconcileBatch();
        return;
      }
      this.floorSlot = bothSlots ? Number(txid & 1n) : slot;
      this.floorTxid = txid;
      this.pending.clear();
    }
  }

  // --- batched durability (spec/design/validated-cow.md §4) ---

  // setBatchMirror mirrors the batch state into a page-service owner's shared state (null: none).
  setBatchMirror(mirror: BatchMirror | null): void {
    this.batchMirror = mirror;
    this.publishBatch();
  }

  // reconcileBatch adopts what the service did meanwhile: a flush before a client begin_write empties
  // the pending set, a synchronous client commit moves the floor, and a relaxed one adds its pages to
  // the pending set. An overflowed mirror (only ever while a commit that outgrew it awaits its flush)
  // leaves this pager's own entries in place.
  private reconcileBatch(): void {
    const mirror = this.batchMirror;
    if (mirror === null || mirror.batchGeneration() === this.mirrorGeneration) return;
    const state = mirror.batchState();
    this.mirrorGeneration = state.generation;
    this.floorSlot = state.floorSlot;
    this.floorTxid = state.floorTxid;
    if (state.entries !== null) {
      this.pending = new Map(state.entries.map((e) => [e.index, e.checksum]));
    }
  }

  private publishBatch(): void {
    const mirror = this.batchMirror;
    if (mirror === null) return;
    this.mirrorGeneration = mirror.publishBatch(
      this.floorSlot,
      this.floorTxid,
      this.sortedPending(),
    );
  }

  private sortedPending(): ManifestEntry[] {
    return [...this.pending]
      .map(([index, checksum]) => ({ index, checksum }))
      .sort((a, b) => a.index - b.index);
  }

  // setFloorSlot sets the floor of a freshly created file (both slots carry the bootstrap txid).
  setFloorSlot(slot: number, txid: bigint): void {
    this.floorSlot = slot;
    this.floorTxid = txid;
    this.publishedTxid = txid;
    this.publishBatch();
  }

  // floorMetaSlot is the slot holding the floor's meta; floorMetaTxid is its txid.
  floorMetaSlot(): number {
    this.reconcileBatch();
    return this.floorSlot;
  }

  floorMetaTxid(): bigint {
    this.reconcileBatch();
    return this.floorTxid;
  }

  // commitSlot is the meta slot the next commit writes: the one not holding the floor (format.md, *Slot
  // alternation*). Under synchronous = on this is txid & 1.
  commitSlot(): number {
    this.reconcileBatch();
    return this.floorSlot ^ 1;
  }

  // hasPending reports whether a relaxed batch is pending a flush.
  hasPending(): boolean {
    this.reconcileBatch();
    return this.pending.size > 0;
  }

  // pendingEntries is the pending set in page order.
  pendingEntries(): ManifestEntry[] {
    this.reconcileBatch();
    return this.sortedPending();
  }

  // recordCommit records a commit whose meta (txid, checksum) was written to slot. durable is whether
  // its barrier ran: the meta is then the floor and nothing is pending. A relaxed commit instead adds
  // its own dirty entries to the pending set.
  recordCommit(
    txid: bigint,
    checksum: number,
    slot: number,
    durable: boolean,
    dirty: ManifestEntry[],
  ): void {
    this.reconcileBatch();
    this.publishedMeta = `${txid}:${checksum}`;
    this.publishedTxid = txid;
    if (durable) {
      this.floorSlot = slot;
      this.floorTxid = txid;
      this.pending.clear();
    } else {
      for (const e of dirty) this.pending.set(e.index, e.checksum);
    }
    this.publishBatch();
  }

  // batchCapReached reports whether the pending set fills the inline manifest of one meta page: the
  // batch cap, after which a flush follows.
  batchCapReached(): boolean {
    this.reconcileBatch();
    return this.pending.size >= manifestInlineCapacity(this.pageSize, this.encrypted);
  }

  // setEncrypted marks the file as the encrypted variant (spec/fileformat/encrypted.md): its meta's
  // inline manifest holds fewer entries, which moves the relaxed batch cap.
  setEncrypted(): void {
    this.encrypted = true;
  }

  // flushPending is a flush: one durable barrier makes every pending commit durable, so the newest
  // meta — in the non-floor slot — becomes the floor. A failed barrier refuses every later commit.
  flushPending(): void {
    runSync(this.flushPendingSteps());
  }

  *flushPendingSteps(): Steps<void> {
    this.reconcileBatch();
    if (this.pending.size === 0) return;
    if (this.commitRequiresReopen()) {
      throw engineError("io_error", "database writer requires reopen after failed commit");
    }
    try {
      yield* this.syncSteps();
    } catch (e) {
      this.poisoned = true;
      throw e;
    }
    if (this.batchMirror !== null) {
      // The newest meta may be a client commit this pager has not adopted yet: the mirror knows.
      this.batchMirror.batchFlushed(this.publishedTxid);
      this.reconcileBatch();
    } else {
      this.floorSlot ^= 1;
      this.floorTxid = this.publishedTxid;
      this.pending.clear();
    }
    yield* this.shrinkAfterBarrierSteps();
  }

  // settleTruncation records the high-water pageCount a commit published. lowered is whether the commit
  // lowered it (trailing truncation); durable is whether the commit's barrier ran. The file is shortened
  // only after a barrier that covers the newest generation: at once for a durable commit, at the next
  // flush for a relaxed one (validated-cow.md §4), so a shrink never reaches the disk while a batch is
  // pending and the floor still needs the pages above the new high-water. A page service's begin_write
  // flush (the worker's, outside this pager) shrinks nothing: the shrink waits for the owner's next
  // barrier, as in every core.
  settleTruncation(pageCount: number, lowered: boolean, durable: boolean): void {
    runSync(this.settleTruncationSteps(pageCount, lowered, durable));
  }

  *settleTruncationSteps(pageCount: number, lowered: boolean, durable: boolean): Steps<void> {
    this.shrinkPending ||= lowered;
    this.shrinkPages = pageCount;
    if (durable) yield* this.shrinkAfterBarrierSteps();
  }

  // shrinkAfterBarrierSteps shortens the file to the newest generation's high-water plus one growth
  // step when a commit since the last barrier lowered it. Called right after a successful barrier.
  private *shrinkAfterBarrierSteps(): Steps<void> {
    if (!this.shrinkPending) return;
    this.shrinkPending = false;
    yield* this.trimSlackSteps(this.shrinkPages);
  }

  // batchFlushed reports whether nothing awaits a barrier: no pending set, no adopted generation awaiting
  // stabilization, no failed commit (the simulator's flush oracle).
  batchFlushed(): boolean {
    this.reconcileBatch();
    return this.pending.size === 0 && !this.adoptedNeedsSync && !this.commitRequiresReopen();
  }

  // revoke makes every later access fail with error: the storage can no longer be trusted (a failed
  // deferred flush may have dropped pending pages from the OS cache). Pages already in the pool still
  // serve the snapshots that cached them.
  revoke(error: Error): void {
    try {
      this.store.close();
    } catch {
      // The descriptor is released either way.
    }
    this.store = new FailingBlockStore(error);
    this.poisoned = true;
    this.refreshMode();
  }

  // Check before serialization can fault inherited pages; an encoding error leaves
  // the storage handle usable because no durable commit has begun.
  checkDurableCommit(): void {
    if (this.commitRequiresReopen()) {
      throw engineError("io_error", "database has an incomplete durable commit; close and reopen");
    }
  }

  commitRequiresReopen(): boolean {
    return this.poisoned || this.durableCommitActive;
  }

  // swapStore replaces the byte backing and returns the old one (compaction, spec/design/api.md §2.6):
  // the caller closes a file before renaming its replacement over it by swapping in a ClosedBlockStore,
  // and swaps the reopened old file back if the rename fails.
  swapStore(store: BlockStore): BlockStore {
    const old = this.store;
    this.store = store;
    this.refreshMode();
    return old;
  }

  // migrateStore moves the store's bytes into store and adopts it, closing the old backing — a
  // session-local temp domain spilling from its in-RAM store to a scratch file (spec/design/
  // temp-tables.md §6). The page layout is unchanged byte for byte, so every page id stays valid.
  // Copies in bounded chunks; on an error the old store stays in place, untouched, and the new one is
  // closed (JS has no destructor to release it later).
  migrateStore(store: BlockStore): void {
    const CHUNK = 1 << 20;
    try {
      const size = this.store.size();
      for (let offset = 0; offset < size; ) {
        const len = Math.min(size - offset, CHUNK);
        store.writeAt(offset, this.store.readAt(offset, len));
        offset += len;
      }
    } catch (e) {
      store.close();
      throw e;
    }
    const old = this.store;
    this.store = store;
    this.refreshMode();
    old.close();
  }

  // skipsSync reports whether the backing skips its durability barrier (fsync=off).
  skipsSync(): boolean {
    return this.store.skipsSync?.() ?? false;
  }

  // poison refuses every later commit: the storage this pager reads is gone or unknown, so the handle
  // must be closed and reopened (a failed compaction after its rename, api.md §2.6).
  poison(): void {
    this.poisoned = true;
  }

  // Begin after serialization succeeds, before any reserve/write or recovery sync.
  beginDurableCommit(): void {
    runSync(this.beginDurableCommitSteps());
  }

  // beginDurableCommitSteps arms the commit guard, then stabilizes an adopted generation (an awaited
  // barrier in the asynchronous mode): a failure from here on keeps the guard armed until reopen.
  *beginDurableCommitSteps(): Steps<void> {
    this.checkDurableCommit();
    this.durableCommitActive = true;
    this.validatedMeta = null;
    this.validatedPages = null;
    if (this.adoptedNeedsSync) {
      yield* this.syncSteps();
      this.adoptedNeedsSync = false;
    }
  }

  // abandonDurableCommit ends a commit whose meta never reached the storage: a page client's commit
  // the owner certainly did not commit (spec/pageservice/README.md §6.4). The pages it buffered are
  // dropped, so the handle stays usable.
  abandonDurableCommit(): void {
    this.durableCommitActive = false;
  }

  finishDurableCommit(txid: bigint, checksum: number): void {
    this.acknowledgedMeta = `${txid}:${checksum}`;
    this.adoptedNeedsSync = false;
    this.durableCommitActive = false;
  }

  cachedValidation(meta: Uint8Array): Set<number> | null {
    if (this.trustMeta) return new Set();
    const cached = this.validatedMeta;
    if (cached === null || cached.length !== meta.length) return null;
    for (let i = 0; i < meta.length; i++) if (cached[i] !== meta[i]) return null;
    return this.validatedPages;
  }

  cacheValidation(meta: Uint8Array, pages: Set<number>): void {
    this.validatedMeta = meta.slice();
    this.validatedPages = pages;
  }

  physicalPages(): number {
    return Math.floor(this.store.size() / this.pageSize);
  }

  // close releases the backing store (close()).
  close(): void {
    this.store.close();
  }
}
