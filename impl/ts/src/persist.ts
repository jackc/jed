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
import { COMPACT_GROWTH, COMPACT_MIN_PAGES } from "./costs.ts";
import { engineError } from "./errors.ts";
import {
  catalogPageCount,
  incrementalImage,
  type IncrementalWrite,
  metaPage,
  planFreeList,
  reachablePages,
  ROOT_PAGE,
  unassignPages,
} from "./format.ts";

// StorageBudget is the max_storage_bytes state of one storage domain (memory.md §8.1–§8.3): the limit,
// plus what the last successful in-memory commit wrote, so a commit that would exceed the limit can
// first compact the committed snapshot (its catalog root and written pages — the latter cover a GiST
// R-tree, which reachablePages cannot see).
export type StorageBudget = {
  // Positive: the limit in bytes over pageCount × pageSize. Zero or negative: unlimited.
  limit: bigint;
  // The catalog root the last commit wrote; 0 before the first commit (nothing to reclaim yet).
  lastCatRoot: number;
  // The pages the last commit wrote.
  lastWritten: number[];
  // How many catalog pages the last commit wrote (the catalog is rewritten whole every commit).
  lastCatalogPages: number;
};

// BudgetCtx is the budget context of one in-memory domain's commit (memory.md §8.3): its name (for the
// 54P06 message), its committed snapshot (what a forced compaction rebuilds the free list from),
// whether the reader watermark allows compacting that snapshot (no live reader pins an older version),
// and whether the commit stages any record version (one that stages none and does not grow the catalog
// is admitted over the limit — the repair exemption).
export type BudgetCtx = {
  name: string;
  prev: Snapshot;
  canCompact: boolean;
  stagesRows: boolean;
};

// storageBytes is the committed-storage measure (memory.md §8.2): the logical high-water times the page
// size — deterministic, not RSS.
export function storageBytes(db: Engine): bigint {
  return BigInt(db.pageCount) * BigInt(db.pageSize);
}

// recordBudget remembers a successful in-memory commit's catalog root and written pages.
function recordBudget(db: Engine, staged: StagedCommit): void {
  db.storageBudget.lastCatRoot = staged.rootPage;
  db.storageBudget.lastWritten = staged.written.slice();
  db.storageBudget.lastCatalogPages = staged.catalogPages;
}

// StagedCommit is an in-memory commit whose pages are written into its block store but whose page
// accounting is not yet adopted (attached-databases.md §5). The pages occupy only free-list slots and
// pages past the high-water, none reachable from the published root, so dropping a staged commit leaves
// the storage exactly as the published root needs it; the next commit re-plans the same pages. Adopting
// it (adoptStaged) advances the high-water, free list, and budget record, then runs the post-commit
// compaction relative to the new root — which is sound only once that root is published.
export interface StagedCommit {
  // name is the attachment the commit belongs to ("" for a temp domain, which adopts in place).
  name: string;
  rootPage: number;
  pageCount: number;
  freeRemaining: number[];
  // written is the pages the commit wrote (unioned into the compaction's live set; the budget record).
  written: number[];
  // catalogPages is how many of them are catalog pages (the budget's repair exemption, memory.md §8.3).
  catalogPages: number;
}

// stagedFromWrite summarizes a written incremental plan as a StagedCommit.
function stagedFromWrite(name: string, write: IncrementalWrite): StagedCommit {
  return {
    name,
    rootPage: write.rootPage,
    pageCount: write.pageCount,
    freeRemaining: write.freeRemaining,
    written: write.pages.map((pg) => pg.index),
    catalogPages: catalogPageCount(write.pages),
  };
}

// planInMemory plans an in-memory commit's page allocation under the domain's max_storage_bytes limit
// (memory.md §8.3), before any page is written. Without a budget context (a temp domain, which
// temp_buffers bounds) or under an unlimited limit this is the plain incremental plan. A plan that raises
// the high-water past the limit first compacts the committed snapshot when the watermark allows, then
// re-plans; if it still does not fit the commit fails 54P06 (unless the repair exemption admits it).
function planInMemory(
  db: Engine,
  snap: Snapshot,
  reuse: boolean,
  budget: BudgetCtx | null,
): IncrementalWrite {
  const write = incrementalImage(snap, db.pageSize, db.pageCount, db.freePages, db.paging, reuse);
  if (budget === null || fits(db, write)) return write;
  if (budget.canCompact && forceCompact(db, budget.prev)) {
    // Planning assigned set-once page ids to the dirty nodes; clear them so the re-plan draws from the
    // enlarged free list.
    unassignPages(snap, write.pages);
    const replanned = incrementalImage(
      snap,
      db.pageSize,
      db.pageCount,
      db.freePages,
      db.paging,
      true,
    );
    if (fits(db, replanned)) return replanned;
    return admitRepair(db, replanned, budget);
  }
  return admitRepair(db, write, budget);
}

// fits reports whether write is admitted (memory.md §8.3): an unlimited domain, a commit that does not
// raise the high-water (the shrink-and-repair exemption), or one whose new high-water fits.
function fits(db: Engine, write: IncrementalWrite): boolean {
  const limit = db.storageBudget.limit;
  return (
    limit <= 0n ||
    write.pageCount <= db.pageCount ||
    BigInt(write.pageCount) * BigInt(db.pageSize) <= limit
  );
}

// admitRepair is the repair exemption (memory.md §8.3): copy-on-write needs fresh pages before the ones
// a delete frees are dead, so a commit that stages no record version and does not grow the catalog —
// pure deletes and drops — is admitted over the limit; anything else fails 54P06.
function admitRepair(db: Engine, write: IncrementalWrite, budget: BudgetCtx): IncrementalWrite {
  if (!budget.stagesRows && catalogPageCount(write.pages) <= db.storageBudget.lastCatalogPages) {
    return write;
  }
  throw engineError(
    "storage_limit_exceeded",
    `storage of database "${budget.name}" exceeded the limit of ${db.storageBudget.limit} bytes`,
  );
}

// forceCompact is the forced compaction of memory.md §8.3: rebuild the free list from the committed
// snapshot prev (the last commit's catalog root and written pages), ignoring the periodic trigger.
// Returns whether it freed any page the free list did not already hold.
function forceCompact(db: Engine, prev: Snapshot): boolean {
  const budget = db.storageBudget;
  if (budget.lastCatRoot === 0 || db.paging === null) return false; // no commit yet: nothing orphaned
  const reached = reachablePages(prev, db.paging, budget.lastCatRoot);
  for (const p of budget.lastWritten) reached.add(p);
  const free: number[] = [];
  for (let p = ROOT_PAGE; p < db.pageCount; p++) if (!reached.has(p)) free.push(p);
  if (free.length <= db.freePages.length) return false;
  db.freePages = free;
  db.liveAtCompaction = reached.size;
  db.freeGenTxid = prev.txid;
  return true;
}

// precheckBudget checks an in-memory domain's budget ahead of a multi-root commit (memory.md §8.3), so a
// rejection in a domain committed later publishes no domain's pages: plan (with any forced compaction),
// then release the plan's page ids. The real commit re-plans the same allocation. A durable or unlimited
// domain checks nothing.
export function precheckBudget(
  db: Engine,
  snap: Snapshot,
  reuse: boolean,
  budget: BudgetCtx,
): void {
  if (db.persistHook !== null || db.paging === null || db.storageBudget.limit <= 0n) return;
  const write = planInMemory(db, snap, reuse, budget);
  unassignPages(snap, write.pages);
}

// Durable stores write body, free-list, descriptor, and alternate meta, then synchronize once.
// Recovery checks every descriptor dependency before selecting that meta. The committed free list
// excludes all descriptor dependencies, preserving the previous candidate during the next commit.
// Memory stores retain their RAM free list and post-commit compaction without descriptor overhead.
// canReclaim is the caller's reader-watermark decision (default: no open streaming cursor).
// budget is the main domain's committed-storage context (memory.md §8.3), checked only for an
// in-memory store.
export function persistImpl(
  db: Engine,
  snap: Snapshot,
  canReclaim?: boolean,
  canReuse = true,
  budget: BudgetCtx | null = null,
): IncrementalWrite {
  if (db.paging !== null && db.persistHook !== null) db.paging.checkDurableCommit();
  const write =
    db.paging !== null && db.persistHook === null
      ? planInMemory(db, snap, canReuse, budget)
      : incrementalImage(snap, db.pageSize, db.pageCount, db.freePages, db.paging, canReuse);
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
  paging?.checkDurableCommit();
  const write = incrementalImage(snap, db.pageSize, db.pageCount, db.freePages, db.paging, false);
  if (paging === null) return { write, publishMeta: () => {} };
  const overflowCount = Math.ceil(
    Math.max(0, write.pages.length - manifestInlineCapacity(db.pageSize)) /
      manifestOverflowCapacity(db.pageSize),
  );
  const pageCount = write.pageCount + overflowCount;
  if (pageCount > 0xffff_ffff)
    throw engineError("program_limit_exceeded", "database page limit exceeded");
  const ids = Array.from({ length: overflowCount }, (_, i) => write.pageCount + i);
  const manifest = buildCommitManifest(db.pageSize, snap.txid, write.pages, ids);
  paging.beginDurableCommit();
  paging.refreshAllocatedPages();
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
  paging.beginDurableCommit();
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
  adoptStaged(db, snap, stagedFromWrite("", write), canReclaim);
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
  written: number[],
  canReclaim: boolean,
): void {
  if (!db.reclaimWithinSession || !canReclaim || db.paging === null) return;
  // The trigger constants are shared data (memory.md §8.4).
  if (db.pageCount <= COMPACT_MIN_PAGES || db.pageCount <= COMPACT_GROWTH * db.liveAtCompaction)
    return;
  const reached = reachablePages(snap, db.paging, catRoot);
  for (const p of written) reached.add(p);
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
// older temp tree). budget is an in-memory attachment's committed-storage context (memory.md §8.3);
// null for a session temp domain, which temp_buffers bounds instead.
export function persistTemp(
  db: Engine,
  snap: Snapshot,
  canReclaim: boolean,
  budget: BudgetCtx | null = null,
): void {
  if (db.paging === null) return;
  adoptStaged(db, snap, stageInMemory(db, snap, budget, ""), canReclaim);
}

// stageInMemory is the first half of persistTemp: write snap's dirty pages into the in-RAM store and
// take the residency flip, but adopt none of the page accounting. An in-memory attachment stages in
// commitTx and adopts only after main persists and the roots publish (attached-databases.md §5):
// adopting first would let a failed main persist leave a free list computed from an unpublished root,
// so a later commit could overwrite pages the published root still references.
export function stageInMemory(
  db: Engine,
  snap: Snapshot,
  budget: BudgetCtx | null,
  name: string,
): StagedCommit {
  const paging = db.paging!;
  const write = planInMemory(db, snap, true, budget);
  paging.reserve(write.pageCount);
  for (const pg of write.pages) {
    paging.writeBlock(pg.index, pg.bytes);
    // Drop any stale pool entry: within-session compaction may hand this page id back for a new node,
    // and the pool caches by page id (bufferpool.ts invalidate). A no-op for a fresh page.
    paging.invalidate(pg.index);
  }
  // No meta write, no sync: never reopened, no durability barrier.
  snap.demoteCleanLeaves();
  return stagedFromWrite(name, write);
}

// adoptStaged is the second half of persistTemp: adopt a staged commit's page accounting and run the
// post-commit compaction relative to snap, its now-committed root.
export function adoptStaged(
  db: Engine,
  snap: Snapshot,
  staged: StagedCommit,
  canReclaim: boolean,
): void {
  db.pageCount = staged.pageCount;
  recordBudget(db, staged);
  db.freePages = staged.freeRemaining;
  maybeCompact(db, snap, staged.rootPage, staged.written, canReclaim);
}
