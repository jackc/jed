// The incremental copy-on-write commit — the host-INDEPENDENT durable-write recipe (spec/design/
// storage.md §4, spec/fileformat/format.md *Allocation & incremental commit*; transactions.md §9).
// Shared by every storage host (file.ts Node `fs`, opfs.ts Browser/OPFS), because the recipe is
// identical across hosts: the only per-host code is the BlockStore beneath SharedPaging (spec/design/
// hosts.md §3). Installed as a Engine's persistHook by the host bootstrap and called by commitTx with
// the working snapshot being published.
//
// Browser-clean: imports only host-agnostic core, no `node:*`, so it lands in a browser bundle.

import type { Engine, Snapshot } from "./executor.ts";
import {
  incrementalImage,
  type IncrementalWrite,
  metaPage,
  reachablePages,
  ROOT_PAGE,
} from "./format.ts";

// persistImpl durably publishes snap to the backing store via an incremental commit: write the dirty
// pages this transaction introduced — reusing free-list pages a prior root abandoned before extending
// the file (P6.2) — sync, write the alternate meta slot (snap.txid & 1), sync. Clean pages are never
// rewritten. A crash between the two syncs leaves the prior meta — and thus the prior snapshot — intact
// (its pages were not overwritten: a reused free page is reachable from no live snapshot). An in-memory
// database has no paging context — a no-op success (committed swaps in commitTx after this). pageCount /
// freePages advance only after both syncs succeed, so a write failure leaves db, committed, and the
// file's prior meta untouched (the working snapshot is then discarded). The future synchronous=off mode
// gates here.
export function persistImpl(db: Engine, snap: Snapshot): IncrementalWrite {
  const write = incrementalImage(snap, db.pageSize, db.pageCount, db.freePages, db.paging);
  if (db.paging === null) return write; // an in-memory database with no paging: nothing to write
  // Preallocate the file ahead of the high-water in chunks, so this commit's body write — and most
  // later commits' — lands in already-allocated space and the body sync below carries no file-growth
  // metadata journaling on the file hosts (spec/design/pager.md §7). On OPFS, with a single flush()
  // barrier, the preallocation is harmless (hosts.md §5).
  db.paging.reserve(write.pageCount);
  for (const pg of write.pages) {
    db.paging.writeBlock(pg.index, pg.bytes);
    // Drop any stale pool entry for a rewritten page (bufferpool.ts invalidate): a no-op unless a
    // reclaim domain reused a freed page id, in which case the pool's prior decode must be evicted.
    db.paging.invalidate(pg.index);
  }
  db.paging.sync(); // body pages durable before the meta can reference them
  const meta = metaPage(db.pageSize, snap.txid, write.rootPage, write.pageCount);
  db.paging.writeBlock(Number(snap.txid & 1n), meta);
  db.paging.sync(); // the commit is published
  db.pageCount = write.pageCount;
  db.freePages = write.freeRemaining;
  return write;
}

// commitDurableAttachment durably commits a FILE-backed host attachment's working snapshot into its own
// byte store (attached-databases.md §5, Slice 2): the SAME durable recipe as the main persist
// (persistImpl — dirty pages + alternating meta slot + fsync, its own page space), then the post-commit
// residency flip (demoteCleanLeaves — bplus-reshape.md B4) and within-session compaction (a no-op for a
// file domain, whose reclaimWithinSession is false — it reconstructs its free-list on open instead). The
// caller advances snap.txid before calling (the alternating meta slot + reopen). Runs under the writer
// gate (single-writer page accounting). An in-memory attachment uses persistTemp instead (no fsync).
export function commitDurableAttachment(db: Engine, snap: Snapshot, canReclaim: boolean): void {
  const write = persistImpl(db, snap);
  snap.demoteCleanLeaves();
  maybeCompact(db, snap, write.rootPage, canReclaim);
}

// maybeCompact reclaims within-session copy-on-write orphans for a reclaim domain (temp) by rebuilding
// the free-list from the live (reachable) set, so later commits reuse dead pages instead of only growing
// the high-water (temp-tables.md §6, bplus-reshape.md). It is:
//   - a no-op for the main domain (reclaimWithinSession false) — that keeps its reconstruct-on-open list;
//   - deferred while any older version is pinned (canReclaim false): compaction frees pages unreachable
//     from the committed root, which an older reader may still observe, so it waits for the pins to drain
//     (temp-tables.md §6);
//   - periodic: it walks (O(pages)) only once the high-water passes ~2× the live count at the last
//     compaction, so pageCount oscillates in [live, 2×live] and the walk is amortized O(height)/commit.
// canReclaim is the caller's watermark decision — true iff no live reader/cursor pins a version older
// than this commit (so no page unreachable from the committed root can still be observed).
export function maybeCompact(
  db: Engine,
  snap: Snapshot,
  catRoot: number,
  canReclaim: boolean,
): void {
  if (!db.reclaimWithinSession || !canReclaim || db.paging === null) return;
  const minCompactPages = 16; // don't churn a tiny store
  if (db.pageCount <= minCompactPages || db.pageCount <= 2 * db.liveAtCompaction) return;
  const reached = reachablePages(snap, db.paging, catRoot);
  const free: number[] = [];
  for (let p = ROOT_PAGE; p < db.pageCount; p++) {
    if (!reached.has(p)) free.push(p);
  }
  db.freePages = free;
  db.liveAtCompaction = reached.size;
}

// persistTemp materializes a TEMP snapshot's dirty pages into the domain's in-RAM MemoryBlockStore
// (temp-tables.md §6): the SAME incremental copy-on-write serialize as a file/in-memory commit, but with
// NO meta slot and NO sync — a temp domain is never reopened and its memory host has no durability
// barrier — then the residency flip (clean leaves demote to OnDisk, faulted back through the temp pool:
// the compact packed footprint) and within-session compaction (maybeCompact). ZERO main-file writes:
// only the temp byte store is touched, so the zero-file-write invariant (temp-tables.md §2, D1) is
// preserved by construction. Assigns page ids on snap in place; the caller adopts snap as the committed
// temp state afterward. canReclaim is the caller's cursor watermark (no open streaming cursor may hold an
// older temp tree).
export function persistTemp(db: Engine, snap: Snapshot, canReclaim: boolean): void {
  if (db.paging === null) return;
  const write = incrementalImage(snap, db.pageSize, db.pageCount, db.freePages, db.paging);
  db.paging.reserve(write.pageCount);
  for (const pg of write.pages) {
    db.paging.writeBlock(pg.index, pg.bytes);
    // Drop any stale pool entry: within-session compaction may hand this page id back for a new node,
    // and the pool caches by page id (bufferpool.ts invalidate). A no-op for a fresh page.
    db.paging.invalidate(pg.index);
  }
  // No meta write, no sync: never reopened, no durability barrier.
  db.pageCount = write.pageCount;
  db.freePages = write.freeRemaining;
  snap.demoteCleanLeaves();
  maybeCompact(db, snap, write.rootPage, canReclaim);
}
