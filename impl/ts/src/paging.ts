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

import type { ScalarType } from "./types.ts";
import type { PNode } from "./pmap.ts";
import { BufferPool } from "./bufferpool.ts";
import { decodeLeafNode } from "./format.ts";
import { type CommitFault, Pager } from "./pager.ts";

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

  constructor(pager: Pager, capacity: number) {
    this.pager = pager;
    this.pool = new BufferPool(capacity);
  }

  // faultLeaf faults the clean leaf at page to a resident node, through the buffer pool: a hit returns
  // the cached node, a miss reads + decodes the page (with this table's colTypes) and caches it,
  // evicting under CLOCK if full. A page id belongs to exactly one table, so caching by global page id
  // with a caller-supplied decoder is consistent (pager.md §4).
  faultLeaf(page: number, colTypes: ScalarType[]): PNode {
    // Materialize any external value by following its overflow chain through the pager (the leaf
    // block holds only the pointer — spec/design/large-values.md §12).
    // Lazy decode (spec/design/large-values.md §14): an external/compressed value stays an
    // unfetched reference — no chain read, no decompression. The scan layer resolves the
    // columns a query touches through readBlock below.
    return this.pool.getOrLoad(page, () => decodeLeafNode(this.pager.readBlock(page), page, colTypes));
  }

  // readBlock reads one page through the pager — the demand-paged loader reads the meta, catalog, and
  // interior skeleton this way (format.ts loadDatabasePaged).
  readBlock(index: number): Uint8Array {
    return this.pager.readBlock(index);
  }

  // reserve / writeBlock / sync / close drive the commit write path (file.ts persistImpl) and handle
  // release. reserve preallocates file growth in chunks ahead of the high-water (spec/design/pager.md §7).
  reserve(minPages: number): void {
    this.pager.reserve(minPages);
  }

  writeBlock(index: number, bytes: Uint8Array): void {
    this.pager.writeBlock(index, bytes);
  }

  sync(): void {
    this.pager.sync();
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
