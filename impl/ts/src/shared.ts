// Shared database handle for the TS core (CLAUDE.md §3, spec/design/transactions.md §8/§10).
//
// JavaScript has no shared-memory threads for live objects, so this core cannot offer the CPU
// parallelism the Rust and Go cores do (real reader threads running on cores while a writer
// commits). What it CAN — and must — offer is the other half of the §3 model: snapshot ISOLATION.
// A reader pins the committed snapshot and observes a single, stable version for its whole life,
// even as a writer interleaves commits between the reader's calls (across `await` points, event-
// loop turns, or generator yields). That is the same contract the threaded cores enforce, minus
// the parallelism — and it is enforced by the same machinery: one committed cell, at most one open
// writer, and a live-reader registry whose minimum pinned version is the reclamation watermark.
//
// Shape (parallel to the Rust/Go cores, so the design stays one shared design):
//   - Database is the handle; read() and write() mint independent per-caller handles.
//   - SharedCore holds the published committed snapshot, the single-writer flag (a second write()
//     while one is open throws 25001 — JS cannot block the one thread, so the faithful analog of
//     "one writer at a time" is to reject, not wait), and the live-reader registry (§8).
//   - ReadHandle pins the committed snapshot at read() and registers its version; it serves reads
//     from that pinned, immutable snapshot — unaffected by any later commit — and a write through
//     it is 25006. close() deregisters (no destructor in JS — the caller calls it), advancing the
//     watermark.
//   - WriteHandle holds the writer flag, captures the committed snapshot as a private working set
//     (an open READ WRITE block over a private Engine), and on commit publishes the working
//     snapshot into the cell at the next version. rollback / leaving it un-ended discards it.
//
// In-memory this slice (the isolation mechanism + watermark are the deliverable; durability is the
// orthogonal §9 axis). Isolation comes for free from the persistent (copy-on-write) stores
// (pmap.ts): a pinned snapshot is immutable and shares structure with later versions, so pinning is
// a reference copy, not a deep clone.

import { Engine, Snapshot, stmtIsWrite, type Outcome } from "./executor.ts";
import { type Rows, rowsFromOutcome } from "./api.ts";
import { engineError } from "./errors.ts";
import type { Value } from "./value.ts";

// databaseFromSnapshot builds an in-memory handle whose committed roots are `snap` (the file
// snapshot) and `sharedTemp` (the database-wide shared-temp snapshot, temp-tables.md §5) — no file
// backing. A read handle keeps one with no open transaction (reads hit committed = the pinned
// snapshot); a write handle keeps one with an open READ WRITE block and publishes its working set.
function databaseFromSnapshot(snap: Snapshot, sharedTemp: Snapshot): Engine {
  const db = new Engine();
  db.committed = snap;
  db.sharedTempCommitted = sharedTemp;
  return db;
}

// SharedCore is the state shared by every handle minted from one Database: the published committed
// roots (the file snapshot AND the database-wide shared-temp snapshot, temp-tables.md §5), the
// single-writer flag, and the live-reader registry (transactions.md §8). Not exported — only the
// handles touch it. (TS is single-threaded, so a handle reads both roots in one synchronous step:
// there is no torn pin, hence no need for a combined holder object — two fields suffice.)
class SharedCore {
  committed: Snapshot;
  // sharedTempCommitted is the published shared-temp root (temp-tables.md §4): the rows of every
  // SHARED temp table, visible to every handle, NEVER serialized. Published alongside `committed` by
  // WriteHandle.commit — a pure in-memory swap (no fsync, nothing written to the file).
  sharedTempCommitted: Snapshot;
  // live maps a pinned snapshot version to its reader refcount; its minimum key is the reclamation
  // watermark (several readers may pin the same version).
  live = new Map<bigint, number>();
  // writerActive is true while a write transaction is open (at most one — CLAUDE.md §3).
  writerActive = false;

  constructor(snap: Snapshot) {
    this.committed = snap;
    this.sharedTempCommitted = new Snapshot();
  }

  register(version: bigint): void {
    this.live.set(version, (this.live.get(version) ?? 0) + 1);
  }

  deregister(version: bigint): void {
    const c = (this.live.get(version) ?? 0) - 1;
    if (c <= 0) this.live.delete(version);
    else this.live.set(version, c);
  }

  // oldest is the oldest still-pinned version, or the committed version when no reader is live. The
  // map scan is order-independent (a minimum), so no map iteration order leaks (CLAUDE.md §8).
  oldest(): bigint {
    let oldest = this.committed.txid;
    for (const v of this.live.keys()) if (v < oldest) oldest = v;
    return oldest;
  }
}

// Database is a database handle offering snapshot-isolated readers and a single writer
// (transactions.md §10). read() and write() mint independent per-caller handles over one core.
export class Database {
  private core: SharedCore;

  private constructor(core: SharedCore) {
    this.core = core;
  }

  // newInMemory builds a fresh, empty in-memory shared database (committed version 0).
  static newInMemory(): Database {
    return new Database(new SharedCore(new Snapshot(0n)));
  }

  // version is the committed version currently published (the monotonic commit counter,
  // transactions.md §8). Advances by 1 on every WriteHandle.commit.
  get version(): bigint {
    return this.core.committed.txid;
  }

  // oldestLiveTxid is the oldest still-live snapshot version (transactions.md §8) — the Phase-6
  // reclamation watermark. With live readers it is the minimum version any of them pinned; with
  // none it is the committed version (nothing older is reachable).
  oldestLiveTxid(): bigint {
    return this.core.oldest();
  }

  // read opens a read handle over a consistent snapshot (transactions.md §10). Pins the committed
  // snapshot now and registers it in the live set; the handle serves reads from that snapshot for
  // its life — observing one stable version even as a writer interleaves commits. The caller must
  // close() it to deregister (advancing the watermark), since JS has no destructor.
  read(): ReadHandle {
    const snap = this.core.committed; // pin (immutable — no clone)
    this.core.register(snap.txid);
    return new ReadHandle(this.core, snap);
  }

  // write opens the write handle (transactions.md §10). A second write() while one is open throws
  // 25001 (JS cannot block the single thread, so the faithful single-writer analog is to reject).
  // Captures the committed snapshot as a private working set; commit publishes it, rollback / an
  // un-ended handle discards it.
  write(): WriteHandle {
    if (this.core.writerActive) {
      throw engineError("active_sql_transaction", "there is already a writer in progress");
    }
    this.core.writerActive = true;
    return new WriteHandle(this.core, this.core.committed);
  }
}

// ReadHandle is a read handle over a pinned, consistent snapshot (transactions.md §10).
export class ReadHandle {
  private core: SharedCore;
  private db: Engine; // committed = the pinned (immutable) snapshot, no open transaction
  private pinnedVersion: bigint;
  private closed = false;

  constructor(core: SharedCore, snap: Snapshot) {
    this.core = core;
    this.pinnedVersion = snap.txid;
    // Pin both roots together (temp-tables.md §5): the reader sees a consistent file + shared-temp
    // view. Single-threaded JS reads both fields synchronously, so the pin is atomic.
    this.db = databaseFromSnapshot(snap, core.sharedTempCommitted);
  }

  // query runs a read query against the pinned snapshot, returning a row cursor. A write statement
  // is 25006 (the snapshot is read-only) — rejected before dispatch, so the handle is never
  // poisoned and every call is independent.
  query(sql: string, params: Value[] = []): Rows {
    return rowsFromOutcome(this.readOnly(sql, params));
  }

  // execute runs a read statement against the pinned snapshot, returning its outcome. A write is
  // 25006.
  execute(sql: string, params: Value[] = []): Outcome {
    return this.readOnly(sql, params);
  }

  private readOnly(sql: string, params: Value[]): Outcome {
    const stmt = this.db.parse(sql);
    if (stmtIsWrite(stmt)) {
      throw engineError(
        "read_only_sql_transaction",
        "cannot execute a write statement against a read-only snapshot",
      );
    }
    return this.db.executeStmtParams(stmt, params);
  }

  // version is the snapshot version this handle pinned (its entry in the live-reader registry).
  get version(): bigint {
    return this.pinnedVersion;
  }

  // close deregisters the handle from the live set, advancing the watermark. Idempotent; the
  // caller must call it (try/finally) since JS has no destructor.
  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.core.deregister(this.pinnedVersion);
  }
}

// WriteHandle is the single write handle (transactions.md §10). Holds the writer flag for its life;
// statements accumulate in a private working set and become visible only at commit.
export class WriteHandle {
  private core: SharedCore;
  private db: Engine; // an open READ WRITE block; its working set is the staging buffer (§3)
  private baseVersion: bigint; // committed version at write(); the published version is baseVersion+1
  private done = false;

  constructor(core: SharedCore, base: Snapshot) {
    this.core = core;
    this.baseVersion = base.txid;
    // committed/sharedTempCommitted = the immutable bases; beginTx clones them to working /
    // sharedTempWorking. Both roots are pinned together (temp-tables.md §5).
    this.db = databaseFromSnapshot(base, core.sharedTempCommitted);
    this.db.beginTx(true);
  }

  // execute runs a (possibly mutating) statement within this write transaction. A statement error
  // aborts the block (every later statement but commit/rollback is then 25P02, §6).
  execute(sql: string, params: Value[] = []): Outcome {
    return this.db.executeStmtParams(this.db.parse(sql), params);
  }

  // query runs a query within this write transaction (read-your-writes against the working set).
  query(sql: string, params: Value[] = []): Rows {
    return rowsFromOutcome(this.db.executeStmtParams(this.db.parse(sql), params));
  }

  // commit publishes the working set as the new committed snapshot at the next version (the §3
  // commit window), then releases the writer flag. A failed (aborted) block publishes nothing — a
  // failed COMMIT is a ROLLBACK (PostgreSQL). Idempotent after the first end.
  commit(): void {
    if (this.done) return;
    this.done = true;
    const failed = this.db.session.tx?.failed ?? false;
    this.db.commitTx(); // inner in-memory swap: committed := working, shared-temp adopted (or no-op if failed)
    if (!failed) {
      const snap = this.db.committed;
      snap.txid = this.baseVersion + 1n; // advance the shared version on every commit
      // Publish BOTH roots (the two-root commit, temp-tables.md §5): the file snapshot and the
      // shared-temp snapshot (a pure in-memory swap — no fsync, nothing written to the file). Single-
      // threaded JS publishes both synchronously, so a reader never observes a torn pair. A writer
      // that did not touch shared temp republishes the unchanged shared-temp root (it pinned it).
      this.core.committed = snap;
      this.core.sharedTempCommitted = this.db.sharedTempCommitted;
    }
    this.core.writerActive = false;
  }

  // rollback discards the working set (the committed snapshot was never touched) and releases the
  // writer flag. Idempotent after the first end.
  rollback(): void {
    if (this.done) return;
    this.done = true;
    this.core.writerActive = false;
  }
}
