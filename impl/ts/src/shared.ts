// Shared database core + the per-caller Session handle for the TS core (CLAUDE.md §3,
// spec/design/session.md §2.4, transactions.md §8/§10).
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
// Shape (the converged §2.4 design, parallel to the Rust/Go cores — SharedDb/ReadHandle/WriteHandle
// folded into two types):
//   - Database is the shared core; readSession() / writeSession() / session() mint independent
//     per-caller Sessions.
//   - SharedCore holds the published committed snapshot, the single-writer flag (a second writer
//     while one is open throws 25001 — JS cannot block the one thread, so the faithful analog of
//     "one writer at a time" is to reject, not wait), and the live-reader registry (§8).
//   - Session is the unified per-caller handle = the §3 envelope + a private Engine + an access mode:
//       - A READ ONLY session (readSession) pins the committed snapshot at mint and registers its
//         version; it serves reads from that pinned, immutable snapshot — unaffected by any later
//         commit — and a write through it is 25006. close() deregisters (no destructor in JS — the
//         caller calls it), advancing the watermark.
//       - A READ WRITE session (writeSession) holds the writer flag, captures the committed snapshot
//         as a private working set (an eager open READ WRITE block — the BEGIN READ WRITE form,
//         §2.4), and on commit publishes the working snapshot into the cell at the next version.
//         rollback / close discards it.
//       - A configured session (session) runs autocommit with the lazy gate: an autocommit read pins
//         the latest committed for that one statement (no gate); an autocommit write takes the gate
//         per statement (throwing 25001 if another writer is open), publishes, releases;
//         BEGIN/COMMIT/ROLLBACK open and end an explicit block.
//
// File-backed sharing (7c) reuses the same publish point plus the §9 persist chokepoint: the shared
// core carries the storage identity as the file-backed Engine that owns the pager + buffer pool + page
// accounting (null = in-memory), and a writer's publish routes through SharedCore.persist — the
// host-independent incremental copy-on-write recipe (persistImpl), a no-op in-memory. Isolation comes
// for free from the persistent (copy-on-write) stores (pmap.ts): a pinned snapshot is immutable and
// shares structure with later versions, so pinning is a reference copy, faulting clean pages through
// SharedPaging. Page reclamation stays watermark-safe trivially: the free-list is reconstruct-on-open
// only (every reusable page was dead at the opened version); continuous within-session reclamation is
// the deferred follow-on (transactions.md §8). No threads, so no concurrent-fault hazard (CLAUDE.md §2).
//
// The host-facing single handle is Database (the back-compat bridge — §2.1): the shared core PLUS one
// long-lived default Session, whose delegators (execute/query/begin/.../executeScript) drive that
// default session. newInMemory and the file.ts open/create wrappers return it; it is also the core
// itself (TS needs no Rust-style !Send split — single-threaded), so the same Database both drives the
// single-handle path and mints additional sessions.

import {
  DEFAULT_PAGE_SIZE,
  Engine,
  SessionState,
  Snapshot,
  stmtIsWrite,
  type Outcome,
  type SessionOptions,
  type TxStatus,
} from "./executor.ts";
import { type Rows, rowsFromOutcome, Transaction } from "./api.ts";
import { throwIfAborted } from "./cancel.ts";
import { engineError } from "./errors.ts";
import { persistImpl } from "./persist.ts";
import type { Statement } from "./ast.ts";
import type { ScriptSummary } from "./split.ts";
import type { Privileges, PrivilegeSet } from "./privileges.ts";
import type { ClockFunc, RandomFill } from "./seam.ts";
import type { Value } from "./value.ts";

// databaseFromSnapshot builds an in-memory handle whose committed roots are `snap` (the file
// snapshot) and `sharedTemp` (the database-wide shared-temp snapshot, temp-tables.md §5) — no file
// backing. A read handle keeps one with no open transaction (reads hit committed = the pinned
// snapshot); a write handle keeps one with an open READ WRITE block and publishes its working set.
function databaseFromSnapshot(snap: Snapshot, sharedTemp: Snapshot, pageSize: number): Engine {
  const db = new Engine();
  db.committed = snap;
  db.sharedTempCommitted = sharedTemp;
  // A minted session MUST serialize/split at the FILE's page size (not the in-memory default), so its
  // stores' cap matches the physical pages persist writes — and so every core builds byte-identical
  // file-backed databases (CLAUDE.md §8). In-memory the core's pageSize is the default, so this is a
  // no-op there.
  db.pageSize = pageSize;
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
  // storage is the file-backed Engine that owns the storage identity (the pager + buffer pool + the
  // mutable page accounting: paging/pageSize/pageCount/freePages); null is in-memory (persist is then
  // a no-op). Only those fields are used — its committed snapshot is unused; readers/writers carry the
  // published snapshot, whose stores already reference the same paging.
  storage: Engine | null = null;
  // readOnly marks a read-only file-backed core (api.md §2.1): every session is then read-only, a
  // write is 25006.
  readOnly = false;

  constructor(snap: Snapshot) {
    this.committed = snap;
    this.sharedTempCommitted = new Snapshot();
  }

  // pageSize is the file's page size for a file-backed core, else the in-memory default. Minted
  // sessions adopt it so their stores' cap matches the physical pages (CLAUDE.md §8).
  get pageSize(): number {
    return this.storage?.pageSize ?? DEFAULT_PAGE_SIZE;
  }

  // persist durably publishes snap to the backing store via an incremental copy-on-write commit
  // (persistImpl — the host-independent recipe, transactions.md §9). In-memory (no storage) is a no-op
  // success. Called from Session.publish; pageCount/freePages on the storage engine advance only after
  // both syncs succeed, so a write failure leaves the file's prior meta untouched.
  persist(snap: Snapshot): void {
    if (this.storage !== null) persistImpl(this.storage, snap);
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

// Database is the host-facing database handle (spec/design/session.md §2.1/§2.4): the shared core. It
// mints independent per-caller handles (readSession/writeSession/session); the durable per-connection
// state (transactions across calls, session variables, the envelope) lives on a Session, never on the
// Database. It also offers bare convenience methods (execute/query/executeScript/view/update) that mint
// a FRESH autocommit session per call and discard it: committed data persists through the shared core,
// but no session-local state carries to the next call. (TS is single-threaded, so no Rust-style !Send
// split is needed.)
export class Database {
  private core: SharedCore;

  private constructor(core: SharedCore) {
    this.core = core;
  }

  // over wraps a shared core as the host handle.
  private static over(core: SharedCore): Database {
    return new Database(core);
  }

  // newInMemory builds a fresh, empty in-memory database plus its default session (committed version 0).
  static newInMemory(): Database {
    return Database.over(new SharedCore(new Snapshot(0n)));
  }

  // fromFileEngine lifts a freshly opened/created file-backed Engine (file.ts) into a host handle: its
  // committed snapshot becomes the published roots and it becomes the storage owner (paging + page
  // accounting). Called by file.ts's createDatabase/openDatabase wrappers (file.ts is the node host
  // module; shared.ts stays browser-clean by not importing it).
  static fromFileEngine(engine: Engine): Database {
    const core = new SharedCore(engine.committed);
    core.sharedTempCommitted = engine.sharedTempCommitted;
    core.storage = engine;
    core.readOnly = engine.readOnly;
    return Database.over(core);
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

  // readSession opens a READ ONLY session over a consistent snapshot (spec/design/session.md §2.4,
  // transactions.md §10). Pins the committed snapshot now and registers its version in the live set;
  // the session serves reads from that snapshot for its life — observing one stable version even as a
  // writer interleaves commits — and a write through it is 25006. The caller must close() it to
  // deregister (advancing the watermark), since JS has no destructor. (The old SharedDb.read().)
  readSession(): Session {
    const snap = this.core.committed; // pin (immutable — no clone)
    this.core.register(snap.txid);
    // Pin both roots together (temp-tables.md §5): the reader sees a consistent file + shared-temp
    // view. Single-threaded JS reads both fields synchronously, so the pin is atomic.
    const engine = databaseFromSnapshot(snap, this.core.sharedTempCommitted, this.core.pageSize);
    return new Session(this.core, engine, "ro", snap.txid, snap.txid);
  }

  // writeSession opens a READ WRITE session with an eager open write block (spec/design/session.md
  // §2.4 — the BEGIN READ WRITE eager-gate form, transactions.md §10). A second writer while one is
  // open throws 25001 (JS cannot block the single thread, so the faithful single-writer analog is to
  // reject). Captures the committed snapshot as a private working set; commit publishes it, rollback
  // / close discards it. (The old SharedDb.write().)
  writeSession(): Session {
    if (this.core.readOnly) {
      // A read-only file has no writer (api.md §2.1); a "write" session degrades to a pinned read-only
      // one — a write through it is 25006, mirroring PostgreSQL hot standby.
      return this.readSession();
    }
    if (this.core.writerActive) {
      throw engineError("active_sql_transaction", "there is already a writer in progress");
    }
    this.core.writerActive = true;
    const base = this.core.committed;
    // committed/sharedTempCommitted = the immutable bases; beginTx clones them to working /
    // sharedTempWorking. Both roots are pinned together (temp-tables.md §5).
    const engine = databaseFromSnapshot(base, this.core.sharedTempCommitted, this.core.pageSize);
    engine.beginTx(true);
    return new Session(this.core, engine, "rw", base.txid, null, true);
  }

  // session mints an ADDITIONAL configured session over this database (spec/design/session.md
  // §2.1/§2.4), with its own envelope from opts. The session shares committed storage with every
  // other session over this Database, and runs autocommit with the lazy gate: an autocommit read
  // pins the latest committed for that one statement (no gate); an autocommit write takes the gate
  // per statement (throwing 25001 if another writer is open), publishes, releases;
  // BEGIN/COMMIT/ROLLBACK open and end an explicit block. (The old db.newSession swap → an
  // independent owns-its-Engine session.)
  session(opts: SessionOptions = {}): Session {
    const snap = this.core.committed;
    const engine = databaseFromSnapshot(snap, this.core.sharedTempCommitted, this.core.pageSize);
    engine.session = new SessionState(opts);
    // A read-only file-backed core mints read-only sessions (a write is 25006); it pins the committed
    // version in the watermark like a read session. A writable core mints the autocommit lazy-gate one.
    if (this.core.readOnly) {
      this.core.register(snap.txid);
      return new Session(this.core, engine, "ro", snap.txid, snap.txid);
    }
    return new Session(this.core, engine, "rw", snap.txid, null);
  }

  // --- Bare convenience methods (CLAUDE.md §2 / spec/design/session.md §2.4): each mints a FRESH
  // autocommit session, runs the statement, and discards it. Committed data persists through the shared
  // core; session-local state (an open block, session variables, currval, session-local temp tables)
  // does NOT carry to the next call — for durable connection state mint an explicit session(). ---

  // execute runs a (possibly mutating) statement on a fresh autocommit session, binding $N params.
  execute(sql: string, params: Value[] = []): Outcome {
    const s = this.session({});
    try {
      return s.execute(sql, params);
    } finally {
      s.close();
    }
  }
  // query runs a query on a fresh autocommit session, returning a row cursor (the rows are
  // materialized, so the cursor stays valid after the session is closed).
  query(sql: string, params: Value[] = []): Rows {
    const s = this.session({});
    try {
      return s.query(sql, params);
    } finally {
      s.close();
    }
  }
  // executeCancelable runs a statement on a fresh autocommit session under an AbortSignal
  // (spec/design/api.md §11.4): an already-aborted signal throws 57014 before any work. TS is
  // synchronous, so the check is at this boundary only (cancel.ts).
  executeCancelable(sql: string, params: Value[] = [], signal?: AbortSignal): Outcome {
    const s = this.session({});
    try {
      return s.executeCancelable(sql, params, signal);
    } finally {
      s.close();
    }
  }
  // queryCancelable is the query sibling of executeCancelable (spec/design/api.md §11.4).
  queryCancelable(sql: string, params: Value[] = [], signal?: AbortSignal): Rows {
    const s = this.session({});
    try {
      return s.queryCancelable(sql, params, signal);
    } finally {
      s.close();
    }
  }
  // executeScript runs a multi-statement script on a fresh autocommit session (spec/design/session.md
  // §4.2): the whole run is one implicit transaction (all-or-nothing).
  executeScript(sql: string): ScriptSummary {
    const s = this.session({});
    try {
      return s.executeScript(sql);
    } finally {
      s.close();
    }
  }
  // view runs fn in a READ ONLY transaction on a fresh session (scoped sugar, §2.2).
  view<R>(fn: (tx: Transaction) => R): R {
    const s = this.session({});
    try {
      return s.view(fn);
    } finally {
      s.close();
    }
  }
  // update runs fn in a READ WRITE transaction on a fresh session (scoped sugar, §2.2): the closure's
  // statements commit together, or roll back together on a thrown error.
  update<R>(fn: (tx: Transaction) => R): R {
    const s = this.session({});
    try {
      return s.update(fn);
    } finally {
      s.close();
    }
  }
  // close closes the backing file (file-backed only). The bare convenience methods autocommit, so
  // there is never uncommitted work to discard. Idempotent.
  close(): void {
    const st = this.core.storage;
    if (st !== null && st.paging !== null) {
      st.paging.close();
      st.paging = null;
    }
  }
}

// Access is the access mode a Session was minted with (spec/design/session.md §2.4/§5.1). Distinct
// from the privilege envelope (§5.3): "ro" is the coarse snapshot read-only mode (a write is 25006),
// the analogue of the old ReadHandle.
type Access = "ro" | "rw";

// Session is the unified per-caller handle (spec/design/session.md §2.4): the §3 envelope + a private
// Engine + an access mode.
export class Session {
  private core: SharedCore;
  // A private executor handle; engine.session is this session's envelope (SessionState).
  private engine: Engine;
  private access: Access;
  // Whether this session currently holds the single-writer flag.
  private gateHeld: boolean;
  // The live-registry version this session has registered (a read session, or an open READ ONLY
  // block); null otherwise. Deregistered on close/end.
  private pinnedVersion: bigint | null;
  // The committed version the current working set / pin is based on; the published version is
  // baseVersion+1 (the monotonic commit counter, transactions.md §8).
  private baseVersion: bigint;
  private closed = false;

  constructor(
    core: SharedCore,
    engine: Engine,
    access: Access,
    baseVersion: bigint,
    pinnedVersion: bigint | null,
    gateHeld = false,
  ) {
    this.core = core;
    this.engine = engine;
    this.access = access;
    this.baseVersion = baseVersion;
    this.pinnedVersion = pinnedVersion;
    this.gateHeld = gateHeld;
  }

  // execute runs a (possibly mutating) statement on this session, binding $N params (spec/design/
  // api.md §5). Routes by the session's state (read-only / open block / autocommit) with the
  // lazy-gate lifecycle (§2.4).
  execute(sql: string, params: Value[] = []): Outcome {
    return this.dispatch(this.engine.parse(sql), params);
  }

  // query runs a query on this session, returning a row cursor.
  query(sql: string, params: Value[] = []): Rows {
    return rowsFromOutcome(this.dispatch(this.engine.parse(sql), params));
  }

  // executeCancelable runs a statement under an AbortSignal (spec/design/api.md §11.4): if the signal
  // is already aborted it throws 57014 query_canceled before any work, else it runs normally. TS is
  // synchronous (one event loop), so the signal cannot flip mid-statement — the check is at this
  // boundary only, the deliberate per-language divergence from Go/Rust's mid-statement meter poll (the
  // cancel.ts note). Useful for skipping work an already-canceled caller no longer wants.
  executeCancelable(sql: string, params: Value[] = [], signal?: AbortSignal): Outcome {
    throwIfAborted(signal);
    return this.execute(sql, params);
  }

  // queryCancelable is the query sibling of executeCancelable (spec/design/api.md §11.4).
  queryCancelable(sql: string, params: Value[] = [], signal?: AbortSignal): Rows {
    throwIfAborted(signal);
    return this.query(sql, params);
  }

  private dispatch(stmt: Statement, params: Value[]): Outcome {
    if (this.access === "ro") {
      if (stmtIsWrite(stmt)) {
        throw engineError(
          "read_only_sql_transaction",
          "cannot execute a write statement against a read-only snapshot",
        );
      }
      return this.engine.executeStmtParams(stmt, params);
    }
    if (stmt.kind === "begin") return this.beginBlock(stmt.writable);
    if (stmt.kind === "commit") return this.endBlock(true);
    if (stmt.kind === "rollback") return this.endBlock(false);
    if (this.engine.session.tx !== null) {
      // Inside an open block (an eager write session, or this session after BEGIN): run on the
      // working set. The gate is already held for a writable block.
      return this.engine.executeStmtParams(stmt, params);
    }
    if (!stmtIsWrite(stmt)) {
      // Autocommit read: pin the latest committed for this one statement (PG-faithful); no gate.
      this.refreshCommitted();
      return this.engine.executeStmtParams(stmt, params);
    }
    // Autocommit write — the lazy gate (§2.4): take it (throwing 25001 if another writer is open),
    // capture the latest committed as the working base, run, publish on success, release.
    this.acquireGate();
    try {
      this.refreshCommitted();
      const out = this.engine.executeStmtParams(stmt, params);
      this.publish();
      return out;
    } finally {
      this.releaseGate();
    }
  }

  // beginBlock opens an explicit transaction block (spec/design/session.md §2.4). A writable block
  // acquires the writer gate eagerly (the BEGIN READ WRITE form) and bases its working set on the
  // latest committed; a READ ONLY block pins its snapshot and registers it in the watermark (like a
  // read session) without the gate.
  private beginBlock(writable: boolean | null): Outcome {
    const rw = writable ?? true;
    if (rw) {
      this.acquireGate();
      this.refreshCommitted();
    } else {
      this.refreshCommitted();
      this.core.register(this.baseVersion);
      this.pinnedVersion = this.baseVersion;
    }
    return this.engine.beginTx(writable);
  }

  // endBlock ends the open block (spec/design/session.md §2.4). commit: a clean writable block
  // publishes its working set at the next version; a failed/read-only block publishes nothing (a
  // failed COMMIT is a ROLLBACK, PostgreSQL). Either way the gate is released and any pin deregistered
  // — finishBlock runs in `finally` so a persist I/O failure (file-backed) never leaks the writer gate.
  private endBlock(commit: boolean): Outcome {
    try {
      if (commit) {
        const failed = this.engine.session.tx?.failed ?? false;
        const out = this.engine.commitTx(); // inner in-memory swap: committed := working
        if (!failed && this.gateHeld) this.publish(); // persist + publish; may throw on I/O failure
        return out;
      }
      return this.engine.rollbackTx();
    } finally {
      this.finishBlock();
    }
  }

  // acquireGate takes the single-writer flag, throwing 25001 if another writer is open (JS cannot
  // block its one thread — the faithful single-writer analog is to reject, transactions.md §10).
  private acquireGate(): void {
    if (this.core.writerActive) {
      throw engineError("active_sql_transaction", "there is already a writer in progress");
    }
    this.core.writerActive = true;
    this.gateHeld = true;
  }

  // releaseGate releases the single-writer flag (if held).
  private releaseGate(): void {
    if (this.gateHeld) {
      this.core.writerActive = false;
      this.gateHeld = false;
    }
  }

  // finishBlock releases the writer flag (if held) and deregisters the watermark pin (if registered)
  // — the shared-core bookkeeping common to ending a block, closing, and an un-ended session.
  private finishBlock(): void {
    this.releaseGate();
    if (this.pinnedVersion !== null) {
      this.core.deregister(this.pinnedVersion);
      this.pinnedVersion = null;
    }
  }

  // refreshCommitted re-pins the latest committed roots as this session's base (spec/design/
  // session.md §2.4): the autocommit read/write path always works against the newest committed state.
  private refreshCommitted(): void {
    this.baseVersion = this.core.committed.txid;
    this.engine.committed = this.core.committed;
    this.engine.sharedTempCommitted = this.core.sharedTempCommitted;
  }

  // publish stores the engine's committed roots into the shared cell at the next version (the §3
  // commit window — both roots together, temp-tables.md §5). Called after a clean autocommit write or
  // an explicit COMMIT of a writable block.
  //
  // File-backed: the new file snapshot is persisted durably first (core.persist) and the cell is
  // updated only on success, so a persist I/O failure throws and leaves the shared committed state (and
  // this session's version) unchanged. In-memory persist is a no-op. The shared-temp root is never
  // serialized — it rides the swap as a pure in-memory reference (temp-tables.md §2/§5).
  private publish(): void {
    const snap = this.engine.committed;
    snap.txid = this.baseVersion + 1n; // advance the shared version on every commit
    this.core.persist(snap); // durable before publish; throws (publishing nothing) on I/O failure
    this.engine.committed = snap;
    this.core.committed = snap;
    this.core.sharedTempCommitted = this.engine.sharedTempCommitted;
    this.baseVersion += 1n;
  }

  // begin opens an explicit transaction block on this session (spec/design/session.md §2.2 — the
  // host-API spelling of SQL BEGIN). writable true is READ WRITE (eager gate, the BEGIN READ WRITE
  // form); false is READ ONLY (pins + registers in the watermark, no gate). Statements then run on the
  // session until commit/rollback. A nested begin (a block is already open) is 25001.
  begin(writable: boolean): void {
    this.beginBlock(writable);
  }

  // commit commits an open write block / write session (publish + release the gate, §2.4). With no
  // open block this is a lenient no-op (PostgreSQL). The session stays usable (autocommit) afterward.
  commit(): void {
    if (this.engine.session.tx !== null) this.endBlock(true);
  }

  // rollback rolls back an open write block / write session (discard the working set + release the
  // gate, §2.4). With no open block this is a no-op.
  rollback(): void {
    if (this.engine.session.tx !== null) this.endBlock(false);
  }

  // close closes the session (spec/design/session.md §2.3): roll back any open block and deregister
  // its snapshot pin (advancing the watermark). Idempotent; the caller must call it (no destructor in
  // JS), idiomatically in a finally.
  close(): void {
    if (this.closed) return;
    this.closed = true;
    if (this.engine.session.tx !== null) this.endBlock(false);
    else this.finishBlock();
  }

  // view runs fn in a READ ONLY transaction on this session (bbolt-style, §2.2): open a read block,
  // run fn(tx), commit on success / roll back on a thrown error. A write inside is 25006.
  view<R>(fn: (tx: Transaction) => R): R {
    return this.withBlock(false, fn);
  }

  // update runs fn in a READ WRITE transaction on this session (bbolt-style, §2.2): open a write block
  // (eager gate), run fn(tx), publish on success / roll back on a thrown error — the safe default over
  // a raw begin.
  update<R>(fn: (tx: Transaction) => R): R {
    return this.withBlock(true, fn);
  }

  private withBlock<R>(writable: boolean, fn: (tx: Transaction) => R): R {
    this.beginBlock(writable);
    // The Transaction's commit/rollback route through this session (publish + gate release), not the
    // bare Engine swap, and are idempotent — so the wrapper's trailing commit/rollback is a no-op when
    // the closure already ended the block.
    const tx = new Transaction(this.engine, {
      commit: () => {
        if (this.engine.session.tx !== null) this.endBlock(true);
      },
      rollback: () => {
        if (this.engine.session.tx !== null) this.endBlock(false);
      },
    });
    try {
      const result = fn(tx);
      tx.commit();
      return result;
    } catch (e) {
      tx.rollback();
      throw e;
    }
  }

  // executeScript runs a multi-statement script on this session (spec/design/session.md §4.2): split
  // it, run each in order, discard rows, return the O(1) ScriptSummary. When the session is Idle the
  // whole run is one implicit transaction (all-or-nothing, published through the shared core); when it
  // is Open the run joins that transaction. In-script transaction control is 0A000.
  executeScript(sql: string): ScriptSummary {
    const ownsWrapper = this.engine.session.tx === null;
    if (ownsWrapper) this.beginBlock(true);
    try {
      const summary = this.engine.runScriptBody(sql);
      if (ownsWrapper) this.endBlock(true);
      return summary;
    } catch (e) {
      if (ownsWrapper) this.endBlock(false);
      throw e;
    }
  }

  // version is the snapshot version this session is currently based on (a read session's pinned
  // version, or the latest base for a writable session).
  get version(): bigint {
    return this.baseVersion;
  }

  // status is this session's transaction status (Idle/Open/Failed, spec/design/session.md §2.2).
  status(): TxStatus {
    return this.engine.session.status();
  }

  // inTransaction reports whether an explicit transaction block is open on this session.
  inTransaction(): boolean {
    return this.engine.session.tx !== null;
  }

  // --- The relocated envelope (spec/design/session.md §3): each accessor delegates to the private
  // engine's SessionState. ---

  get maxCost(): bigint {
    return this.engine.session.maxCost;
  }
  setMaxCost(limit: bigint): void {
    this.engine.session.maxCost = limit;
  }
  setLifetimeMaxCost(limit: bigint): void {
    this.engine.session.lifetime.limit = limit;
  }
  lifetimeMaxCost(): bigint {
    return this.engine.session.lifetime.limit;
  }
  lifetimeCost(): bigint {
    return this.engine.session.lifetime.total;
  }
  get maxSqlLength(): number {
    return this.engine.session.maxSqlLength;
  }
  setMaxSqlLength(bytes: number): void {
    this.engine.session.maxSqlLength = bytes;
  }
  get workMem(): number {
    return this.engine.session.workMem;
  }
  setWorkMem(bytes: number): void {
    this.engine.session.workMem = bytes;
  }
  setDefaultPrivileges(privs: PrivilegeSet): void {
    this.engine.session.setDefaultPrivileges(privs);
  }
  grant(privs: PrivilegeSet, object: string): void {
    this.engine.session.grant(privs, object);
  }
  revoke(privs: PrivilegeSet, object: string): void {
    this.engine.session.revoke(privs, object);
  }
  get privileges(): Privileges {
    return this.engine.session.privileges;
  }
  get allowDdl(): boolean {
    return this.engine.session.allowDdl;
  }
  setAllowDdl(allow: boolean): void {
    this.engine.session.allowDdl = allow;
  }
  setVar(name: string, value: string): void {
    this.engine.session.setVar(name, value);
  }
  resetVar(name: string): void {
    this.engine.session.resetVar(name);
  }
  var(name: string): string | undefined {
    return this.engine.session.var(name);
  }
  setTimeZone(zone: string): void {
    this.engine.session.setTimeZone(zone);
  }
  setRandomSource(f: RandomFill): void {
    this.engine.session.seam.randomFill = f;
  }
  clearRandomSource(): void {
    this.engine.session.seam.randomFill = undefined;
  }
  setClockSource(f: ClockFunc): void {
    this.engine.session.seam.clock = f;
  }
  clearClockSource(): void {
    this.engine.session.seam.clock = undefined;
  }
}
