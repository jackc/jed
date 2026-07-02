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

import type { BlockStore } from "./blockstore.ts";
import { type EngineError, engineError } from "./errors.ts";

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

  private constructor(store: BlockStore, pageSize: number, allocatedPages: number) {
    this.store = store;
    this.pageSize = pageSize;
    this.allocatedPages = allocatedPages;
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

  // fromStore adopts an already-open store as the byte backing, reading the page size from its meta
  // header (offset 8, format.md). The host layer (file.ts) opens the host — mapping a missing path to
  // 58P01 — and hands it here wrapped in a BlockStore. A store smaller than a meta header, or a zero
  // page size, is XX001.
  static fromStore(store: BlockStore): Pager {
    const size = store.size();
    if (size < 12) {
      throw engineError("data_corrupted", "database file smaller than a meta header");
    }
    const header = store.readAt(0, 12);
    const pageSize = new DataView(header.buffer, header.byteOffset, header.byteLength).getUint32(
      8,
      false,
    );
    if (pageSize === 0) {
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
    return this.store.readAt(index * this.pageSize, this.pageSize);
  }

  // writeBlock writes one page (bytes) at block index. Overwrites in place — persistImpl always
  // reserves the high-water first, so the target is already-allocated space (a reused free page, or a
  // preallocated slot past the old high-water). bytes is one page wide.
  writeBlock(index: number, bytes: Uint8Array): void {
    this.faultOnWrite(index, bytes);
    this.store.writeAt(index * this.pageSize, bytes);
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
    this.store.setSize(target * this.pageSize);
    this.allocatedPages = target;
  }

  // sync is the metadata-free durability barrier — the host's data-only sync (fdatasync). Called twice
  // per commit — body pages, then the meta — to honour the body-before-meta write-ordering rule
  // (format.md, file.ts persistImpl). Data-only (not a full fsync) so an overwrite into the
  // preallocated region (reserve) flushes only the data, never a file-size/inode-timestamp metadata
  // journal (spec/design/pager.md §7).
  sync(): void {
    if (this.fault !== null && this.fault.point === "sync") {
      this.syncs++;
      if (this.syncs === this.fault.n) {
        this.fault = null; // one-shot
        throw injectedCrash();
      }
    }
    this.store.sync();
  }

  // close releases the backing store (close()).
  close(): void {
    this.store.close();
  }
}
