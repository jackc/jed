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
  buildCommitManifest,
  manifestInlineCapacity,
  manifestOverflowCapacity,
  metaChecksum,
} from "./commit_manifest.ts";
import { engineError } from "./errors.ts";
import {
  incrementalImage,
  type IncrementalWrite,
  metaPage,
  planFreeList,
  reachablePages,
  ROOT_PAGE,
} from "./format.ts";

// Durable stores write body, free-list, descriptor, and alternate meta, then synchronize once.
// Recovery checks every descriptor dependency before selecting that meta. The committed free list
// excludes all descriptor dependencies, preserving the previous candidate during the next commit.
// Memory stores retain their RAM free list and post-commit compaction without descriptor overhead.
// canReclaim is the caller's reader-watermark decision (default: no open streaming cursor).
export function persistImpl(
  db: Engine,
  snap: Snapshot,
  canReclaim?: boolean,
  canReuse = true,
): IncrementalWrite {
  if (db.paging !== null && db.persistHook !== null) db.paging.beginDurableCommit();
  const write = incrementalImage(
    snap,
    db.pageSize,
    db.pageCount,
    db.freePages,
    db.paging,
    canReuse,
  );
  if (db.paging === null) return write; // a bare engine with no byte store: nothing to write
  const reclaim = canReclaim ?? db.openStreams === 0;
  // DURABLE (file or OPFS — both reopened, both set a persistHook; the OPFS host leaves `path` null, so
  // durability is keyed on persistHook, not path) persists the free-list in-commit; an IN-MEMORY main
  // store (persistHook null) keeps its free-list in RAM.
  if (db.persistHook !== null) commitFile(db, snap, write, reclaim, canReuse);
  else commitInMemory(db, snap, write, reclaim);
  return write;
}

// Shared-process commit is deliberately append-only (locking.md §5.3): it never consumes the
// persisted free list, rewrites its chain, truncates, or replaces the file. Body and overflow
// descriptor writes happen before commit EX; meta publication and the single durability barrier
// stay inside commit EX so another process cannot adopt an unacknowledged live writer's generation.
export function persistSharedBody(
  db: Engine,
  snap: Snapshot,
): {
  write: IncrementalWrite;
  publishMeta: () => void;
} {
  const paging = db.paging;
  paging?.beginDurableCommit();
  const write = incrementalImage(snap, db.pageSize, db.pageCount, db.freePages, db.paging, false);
  if (paging === null) return { write, publishMeta: () => {} };
  paging.refreshAllocatedPages();
  const overflowCount = Math.ceil(
    Math.max(0, write.pages.length - manifestInlineCapacity(db.pageSize)) /
      manifestOverflowCapacity(db.pageSize),
  );
  const pageCount = write.pageCount + overflowCount;
  if (pageCount > 0xffff_ffff)
    throw engineError("program_limit_exceeded", "database page limit exceeded");
  const ids = Array.from({ length: overflowCount }, (_, i) => write.pageCount + i);
  const manifest = buildCommitManifest(db.pageSize, snap.txid, write.pages, ids);
  paging.reserve(pageCount);
  for (const pg of [...write.pages, ...manifest.pages]) paging.writeBlock(pg.index, pg.bytes);
  return {
    write,
    publishMeta: () => {
      const meta = metaPage(db.pageSize, snap.txid, write.rootPage, pageCount, 0, manifest);
      paging.writeBlock(Number(snap.txid & 1n), meta);
      paging.sync();
      paging.finishDurableCommit(snap.txid, metaChecksum(meta));
      db.pageCount = pageCount;
      db.freePages = [];
      // Zero is the orphan sentinel: the first later alone commit rebuilds the free list instead of
      // trusting reachability facts from before a co-resident interval.
      db.liveAtCompaction = 0;
      db.freeGenTxid = snap.txid;
    },
  };
}

// Write tree/catalog first so reclamation can read the new catalog. Joint allocation then reserves
// descriptor and free-list pages from the prior safe free set, protecting every new write from reuse.
// The inline/overflow descriptor hashes those body pages and the single final sync publishes them.
function commitFile(
  db: Engine,
  snap: Snapshot,
  write: IncrementalWrite,
  canReclaim: boolean,
  canReuse: boolean,
): void {
  const paging = db.paging!;
  // Preallocate ahead of the high-water so the body sync carries no file-growth journaling (pager.md §7).
  paging.reserve(write.pageCount);
  for (const pg of write.pages) {
    paging.writeBlock(pg.index, pg.bytes);
    paging.invalidate(pg.index);
  }
  const plan = planFreeList(
    snap,
    paging,
    write.rootPage,
    write.pages,
    write.freeRemaining,
    write.pageCount,
    db.liveAtCompaction,
    db.freeGenTxid,
    db.pageSize,
    canReclaim,
    canReuse,
  );
  paging.reserve(plan.newPageCount);
  for (const pg of plan.pages) {
    paging.writeBlock(pg.index, pg.bytes);
    paging.invalidate(pg.index);
  }
  const manifest = buildCommitManifest(
    db.pageSize,
    snap.txid,
    [...write.pages, ...plan.pages],
    plan.manifestIds,
  );
  for (const pg of manifest.pages) {
    paging.writeBlock(pg.index, pg.bytes);
    paging.invalidate(pg.index);
  }
  const meta = metaPage(
    db.pageSize,
    snap.txid,
    write.rootPage,
    plan.newPageCount,
    plan.head,
    manifest,
  );
  paging.writeBlock(Number(snap.txid & 1n), meta);
  paging.sync(); // one durability barrier covers body + descriptor + meta
  paging.finishDurableCommit(snap.txid, metaChecksum(meta));
  db.pageCount = plan.newPageCount;
  db.freePages = plan.persisted;
  db.liveAtCompaction = plan.newLive;
  db.freeGenTxid = plan.newGen;
}

// commitInMemory is the IN-MEMORY branch of persistImpl: a MemoryBlockStore is never reopened, so it
// keeps its free-list in RAM and persists NO page_type 7 pages (writing them would waste memory pages);
// the meta write + sync are no-ops on the store. Within-session reclamation is a POST-commit RAM rebuild
// (maybeCompact).
function commitInMemory(
  db: Engine,
  snap: Snapshot,
  write: IncrementalWrite,
  canReclaim: boolean,
): void {
  const paging = db.paging!;
  paging.reserve(write.pageCount);
  for (const pg of write.pages) {
    paging.writeBlock(pg.index, pg.bytes);
    paging.invalidate(pg.index);
  }
  paging.sync(); // a no-op on a MemoryBlockStore
  const meta = metaPage(db.pageSize, snap.txid, write.rootPage, write.pageCount, 0);
  paging.writeBlock(Number(snap.txid & 1n), meta);
  paging.sync();
  db.pageCount = write.pageCount;
  db.freePages = write.freeRemaining;
  maybeCompact(db, snap, write.rootPage, write.pages, canReclaim);
}

// commitDurableAttachment durably commits a FILE-backed host attachment's working snapshot into its own
// byte store (attached-databases.md §5, Slice 2): the SAME durable recipe as the main persist
// (persistImpl — v25 persists the free-list in-commit for a file store), then the post-commit residency
// flip (demoteCleanLeaves — bplus-reshape.md B4). The caller advances snap.txid before calling. Runs
// under the writer gate. An in-memory attachment uses persistTemp instead (no fsync).
export function commitDurableAttachment(
  db: Engine,
  snap: Snapshot,
  canReclaim: boolean,
  canReuse = true,
): void {
  persistImpl(db, snap, canReclaim, canReuse);
  snap.demoteCleanLeaves();
}

// maybeCompact reclaims within-session copy-on-write orphans IN RAM by rebuilding the free-list from the
// live (reachable) set — the POST-commit form used by never-reopened stores (session temp, in-memory
// attachments, in-memory main), which need no persisted free-list. (A file-backed store instead reclaims
// IN-COMMIT so the reclaimed list is durable — planFreeList.) It is:
//   - a no-op for a non-reclaim domain (reclaimWithinSession false);
//   - deferred while any older version is pinned (canReclaim false): compaction frees pages unreachable
//     from the committed root, which an older reader may still observe, so it waits for the pins to drain;
//   - periodic: it walks (O(pages)) only once the high-water passes ~2× the live count at the last
//     compaction, so pageCount oscillates in [live, 2×live] and the walk is amortized O(height)/commit.
// written is the pages THIS commit wrote — unioned into the live set so a live GiST R-tree (rewritten
// wholesale each commit, invisible to reachablePages) is never freed. canReclaim is the caller's
// watermark decision.
export function maybeCompact(
  db: Engine,
  snap: Snapshot,
  catRoot: number,
  written: { index: number; bytes: Uint8Array }[],
  canReclaim: boolean,
): void {
  if (!db.reclaimWithinSession || !canReclaim || db.paging === null) return;
  const minCompactPages = 16; // don't churn a tiny store
  if (db.pageCount <= minCompactPages || db.pageCount <= 2 * db.liveAtCompaction) return;
  const reached = reachablePages(snap, db.paging, catRoot);
  for (const w of written) reached.add(w.index);
  const free: number[] = [];
  for (let p = ROOT_PAGE; p < db.pageCount; p++) {
    if (!reached.has(p)) free.push(p);
  }
  db.freePages = free;
  db.liveAtCompaction = reached.size;
  db.freeGenTxid = snap.txid; // the recomputed list is proven dead at snap.txid (the §8 reuse gate)
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
  maybeCompact(db, snap, write.rootPage, write.pages, canReclaim);
}
