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
  type ManifestEntry,
  manifestEntriesOf,
  metaChecksum,
} from "./commit_manifest.ts";
import { unselectedKeyPage } from "./crypto.ts";
import { engineError } from "./errors.ts";
import { runSync, type Steps } from "./execution.ts";
import type { FileHost } from "./filehost.ts";
import {
  BatchDeadline,
  type DeadlineSlot,
  type Durability,
  type IdleTarget,
} from "./idle_flush.ts";
import { cacheLeaves } from "./paging.ts";
import { wallClock } from "./seam.ts";
import type { SpillSink } from "./spill.ts";
import {
  catalogPageCount,
  incrementalImage,
  incrementalImageSteps,
  type IncrementalWrite,
  liveAfterSteps,
  type LiveCount,
  metaPage,
  planFreeListSteps,
  reachablePages,
  readFloorFreeSteps,
  reclaimDue,
  ROOT_PAGE,
  unassignPages,
  verifyLiveCountSteps,
} from "./format.ts";

// StorageBudget is the max_storage_bytes state of one storage domain (memory.md §8.1–§8.3, §8.7): the
// limit, plus what the last successful in-memory commit wrote, so a commit that would exceed the limit
// can first compact the committed snapshot (its catalog root and written pages — the latter cover a
// GiST R-tree, which reachablePages cannot see).
export type StorageBudget = {
  // Positive: the limit in bytes over pageCount × pageSize for an in-memory domain (§8.2), over
  // live pages × pageSize for a durable one (§8.7). Zero or negative: unlimited.
  limit: bigint;
  // The catalog root the last commit wrote; 0 before the first commit (nothing to reclaim yet).
  lastCatRoot: number;
  // The pages the last commit wrote.
  lastWritten: number[];
  // How many catalog pages the last commit wrote (the catalog is rewritten whole every commit).
  lastCatalogPages: number;
};

// BudgetCtx is the budget context of one domain's commit (memory.md §8.3, §8.7): its name (for the 54P06
// message), its committed snapshot (what a forced compaction rebuilds the free list from, and what a
// durable commit's live-page delta is taken against), whether the reader watermark allows compacting
// that snapshot (no live reader pins an older version), and whether the commit stages any record
// version (one that stages none and does not grow the catalog is admitted over the limit — the repair
// exemption).
export type BudgetCtx = {
  name: string;
  prev: Snapshot;
  canCompact: boolean;
  stagesRows: boolean;
};

// storageBytes is the committed-storage measure: an in-memory domain's logical high-water times the
// page size (memory.md §8.2), a durable domain's live pages times the page size (§8.7) — deterministic,
// not RSS and not the file length.
export function storageBytes(db: Engine): bigint {
  const pages = db.persistHook !== null ? db.live.live : db.pageCount;
  return BigInt(pages) * BigInt(db.pageSize);
}

// admitFile is the live-page count a DURABLE commit of write would publish, admitted under the file
// form of max_storage_bytes (memory.md §8.7) before any page is written. The measure is the live
// pages — reachable from the new catalog root — times the page size, so free pages never count. A
// commit is admitted when the domain is unlimited, when it does not grow the live count, when it
// fits, or under the repair exemption (it stages no record version and rewrites no more catalog
// pages than the last commit). There is no forced compaction: reclaiming dead pages cannot lower
// the measure. prev is the committed snapshot the domain's live count describes; a null budget (a
// bare engine's commit) only computes the count.
function* admitFileSteps(
  db: Engine,
  write: IncrementalWrite,
  prev: Snapshot,
  budget: BudgetCtx | null,
): Steps<LiveCount> {
  const live = yield* liveAfterSteps(db.live, write, prev, db.paging!);
  const limit = db.storageBudget.limit;
  if (
    budget === null ||
    limit <= 0n ||
    live.live <= db.live.live ||
    BigInt(live.live) * BigInt(db.pageSize) <= limit ||
    (!budget.stagesRows && write.catalogPages <= db.live.catalog)
  ) {
    return live;
  }
  throw engineError(
    "storage_limit_exceeded",
    `storage of database "${budget.name}" exceeded the limit of ${limit} bytes`,
  );
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
  // rootTag is the catalog head's tag in an encrypted domain (NO_TAG in plaintext) — a rekey's sealed
  // scratch domain (spec/fileformat/encrypted.md §12) follows it when it reclaims.
  rootTag: Uint8Array;
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
    rootTag: write.rootTag,
    pageCount: write.pageCount,
    freeRemaining: write.freeRemaining,
    written: write.pages.map((pg) => pg.index),
    catalogPages: catalogPageCount(write.pages),
  };
}

// planInMemory plans an in-memory commit's page allocation under the domain's max_storage_bytes limit
// (memory.md §8.3), before any page is written. Without a budget context (a temp domain, which
// max_temp_storage_bytes bounds) or under an unlimited limit this is the plain incremental plan. A plan
// that raises the high-water past the limit first compacts the committed snapshot when the watermark
// allows, then re-plans; if it still does not fit the commit fails 54P06 (unless the repair exemption
// admits it).
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

// precheckBudget checks a domain's budget ahead of a multi-root commit (memory.md §8.3, §8.7), so a
// rejection in a domain committed later publishes no domain's pages: plan (an in-memory domain with any
// forced compaction), admit, then release the plan's page ids. The real commit re-plans the same
// allocation. An unlimited domain checks nothing.
export function precheckBudget(
  db: Engine,
  snap: Snapshot,
  reuse: boolean,
  budget: BudgetCtx,
): void {
  runSync(precheckBudgetSteps(db, snap, reuse, budget));
}

export function* precheckBudgetSteps(
  db: Engine,
  snap: Snapshot,
  reuse: boolean,
  budget: BudgetCtx,
): Steps<void> {
  if (db.paging === null || db.storageBudget.limit <= 0n) return;
  if (db.persistHook !== null) {
    // Plan as the commit will: a pending relaxed batch reuses only pages free at its floor. A commit
    // past its batch's deadline flushes first and plans without that restriction (validated-cow.md
    // §4.1); the answer is the same, since live pages (memory.md §8.7) do not depend on which page
    // ids a commit takes.
    const allowed = batchOpen(db) ? yield* floorFreeSteps(db) : null;
    const write = yield* incrementalImageSteps(
      snap,
      db.pageSize,
      db.pageCount,
      db.freePages,
      db.paging,
      reuse,
      allowed,
    );
    try {
      yield* admitFileSteps(db, write, budget.prev, budget);
    } finally {
      unassignPages(snap, write.pages);
    }
    return;
  }
  const write = planInMemory(db, snap, reuse, budget);
  unassignPages(snap, write.pages);
}

// Durable stores write body, free-list, descriptor, and alternate meta, then synchronize once.
// Recovery checks every descriptor dependency before selecting that meta. The committed free list
// excludes all descriptor dependencies, preserving the previous candidate during the next commit.
// Memory stores retain their RAM free list and post-commit compaction without descriptor overhead.
// canReclaim is the caller's reader-watermark decision (default: no open streaming cursor).
// budget is the domain's committed-storage context (memory.md §8.3, §8.7): an in-memory store is
// planned under it, a durable one admitted under its file form. A durable commit without one (a bare
// engine's persistHook) takes its live-page delta against db.committed, still the previous snapshot.
export function persistImpl(
  db: Engine,
  snap: Snapshot,
  canReclaim?: boolean,
  canReuse = true,
  budget: BudgetCtx | null = null,
  truncate = true,
): IncrementalWrite {
  return runSync(persistSteps(db, snap, canReclaim, canReuse, budget, truncate));
}

// persistSteps is persistImpl as one continuation (ts-runtime.md §11.2): the synchronous internal API
// drains it with runSync; the execution worker awaits it inside the uncancelable commit transition.
// Over a synchronous storage stack every step runs inline.
export function* persistSteps(
  db: Engine,
  snap: Snapshot,
  canReclaim?: boolean,
  canReuse = true,
  budget: BudgetCtx | null = null,
  truncate = true,
): Steps<IncrementalWrite> {
  // The batch deadline (validated-cow.md §4.1): the idle flusher is held off from the clock reading to
  // the publish, so it never runs inside a commit that awaits. The file's owner times every batch, a
  // synchronous one's included, since its relaxed page clients may open one; a page client's storage
  // times none.
  const deadline = timedFile(db) ? db.batchDeadline : null;
  if (deadline === null) {
    return yield* persistBodySteps(db, snap, canReclaim, canReuse, budget, truncate, null);
  }
  deadline.enter();
  try {
    return yield* persistBodySteps(db, snap, canReclaim, canReuse, budget, truncate, deadline);
  } finally {
    deadline.leave();
  }
}

function* persistBodySteps(
  db: Engine,
  snap: Snapshot,
  canReclaim: boolean | undefined,
  canReuse: boolean,
  budget: BudgetCtx | null,
  truncate: boolean,
  deadline: BatchDeadline | null,
): Steps<IncrementalWrite> {
  let allowed: Set<number> | null = null;
  let pending = false;
  let now = 0n;
  let read = false;
  if (db.paging !== null && db.persistHook !== null) {
    pending = batchOpen(db);
    db.paging.checkDurableCommit();
    if (deadline !== null && (pending || relaxedFile(db))) {
      // One clock reading per relaxed commit, or per commit that begins inside a batch (rule 1); past
      // the deadline, flush the batch before planning (rule 3). A failed flush is the commit's error and
      // poisons the storage.
      read = true;
      now = db.environment.clockSource().nowMicros();
      if (pending && deadline.passed(now)) {
        yield* db.paging.flushPendingSteps();
        deadline.clear();
        pending = false;
      }
    }
    allowed = yield* reuseAllowedSteps(db, pending);
  }
  const write =
    db.paging !== null && db.persistHook === null
      ? planInMemory(db, snap, canReuse, budget)
      : yield* incrementalImageSteps(
          snap,
          db.pageSize,
          db.pageCount,
          db.freePages,
          db.paging,
          canReuse,
          allowed,
        );
  if (db.paging === null) return write; // a bare engine with no byte store: nothing to write
  const reclaim = canReclaim ?? db.openStreams === 0;
  // DURABLE (file or OPFS — both reopened, both set a persistHook; the OPFS host leaves `path` null, so
  // durability is keyed on persistHook, not path) persists the free-list in-commit; an IN-MEMORY main
  // store (persistHook null) keeps its free-list in RAM.
  if (db.persistHook !== null) {
    const live = yield* admitFileSteps(db, write, budget?.prev ?? db.committed, budget);
    const capReached = yield* commitFileSteps(
      db,
      snap,
      write,
      live,
      reclaim,
      canReuse,
      allowed,
      truncate,
    );
    // A synchronous commit's own barrier ends the batch it began in.
    if (deadline !== null && read) {
      deadline.afterCommit(
        !pending,
        relaxedFile(db) && !capReached,
        now,
        idleTarget(db),
        db.environment.clockSource(),
      );
    }
  } else commitInMemory(db, snap, write, reclaim);
  return write;
}

// idleTarget is db's domain as its idle flusher flushes it (validated-cow.md §4.1 rule 5), read when the
// timer fires: the storage then in place (compaction may have replaced it), flushed like the cap flush
// under a page-service owner's writer gate, so it never runs beside a client's begin_write or commit.
function idleTarget(db: Engine): IdleTarget {
  return {
    pending: () => {
      const paging = db.paging;
      return (
        paging !== null && timedFile(db) && !paging.commitRequiresReopen() && paging.hasPending()
      );
    },
    flush: (begin) => {
      let release: (() => void) | null = null;
      if (db.flushGate !== null) {
        release = db.flushGate();
        if (release === null) return false;
      }
      try {
        if (begin()) runSync(capFlushSteps(db));
      } finally {
        release?.();
      }
      return true;
    },
  };
}

// armClientBatch arms db's idle flusher for a batch a relaxed page client's commit started on its
// service (validated-cow.md §4.1 rule 4): the service runs on another thread, so it asks the engine
// thread to (owner.ts).
export function armClientBatch(db: Engine): void {
  if (!timedFile(db)) return;
  db.batchDeadline.armClientBatch(idleTarget(db), db.environment.clockSource());
}

// bindBatchDeadline keeps db's batch deadline in a page-service owner's shared state (validated-cow.md
// §4.1 rule 4), where its service reads and sets it under the writer gate.
export function bindBatchDeadline(db: Engine, slot: DeadlineSlot): void {
  db.batchDeadline.bind(slot, db.environment.clockSource() === wallClock());
}

// setDurability sets the handle's durability settings on domain db: synchronous = off and its
// time-based flush (validated-cow.md §4, §4.1). A durable domain only batches; in-memory and page-client
// storage never read the settings.
export function setDurability(db: Engine, d: Durability): void {
  db.relaxed = d.relaxed;
  const slot = db.batchDeadline.slot();
  db.batchDeadline.stop();
  db.batchDeadline = new BatchDeadline(d.flushIntervalMs, d.idleFlush);
  if (slot !== null) bindBatchDeadline(db, slot);
}

// durabilityOf is db's durability settings, which a file attachment follows.
export function durabilityOf(db: Engine): Durability {
  return {
    relaxed: db.relaxed,
    flushIntervalMs: db.batchDeadline.intervalMs,
    idleFlush: db.batchDeadline.idle,
  };
}

// stopIdleFlush ends db's idle flusher (close, detach, a role change).
export function stopIdleFlush(db: Engine): void {
  db.batchDeadline.stop();
}

// DirectorySync makes the directory holding the database file at path durable, through files (null is
// the OS). Registered by the Node host (file.ts), so this module stays browser-clean.
type DirectorySync = (files: FileHost | null, path: string) => void;
// relaxedFile reports whether db's commits batch their barrier (synchronous = off, validated-cow.md
// §4): a durable domain of a relaxed handle.
function relaxedFile(db: Engine): boolean {
  return db.relaxed && db.persistHook !== null && db.paging !== null;
}

// timedFile reports whether db's commits time batches (validated-cow.md §4.1 rule 1): a durable domain
// with a non-zero interval that is not a page client's storage, whose owner times its batches.
function timedFile(db: Engine): boolean {
  return (
    db.batchDeadline.timed() &&
    db.persistHook !== null &&
    db.paging !== null &&
    !db.paging.trustsMeta()
  );
}

// reuseAllowedSteps is the reuse restriction of the commit about to plan (validated-cow.md §4 rule 3).
// With nothing pending the floor is the committed generation, so a relaxed domain records the pages
// free now — the floor's free list — and allows them all; while a batch is pending only those not yet
// consumed may be reused, since a page freed during the batch may still be needed by the floor. The
// restriction binds every commit that begins inside a batch, a synchronous = on owner's included,
// since its relaxed page clients may have opened one (page-service.md §6).
function* reuseAllowedSteps(db: Engine, pending: boolean): Steps<Set<number> | null> {
  if (pending) return yield* floorFreeSteps(db);
  if (relaxedFile(db)) {
    db.floorFree = new Set(db.freePages);
    db.floorFreeTxid = db.paging!.floorMetaTxid();
  }
  return null;
}

// floorFreeSteps is the restriction a pending batch plans under, without recording a new floor. A page
// client's is the reuse list its owner sent. A batch this domain did not start — a page-service
// owner's batch a relaxed client's commit opened — has no recorded floor list, so the floor's is read
// from its meta; a list that cannot be read allows no reuse.
function* floorFreeSteps(db: Engine): Steps<Set<number>> {
  if (db.serviceBatch !== null) return db.serviceBatch.reuse;
  const paging = db.paging!;
  const floor = paging.floorMetaTxid();
  if (db.floorFree !== null && db.floorFreeTxid === floor) return db.floorFree;
  let free: number[] = [];
  try {
    free = yield* readFloorFreeSteps(paging);
  } catch {
    // No reuse: always safe inside the batch.
  }
  db.floorFree = new Set(free);
  db.floorFreeTxid = floor;
  return db.floorFree;
}

// batchOpen reports whether the commit about to plan begins inside a batch: the domain's own pending
// set, or on a page client its owner's from a relaxed write_begun (spec/pageservice/README.md §3.5).
function batchOpen(db: Engine): boolean {
  return db.serviceBatch !== null || db.paging!.hasPending();
}

// batchEntries is the pending set the commit about to plan carries in its manifest.
function batchEntries(db: Engine): ManifestEntry[] {
  return db.serviceBatch?.pending ?? db.paging!.pendingEntries();
}

// capFlush ends a batch that reached the cap (validated-cow.md §4). TypeScript has no second engine
// thread, so it runs inline after the commit that reached the cap, which is already published and
// still succeeds. A failed barrier may have dropped pending pages from the OS cache ("fsyncgate"), so
// pages the published snapshot needs could now read stale: the storage is revoked, and every later
// read, commit, and close fails 58030 until the database is reopened through recovery.
function* capFlushSteps(db: Engine): Steps<void> {
  const paging = db.paging!;
  try {
    yield* paging.flushPendingSteps();
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    paging.revoke(
      engineError("io_error", "a deferred durability flush failed; reopen the database: " + msg),
    );
  }
}

let directorySync: DirectorySync | null = null;

// registerDirectorySync installs the host's directory barrier for syncDirectoryOnce (called once, at
// file.ts module load).
export function registerDirectorySync(fn: DirectorySync): void {
  directorySync = fn;
}

// syncDirectoryOnce makes the database file's directory durable before the first durable commit after
// open (validated-cow.md §3). A process that crashed between a compaction's or create's rename and its
// directory sync leaves the new file in place but its directory entry unsynced; a later power loss could
// revert the rename and bring back the older file, losing every commit made since. Syncing a file never
// makes its directory entry durable, so the first writer does it once. Best effort, like every
// directory sync, and skipped under fsync=off. A store without a path (OPFS, in memory) has no directory.
function syncDirectoryOnce(db: Engine): void {
  if (db.dirSynced || db.path === null) return;
  db.dirSynced = true;
  if (db.paging === null || db.paging.skipsSync() || directorySync === null) return;
  try {
    directorySync(db.fileHost, db.path);
  } catch {
    // Best effort: an unresolvable path skips the directory barrier, as every directory sync may.
  }
}

// writeKeyBlockSteps writes a credential change's key block (spec/design/encryption.md §5;
// spec/fileformat/encrypted.md §5): a stabilization barrier (any pending relaxed batch flushed, the file
// synced, and its directory once), then the copy that is not the selected one, a barrier, the other copy,
// a barrier; only then does the codec adopt block. The caller holds the writer gate. A failure poisons the
// storage, so the handle must be reopened; recovery then unlocks with exactly one of the old and new
// credentials. A block derived before another change won the gate is not the selected copy's next
// generation: 55006 before anything is written.
export function* writeKeyBlockSteps(db: Engine, block: Uint8Array): Steps<void> {
  const paging = db.paging;
  if (paging === null) throw engineError("io_error", "database storage is closed");
  const codec = paging.codec()!;
  yield* paging.flushPendingSteps();
  paging.checkDurableCommit();
  if (!codec.follows(block)) {
    throw engineError("object_in_use", "the credential was changed concurrently");
  }
  syncDirectoryOnce(db);
  try {
    yield* paging.syncSteps();
    const first = unselectedKeyPage(yield* paging.readPageSteps(2), yield* paging.readPageSteps(3));
    const page = new Uint8Array(db.pageSize);
    page.set(block, 0);
    yield* paging.writePagesSteps([{ index: first, bytes: page }]);
    yield* paging.syncSteps();
    yield* paging.writePagesSteps([{ index: 5 - first, bytes: page }]);
    yield* paging.syncSteps();
  } catch (e) {
    paging.poison();
    throw e;
  }
  codec.setKeyBlock(block);
}

// Write tree/catalog first so reclamation can read the new catalog. Joint allocation then reserves
// descriptor and free-list pages from the prior safe free set, protecting every new write from reuse.
// The inline/overflow descriptor hashes those body pages and the single final sync publishes them.
// live is the admitted live-page count the meta publishes (memory.md §8.7). Returns whether the commit
// reached the relaxed batch cap (a flush followed).
// Each write, barrier, and read below completes before the next begins, in the synchronous order
// (ts-runtime.md §11.2); after beginDurableCommitSteps any failure keeps the commit guard armed.
function* commitFileSteps(
  db: Engine,
  snap: Snapshot,
  write: IncrementalWrite,
  live: LiveCount,
  canReclaim: boolean,
  canReuse: boolean,
  allowed: Set<number> | null,
  truncate: boolean,
): Steps<boolean> {
  syncDirectoryOnce(db);
  const paging = db.paging!;
  yield* paging.beginDurableCommitSteps();
  // Preallocate ahead of the high-water so the body sync carries no file-growth journaling (pager.md §7).
  yield* paging.reserveSteps(write.pageCount);
  yield* paging.writePagesSteps(write.pages);
  // A relaxed batch's pending set joins the manifest and stays off the free list (validated-cow.md §4).
  const relaxed = relaxedFile(db);
  const pending = batchEntries(db);
  const plan = yield* planFreeListSteps(
    snap,
    paging,
    write.rootPage,
    write.rootTag,
    write.pages,
    write.freeRemaining,
    write.pageCount,
    db.liveAtCompaction,
    db.freeGenTxid,
    db.pageSize,
    canReclaim,
    canReuse,
    pending,
    allowed,
    // Only the file's owner lowers the high-water, and only against a durable floor: a commit inside a
    // relaxed batch may not cut pages the batch has yet to make durable (validated-cow.md §4).
    truncate && pending.length === 0,
  );
  yield* verifyLiveCountSteps(snap, paging, write, live);
  yield* paging.reserveSteps(plan.newPageCount);
  yield* paging.writePagesSteps(plan.pages);
  // The commit's own entries (its body and free-list pages) join the pending set of a relaxed commit.
  const dirty = manifestEntriesOf([...write.pages, ...plan.pages]);
  const codec = paging.codec();
  const manifest = buildCommitManifest(
    db.pageSize,
    snap.txid,
    [...pending, ...dirty],
    plan.manifestIds,
    codec !== null,
  );
  yield* paging.writePagesSteps(manifest.pages);
  const meta = metaPage(
    db.pageSize,
    snap.txid,
    write.rootPage,
    plan.newPageCount,
    plan.head,
    live.live,
    manifest,
    codec !== null ? { root: write.rootTag, freeList: plan.headTag } : undefined,
  );
  // The slot not holding the floor (format.md, *Slot alternation*): txid & 1 under synchronous = on.
  const slot = paging.commitSlot();
  // An encrypted meta is sealed for its slot over its manifest chain (encrypted.md §6).
  if (codec !== null) yield* codec.sealMetaSteps(slot, meta, manifest.pages);
  yield* paging.writePagesSteps([{ index: slot, bytes: meta }]);
  // One durability barrier covers body + descriptor + meta; a relaxed commit defers it to a flush.
  if (!relaxed) yield* paging.syncSteps();
  paging.finishDurableCommit(snap.txid, metaChecksum(meta));
  paging.recordCommit(snap.txid, metaChecksum(meta), slot, !relaxed, dirty);
  // Trailing truncation lowered the high-water: shorten the file once the new meta is durable — now
  // under synchronous = on, at the batch's flush under synchronous = off — keeping one growth step of
  // slack (pager.md §7).
  yield* paging.settleTruncationSteps(
    plan.newPageCount,
    plan.newPageCount < db.pageCount,
    !relaxed,
  );
  const capReached = relaxed && paging.batchCapReached();
  db.lastCommitMeta = meta;
  db.lastCommitWritten = [...write.pages, ...plan.pages, ...manifest.pages].map((pg) => pg.index);
  db.pageCount = plan.newPageCount;
  db.freePages = plan.persisted;
  db.liveAtCompaction = plan.newLive;
  db.freeGenTxid = plan.newGen;
  db.live = live;
  if (capReached) yield* capFlushSteps(db);
  return capReached;
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
  // A memory store is never reopened, so its meta carries no live-page count (memory.md §8.7).
  const meta = metaPage(db.pageSize, snap.txid, write.rootPage, write.pageCount, 0, 0);
  paging.writeBlock(Number(snap.txid & 1n), meta);
  paging.sync();
  adoptStaged(db, snap, stagedFromWrite("", write), canReclaim);
}

// commitDurableAttachment durably commits a FILE-backed host attachment's working snapshot into its own
// byte store (attached-databases.md §5, Slice 2): the SAME durable recipe as the main persist
// (persistImpl — v25 persists the free-list in-commit for a file store), then the post-commit residency
// flip (demoteCleanLeaves — bplus-reshape.md B4). The caller advances snap.txid before calling. Runs
// under the writer gate. An in-memory attachment uses stageInMemory instead (no fsync). budget is the
// attachment's committed-storage context (memory.md §8.7).
export function* commitDurableAttachmentSteps(
  db: Engine,
  snap: Snapshot,
  canReclaim: boolean,
  canReuse: boolean,
  budget: BudgetCtx,
): Steps<void> {
  yield* persistSteps(db, snap, canReclaim, canReuse, budget);
  yield* snap.demoteCleanLeavesSteps();
}

// maybeCompact reclaims within-session copy-on-write orphans IN RAM by rebuilding the free-list from the
// live (reachable) set — the POST-commit form used by never-reopened stores (session temp, in-memory
// attachments, in-memory main), which need no persisted free-list. (A file-backed store instead reclaims
// IN-COMMIT so the reclaimed list is durable — planFreeList.) It is:
//   - a no-op for a non-reclaim domain (reclaimWithinSession false);
//   - deferred while any older version is pinned (canReclaim false): compaction frees pages unreachable
//     from the committed root, which an older reader may still observe, so it waits for the pins to drain;
//   - periodic: it walks (O(pages)) only once the pages off the free list pass ~2× the live count at
//     the last compaction (reclaimDue), so the walk is amortized O(height)/commit.
// written is the pages THIS commit wrote — unioned into the live set so a live GiST R-tree (rewritten
// wholesale each commit, invisible to reachablePages) is never freed. canReclaim is the caller's
// watermark decision.
export function maybeCompact(
  db: Engine,
  snap: Snapshot,
  catRoot: number,
  written: number[],
  canReclaim: boolean,
  catTag?: Uint8Array,
): void {
  if (!db.reclaimWithinSession || !canReclaim || db.paging === null) return;
  if (!reclaimDue(db.pageCount, db.freePages.length, db.liveAtCompaction)) return;
  const reached = reachablePages(snap, db.paging, catRoot, catTag);
  for (const p of written) reached.add(p);
  const free: number[] = [];
  for (let p = ROOT_PAGE; p < db.pageCount; p++) {
    if (!reached.has(p)) free.push(p);
  }
  db.freePages = free;
  db.liveAtCompaction = reached.size;
  db.freeGenTxid = snap.txid; // the recomputed list is proven dead at snap.txid (the §8 reuse gate)
}

// tempSpills counts the session-local temp domains spilled to a scratch file in this process
// (temp-tables.md §6) — the conformance harness's forced temp-spill mode reports it, so a run that never
// actually spilled cannot pass silently. Rust's TEMP_SPILLS.
export const tempSpills = { count: 0 };

// stageTemp stages a TEMP snapshot's dirty pages into the domain's store (temp-tables.md §6): the SAME
// incremental copy-on-write serialize as a file/in-memory commit, but with NO meta slot and NO sync — a
// temp domain is never reopened and has no durability barrier — then the residency flip (clean leaves
// demote to OnDisk, faulted back through the temp pool: the compact packed footprint). ZERO main-file
// writes: only the temp byte store is touched, so the zero-file-write invariant (temp-tables.md §2, D1)
// holds by construction. Assigns page ids on snap in place. Staged pages occupy only free-list slots and
// pages past the high-water, so the caller stages BEFORE the main persist and adopts (adoptStaged) after
// it: a failure anywhere in between leaves the temp store matching its still-committed root.
//
// Spill-to-disk (temp-tables.md §6): once the domain's page bytes exceed tempBuffers (0 ⇒ never) and the
// host has a spill sink, the in-RAM store migrates to a scratch file and the pool is bounded by
// tempBuffers. Residency only — the page layout, and so results, cost, and the 54P03 point, are
// unchanged. An in-memory or OPFS database passes a null sink (or one without createScratchStore) and
// stays resident.
export function stageTemp(
  db: Engine,
  snap: Snapshot,
  tempBuffers: number,
  sink: SpillSink | null,
): StagedCommit {
  const paging = db.paging!;
  const staged = stageInMemory(db, snap, null, "");
  const bytes = staged.pageCount * db.pageSize;
  if (
    !db.spilled &&
    tempBuffers > 0 &&
    bytes > tempBuffers &&
    sink?.createScratchStore !== undefined
  ) {
    paging.migrateStore(sink.createScratchStore());
    db.spilled = true;
    tempSpills.count++;
  }
  if (db.spilled) {
    // Re-applied every commit, so a later temp_buffers change takes effect.
    paging.setCapacity(
      tempBuffers === 0 ? Number.MAX_SAFE_INTEGER : cacheLeaves(tempBuffers, db.pageSize),
    );
  }
  return staged;
}

// stageInMemory is the first half of a temp or in-memory attachment commit: write snap's dirty pages
// into the in-RAM store and take the residency flip, but adopt none of the page accounting. An in-memory attachment stages in
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

// adoptStaged is the second half of stageTemp / stageInMemory: adopt a staged commit's page accounting
// and run the post-commit compaction relative to snap, its now-committed root.
export function adoptStaged(
  db: Engine,
  snap: Snapshot,
  staged: StagedCommit,
  canReclaim: boolean,
): void {
  db.pageCount = staged.pageCount;
  recordBudget(db, staged);
  db.freePages = staged.freeRemaining;
  maybeCompact(db, snap, staged.rootPage, staged.written, canReclaim, staged.rootTag);
}

// repairKeyBlockSteps finishes a credential change a crash interrupted (spec/design/encryption.md §5;
// spec/fileformat/encrypted.md §5): when the two key-block copies differ, the opener that unwrapped the
// selected copy writes it over the other, between barriers. A read-only open leaves the pair alone, which
// is safe because selection already prefers the newer copy.
export function* repairKeyBlockSteps(db: Engine): Steps<void> {
  const paging = db.paging;
  const codec = paging?.codec() ?? null;
  if (paging === null || codec === null) return;
  const page2 = yield* paging.readPageSteps(2);
  const page3 = yield* paging.readPageSteps(3);
  if (bytesEqual(page2, page3)) return;
  const stale = unselectedKeyPage(page2, page3);
  yield* paging.syncSteps();
  yield* paging.writePagesSteps([{ index: stale, bytes: codec.keyBlockPage(db.pageSize) }]);
  yield* paging.syncSteps();
}

function bytesEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}
