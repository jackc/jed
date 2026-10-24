import { runSync, type Steps, uncancelableSteps } from "./execution.ts";
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
// SharedPaging. Continuous within-session reclamation (v25) makes the watermark gate load-bearing:
// free-list reuse is gated by freeGenTxid (a page dead at the list's generation is reused only once
// oldest_live ≥ generation, transactions.md §8). No threads, so pin registration is already atomic — no
// concurrent-fault hazard (CLAUDE.md §2); only the reuse gate is needed here.
//
// The host-facing single handle is Database (the back-compat bridge — §2.1): the shared core PLUS one
// long-lived default Session, whose delegators (execute/query/begin/.../executeScript) drive that
// default session. The file.ts createDatabase/openDatabase wrappers return it; it is also the core
// itself (TS needs no Rust-style !Send split — single-threaded), so the same Database both drives the
// single-handle path and mints additional sessions.

import type { FileHost } from "./filehost.ts";
import type { TimeZoneInfo } from "./timezone.ts";
import {
  type Attachment,
  type CollationInfo,
  Engine,
  SessionState,
  Snapshot,
  type Outcome,
  type InsertCacheHolder,
  type ScanCacheHolder,
  type SessionOptions,
  type TxStatus,
  type FileCoordinatorHost,
} from "./executor.ts";
import { drainRunSteps, PreparedStatement, Rows, rowsFromOutcome, Transaction } from "./api.ts";
import {
  loadEngine,
  loadEnginePaged,
  loadEnginePagedSteps,
  metaAuthenticates,
  newAttachedStorage,
  newScratchStorage,
  prefetchSnapshotSteps,
  reclaimDue,
  toImage as toImageBytes,
} from "./format.ts";
import type { CompositeType, Table, ViewDef } from "./catalog.ts";
import {
  type Argon2Params,
  type Credential,
  createCodecSteps,
  DEFAULT_ARGON2_PARAMS,
  ENCRYPTED_MIN_PAGE_SIZE,
  type PageCodec,
  scratchCodec,
} from "./crypto.ts";
import { capOf } from "./cap.ts";
import type { SpillSink } from "./spill.ts";
import { Environment } from "./environment.ts";
import type { Row } from "./storage.ts";
import type { Cursor } from "./cursor.ts";
import { throwIfAborted } from "./cancel.ts";
import {
  type JsParam,
  type Row as ErgoRow,
  type RunResult,
  Statement as ErgoStatement,
} from "./ergonomic.ts";
import { metaChecksum } from "./commit_manifest.ts";
import type { BlockStore } from "./blockstore.ts";
import { EngineError, engineError } from "./errors.ts";
import { MemoryBlockStore } from "./memoryblockstore.ts";
import { Pager } from "./pager.ts";
import { SharedPaging } from "./paging.ts";
import {
  adoptStaged,
  type BudgetCtx,
  persistImpl,
  persistSteps,
  precheckBudgetSteps,
  type StagedCommit,
  stageTemp,
  storageBytes,
  writeKeyBlockSteps,
} from "./persist.ts";
import type { Statement } from "./ast.ts";

// afterPersistHook is a test-only seam (null in production, a single null-check per commit): it fires in
// Session.publish AFTER the durable persist but BEFORE the committed-root swap — the window in which the
// just-committed version is durable yet not yet published, so a reader that pins here still gets the PRIOR
// committed version (the reuse-gate fallback-reader race, transactions.md §8). It receives the still-
// published committed txid and the storage's new free-list generation, so a test can detect a compacting
// commit and pin a fallback reader synchronously (JS is single-threaded — no thread coordination needed).
// The deterministic pin-registration regression test installs a closure.
export let afterPersistHook: ((committedTxid: bigint, freeGenTxid: bigint) => void) | null = null;
export function setAfterPersistHook(
  h: ((committedTxid: bigint, freeGenTxid: bigint) => void) | null,
): void {
  afterPersistHook = h;
}
import type { ScriptSummary } from "./split.ts";
import type { Privileges, PrivilegeSet } from "./privileges.ts";
import { renderIn, type Value } from "./value.ts";

// ServiceEngineFacts are the owner's page-accounting facts its service needs to grant free-page reuse
// and to decide a maintenance commit (spec/pageservice/README.md §3.5, §6.4): the free list's
// generation, the live count at the last reclamation, and the oldest version a local reader pins (null
// when none does).
export type ServiceEngineFacts = {
  freeGenTxid: bigint;
  liveAtCompaction: number;
  localOldest: bigint | null;
};

// ServicePending is what the owner's engine thread adopts from client commits (README §6.4): the
// newest published txid and its meta, and every page index written by a commit after `since` (all:
// the window no longer covers them).
export type ServicePending = { txid: bigint; meta: Uint8Array; all: boolean; indices: number[] };

// ServiceOwnerHandle is a serving page-service owner as the core sees it (spec/design/page-service.md
// §4.1, §14; the Node implementation is pageservice/owner.ts, injected by file.ts so this module stays
// browser-clean). Its lock orders client pins against the core's commit publication and watermark; its
// writer gate is shared by the owner's writers, compaction, and client write transactions (README §4).
export interface ServiceOwnerHandle {
  lock(): void;
  unlock(): void;
  // oldestPinnedLocked is the oldest version any client connection pins, or null (lock held).
  oldestPinnedLocked(): bigint | null;
  // published records a main-database commit: its version, meta page, written page indices, and the
  // engine's facts after it.
  published(txid: bigint, meta: Uint8Array, written: number[], facts: ServiceEngineFacts): void;
  // compactedLocked resets the service after compaction published txid over paging (lock held).
  compactedLocked(txid: bigint, paging: SharedPaging, facts: ServiceEngineFacts): void;
  // epoch changes whenever a client commit is published or the gate is handed to the engine thread:
  // one atomic load, the owner's whole per-transaction cost while nothing changed.
  epoch(): number;
  // pending reports the client commits published after since.
  pending(since: bigint): ServicePending;
  // acquireGate waits for the writer gate at most timeoutMs (0: no deadline; 55P03 on expiry). It
  // returns true when the gate was handed over for a maintenance commit rather than free.
  acquireGate(timeoutMs: number): boolean;
  tryGate(): boolean;
  // takeHanded takes the gate when the service handed it to the engine thread.
  takeHanded(): boolean;
  releaseGate(): void;
  // poisoned reports a failed client commit (the owner stopped serving); stopServing poisons it.
  poisoned(): boolean;
  stopServing(): void;
  // clearLocalOldest records that no local reader pins an old version any more.
  clearLocalOldest(): void;
  // onHandoff installs the engine thread's handler for a gate handed over between transactions.
  onHandoff(handler: () => void): void;
  // hangUpNextAck arms the commit-resolution test hook (README §6.4); crashBeforeNextAck makes the
  // process exit instead of acknowledging the next client commit (README §6.5).
  hangUpNextAck(): void;
  crashBeforeNextAck(): void;
  // close stops serving (clients see EOF) and removes the endpoint and owner record.
  close(): void;
  // --- handoff (spec/pageservice/README.md §6.5) ---
  // onYield installs the engine thread's handler for an accepted ownership request.
  onYield(handler: () => void): void;
  // onLocalOldest installs the oldest-local-pin reader a same-thread service (the simulator) reads.
  onLocalOldest(oldest: () => bigint | null): void;
  // admitting: the owner serves normally (no drain); yielding: it drains for a yield.
  admitting(): boolean;
  yielding(): boolean;
  // beginRelease ends a yield's drain: true when still yielding (now releasing), false when the
  // requester disconnected and the yield was cancelled.
  beginRelease(): boolean;
  // beginClosing starts a graceful close's drain (notice kind 3 to every connection).
  beginClosing(): void;
  // clientsIdle: no connection pins a snapshot or holds the writer gate (own: nor does this thread).
  clientsIdle(own: boolean): boolean;
  // resolve answers commit_status from the owner's resolution window (README §3.5).
  resolve(txid: bigint, digest: bigint): number;
}

// ServiceRole is what a page-service role change installs (spec/pageservice/README.md §6.5): the
// main domain's storage for the role (null for none, between roles) and the owner's service and
// presence coordinator, or the client's connection.
export type ServiceRole = {
  kind: "owner" | "client" | "none";
  engine: Engine | null;
  service: ServiceOwnerHandle | null;
  client: PageClientHandle | null;
  coordinator: FileCoordinatorHost | null;
};

// ServiceHandoffHost is a handle's page-service state that outlives its roles (the Node
// implementation is pageservice/handoff.ts, injected by file.ts so this module stays browser-clean):
// the ownership preference and grace periods, and the election a role change runs.
export interface ServiceHandoffHost {
  // path is the database's resolved real path, the writer-gate order key (page-service.md §5.5).
  readonly path: string;
  readonly ownership: "prefer" | "yield" | "never";
  readonly yieldGraceMs: number;
  readonly shutdownGraceMs: number;
  // relaxed is the handle's synchronous = off, whatever role it holds (validated-cow.md §4).
  readonly relaxed: boolean;
  // skipFsync is the handle's fsync=off open setting (a backup's file skips its barriers too).
  readonly skipFsync: boolean;
  // elect runs the election loop under the open timeout, keeping the path claim: it returns the new
  // role (an owner opened with recovery and serving, or an opened client). 55006 on timeout.
  elect(pauseUntilMs: number, held?: PageCodec | null): ServiceRole;
  // canOpenReadWrite reports whether this user can open the file read-write (opened and closed).
  canOpenReadWrite(): boolean;
  // releasePath releases the process-local open-path claim when the handle closes.
  releasePath(): void;
  // background: a client re-elects on the event loop as soon as its connection ends (false: only at
  // its next transaction begin — the simulator's choice, so every re-election runs in trace order).
  readonly background: boolean;
  // nowMicros and pause are the lock host's clock and poll pause, which a close's drain reads.
  nowMicros(): number;
  pause(): void;
}

// HANDOFF_PAUSE_MS is how long a yielding owner skips presence tries after releasing ownership, so the
// requester takes it (README §6.5); DRAIN_POLL_MS is how often a drain checks for running
// transactions.
const HANDOFF_PAUSE_MS = 2000;
const DRAIN_POLL_MS = 10;

function sleepMs(ms: number): void {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

function isCode(error: unknown, code: string): boolean {
  return error instanceof EngineError && error.code() === code;
}

function errStaleRole(): Error {
  return engineError(
    "connection_failure",
    "the transaction began before this process's connection to the database owner changed",
  );
}

function errNoOwner(): Error {
  return engineError(
    "connection_failure",
    "lost the connection to the database owner, and no process took ownership in time",
  );
}

// RevokedBlockStore is the byte store of storage whose role ended (README §6.5): the process no longer
// owns the file, so every access fails 08006. Pages already in the pool still serve the snapshots that
// cached them.
class RevokedBlockStore implements BlockStore {
  private fail(): never {
    throw engineError("connection_failure", "this process no longer owns the database");
  }
  readAt(_offset: number, _len: number): Uint8Array {
    this.fail();
  }
  writeAt(_offset: number, _bytes: Uint8Array): void {
    this.fail();
  }
  sync(): void {
    this.fail();
  }
  size(): number {
    this.fail();
  }
  setSize(_bytes: number): void {
    this.fail();
  }
  close(): void {}
}

// ClientReadBegun is a page client's begin_read reply as the core uses it (spec/pageservice/README.md
// §3.2): invalidateAll also covers an incarnation change.
export type ClientReadBegun = {
  txid: bigint;
  incarnation: Uint8Array;
  invalidateAll: boolean;
  indices: number[];
  meta: Uint8Array;
};

// PageClientHandle is a page client as the core sees it (spec/design/page-service.md §4.2; the Node
// implementation is pageservice/client.ts). held is the set of versions it pins at the owner.
export interface PageClientHandle {
  readonly held: Set<bigint>;
  readonly adopted: bigint;
  // readOnly is whether the connection is read-only (hello_ok).
  readonly readOnly: boolean;
  // reuseGrant is the current write transaction's reuse grant (README §3.5).
  readonly reuseGrant: boolean;
  // beginRead runs begin_read and records the pin it carries (a duplicate is released at once).
  beginRead(): ClientReadBegun;
  // beginWrite runs begin_write (waiting at most timeoutMs for the owner's gate) and holds the write
  // pin it carries until endWrite or the commit.
  beginWrite(timeoutMs: number): ClientReadBegun;
  // endWrite ends a write transaction whose commit never reached the owner (abort).
  endWrite(): void;
  // setMeta makes meta serve page indices 0 and 1 (before a reload reads it); markAdopted records the
  // version the cache now corresponds to.
  setMeta(meta: Uint8Array): void;
  markAdopted(incarnation: Uint8Array, txid: bigint): void;
  // adoptOwnCommit records this process's own published commit as its adopted version.
  adoptOwnCommit(txid: bigint, meta: Uint8Array): void;
  // discardPending drops a commit's buffered pages after a failure; failedCommit unwraps a commit
  // failure, saying whether the owner certainly did not commit it.
  discardPending(): void;
  failedCommit(error: unknown): { error: unknown; notCommitted: boolean };
  // releasePin sends end_read for a held version.
  releasePin(v: bigint): void;
  // readPages fetches a batch of at most maxReadPages() pages under the newest pinned version (a
  // backup's copy, spec/pageservice/README.md §6.6).
  readPages(indices: number[]): Uint8Array[];
  maxReadPages(): number;
  close(): void;
  // --- handoff (spec/pageservice/README.md §6.5) ---
  // ownerReadOnly and ownerYields are the owner's hello_ok flags.
  readonly ownerReadOnly: boolean;
  readonly ownerYields: boolean;
  // lost: the connection ended; waitForLoss blocks until it does or deadlineMs passes.
  lost(): boolean;
  waitForLoss(deadlineMs: number): boolean;
  // onLost installs the event-loop handler for the connection's end.
  onLost(handler: () => void): void;
  // handoffPauseUntil is when the handoff pause after this connection's end ends (0: none).
  handoffPauseUntil(): number;
  // requestOwnershipNoWait sends request_ownership without waiting; askOwnership waits for the reply.
  requestOwnershipNoWait(): void;
  askOwnership(): void;
  // commitStatus asks the owner whether a commit happened (README §3.5).
  commitStatus(txid: bigint, digest: bigint): number;
  // lostCommit recognizes a commit failure whose acknowledgement was lost.
  lostCommit(error: unknown): { txid: bigint; digest: bigint; failure: Error } | null;
}

const pathEncoder = new TextEncoder();
function compareFilePaths(left: string, right: string): number {
  const a = pathEncoder.encode(left);
  const b = pathEncoder.encode(right);
  const length = Math.min(a.length, b.length);
  for (let index = 0; index < length; index++) {
    if (a[index] !== b[index]) return a[index]! - b[index]!;
  }
  return a.length - b.length;
}

// databaseFromSnapshot builds a session-local handle whose committed root is `snap`. It retains the
// shared storage identity's independent host scratch sink for spill; the snapshot's stores continue
// to own paging and the database path remains persistence-only.
// A read handle keeps no open transaction (reads hit committed = the pinned snapshot); a write handle
// keeps an open READ WRITE block and publishes its working set.
function databaseFromSnapshot(snap: Snapshot, core: SharedCore): Engine {
  const db = new Engine(core.environment);
  db.committed = snap;
  // A minted session MUST serialize/split at the FILE's page size (not the in-memory default), so its
  // stores' cap matches the physical pages persist writes — and so every core builds byte-identical
  // file-backed databases (CLAUDE.md §8). In-memory the core's pageSize is the default, so this is a
  // no-op there.
  db.pageSize = core.pageSize;
  // work_mem spill is a host property of the shared storage identity. Minted sessions retain its
  // scratch sink independently of the persistence path; otherwise a file-backed ORDER BY silently
  // becomes in-memory-only. The snapshot's stores already carry paging; installing the storage
  // owner's pager here would bypass its page accounting.
  db.path = core.storage.path;
  db.spillSink = core.storage.spillSink;
  db.readOnly = core.readOnly;
  return db;
}

// SharedCore is the state shared by every handle minted from one Database: the published committed
// root (the file snapshot, transactions.md §2), the single-writer flag, and the live-reader registry
// (transactions.md §8). Not exported — only the handles touch it.
class SharedCore {
  committed: Snapshot;
  // attached is the published committed root of every host-attached DATABASE-scoped in-memory database
  // (spec/design/attached-databases.md §5), keyed by exact name. A minted session captures this map
  // (with the committed root) so it sees a CONSISTENT cross-database snapshot; attach/detach REPLACE it
  // (never mutate in place) so a pinned reader is unaffected. A publish swaps the committing session's
  // adopted attached view in. Empty when nothing is attached — byte-for-byte the pre-attachment behavior.
  // Session-local `temp` is NOT here (it is session-private, on SessionState.tempCommitted).
  attached = new Map<string, Snapshot>();
  // attachments is the core-owned registry of host-attached databases (attached-databases.md §2/§5),
  // keyed by exact name — each attachment's MUTABLE storage identity (a MemoryBlockStore Engine,
  // like the temp domain) + its write mode. The immutable published root lives in `attached` under the
  // same key. Populated by Database.attach / cleared by Database.detach. Read by the executor via
  // Engine.core (the structural AttachmentCore view) during a commit persist.
  attachments = new Map<string, Attachment>();
  // live maps a pinned snapshot version to its reader refcount; its minimum key is the reclamation
  // watermark (several readers may pin the same version).
  live = new Map<bigint, number>();
  // writerActive is true while a write transaction is open (at most one — CLAUDE.md §3).
  writerActive = false;
  // storage is the Engine that owns the storage identity (the pager + buffer pool + the mutable page
  // accounting: paging/pageSize/pageCount/freePages) — since B3 (bplus-reshape.md) EVERY core has
  // one: a file-backed core over a FileBlockStore, an in-memory core over a MemoryBlockStore (with a
  // pinned, unbounded pool — an in-memory database is resident by definition). So the commit path is
  // one path: persist packs dirty pages into the byte store either way, and the store's sync is what
  // durability means for that host (a no-op in memory). Only those fields are used — its committed
  // snapshot is unused; readers/writers carry the published snapshot, whose stores already reference
  // the same paging.
  storage: Engine;
  // readOnly marks a read-only file-backed core (api.md §2.1): every session is then read-only, a
  // write is 25006. Always false for an in-memory core.
  readOnly = false;
  coordinator: FileCoordinatorHost | null;
  // service is set on a page-service owner, client on a page client (spec/design/page-service.md
  // §4); at most one is set, and only under the page service (`locking = "auto"`). They are the
  // current role's (spec/pageservice/README.md §6.5): handoff is set under the page service, and
  // roleKind/roleGen name the current role — owner, client, or none between roles. A role change
  // installs fresh main storage and committed snapshot together with the role and starts an empty
  // live registry; a pin of an older role deregisters as a no-op, and a commit whose writer began
  // under an older role fails 08006 without writing.
  service: ServiceOwnerHandle | null = null;
  client: PageClientHandle | null = null;
  handoff: ServiceHandoffHost | null = null;
  roleKind: "owner" | "client" | "none" = "none";
  roleGen = 0;
  // handoffClosed is set when the handle closes; pauseUntilMs is the handoff pause of a role with no
  // connection (after a yield released ownership).
  handoffClosed = false;
  private pauseUntilMs = 0;
  // writerGen is the role generation the current writer took its gate under; gateService and
  // writerClient are the service whose gate this thread holds and the client whose write transaction
  // is open, so a role change never releases the new role's gate.
  private writerGen = 0;
  private gateService: ServiceOwnerHandle | null = null;
  private writerClient: PageClientHandle | null = null;
  // A yield's drain (README §6.5): its deadline, and the event-loop timer polling it.
  private yieldDeadline: number | null = null;
  private yieldTimer: ReturnType<typeof setTimeout> | null = null;
  // On a page-service owner: the service epoch it last caught up with, and whether its last
  // publication reported a local reader to the service.
  private serviceEpoch = 0;
  private serviceLocalOldest = false;
  planEpoch = 0;
  // serviceAttachCount counts the file attachments under the page service (page-service.md §5.5).
  // heldGates and writerPins are the current writer's: the gates it took after the in-process writer
  // flag, in real-path order, and its pins on read-only service attachments.
  serviceAttachCount = 0;
  private heldGates: GateRef[] = [];
  private writerPins: AttachPin[] = [];
  // name is the attachment an inner handle serves ("" for a host-facing handle: main).
  name = "";
  // The handle's Environment (spec/design/environment.md §3): fixed at open/create and shared (by
  // reference) by every minted session, attachment, and temp domain — the storage owner's.
  readonly environment: Environment;

  constructor(snap: Snapshot, storage: Engine, coordinator: FileCoordinatorHost | null = null) {
    this.committed = snap;
    this.storage = storage;
    this.coordinator = coordinator;
    this.environment = storage.environment;
  }

  acquireWriter(timeoutMs: number): void {
    this.acquireWriterScoped(timeoutMs, true);
  }

  // acquireWriterScoped takes the in-process writer flag, then (when gates is set) the gates of main and
  // of the service attachments, in real-path order (page-service.md §5.5). Attach and detach take only
  // the in-process flag: they change only this handle's registry, and waiting there on a gate another
  // process's writer holds could deadlock against that process's own attach.
  acquireWriterScoped(timeoutMs: number, gates: boolean): void {
    if (this.writerActive) {
      throw engineError("active_sql_transaction", "there is already a writer in progress");
    }
    this.writerActive = true;
    if (gates && (this.handoff !== null || this.serviceAttachCount > 0)) {
      // Under the page service a writer takes the owner's gate — its own, or through begin_write — and
      // an owner then adopts any client commit published since its last transaction (page-service.md
      // §5.5, §7; README §6.5).
      try {
        this.acquireGates(timeoutMs);
      } catch (error) {
        this.writerActive = false;
        throw error;
      }
    }
    try {
      this.checkPid();
    } catch (error) {
      this.releaseWriter();
      throw error;
    }
  }

  // acquireGates takes, after the in-process writer flag the caller holds, the writer gate of main
  // (when main is under the page service) and of every read-write service attachment, in real-path
  // byte order (page-service.md §5.5). Each wait is bounded by timeoutMs; a failure releases the gates
  // already held in reverse order. Read-only service attachments are then refreshed and pinned for the
  // writer, in real-path order.
  private acquireGates(timeoutMs: number): void {
    const gates: GateRef[] = [];
    if (this.handoff !== null) gates.push({ path: this.handoff.path, att: null });
    const readOnly: ServiceAttachment[] = [];
    for (const svc of this.serviceAttachments()) {
      if (svc.readOnly) readOnly.push(svc);
      else gates.push({ path: svc.realPath, att: svc });
    }
    // A stable sort: main's gate and an attachment's never share a path (one open per path).
    gates.sort((a, b) => compareFilePaths(a.path, b.path));
    const held: GateRef[] = [];
    for (const gate of gates) {
      try {
        if (gate.att === null) this.beginServiceWrite(timeoutMs);
        else gate.att.core.acquireWriter(timeoutMs);
      } catch (error) {
        if (gate.att === null) this.releaseMainGate();
        this.releaseGates(held);
        throw error;
      }
      held.push(gate);
    }
    const pins: AttachPin[] = [];
    for (const svc of readOnly) {
      try {
        pins.push(pinServiceAttachment(svc));
      } catch (error) {
        releaseAttachPins(pins);
        this.releaseGates(held);
        throw error;
      }
    }
    this.heldGates = held;
    this.writerPins = pins;
  }

  // releaseGates releases held gates in reverse order.
  private releaseGates(held: GateRef[]): void {
    for (let i = held.length - 1; i >= 0; i--) {
      const gate = held[i]!;
      if (gate.att === null) this.releaseMainGate();
      else gate.att.core.releaseWriter();
    }
  }

  // releaseMainGate ends main's page-service write: a client's open write transaction, and the owner's
  // gate this thread holds. A no-op when neither is held.
  private releaseMainGate(): void {
    this.writerClient?.endWrite();
    this.writerClient = null;
    this.releaseServiceGate();
  }

  // --- file attachments under the page service (page-service.md §5.5; README §6.6) ---

  // serviceAttachments lists the attachments under the page service in real-path byte order.
  serviceAttachments(): ServiceAttachment[] {
    if (this.serviceAttachCount === 0) return [];
    const out: ServiceAttachment[] = [];
    for (const att of this.attachments.values()) {
      if (att.svc !== null) out.push(att.svc as ServiceAttachment);
    }
    out.sort((a, b) => compareFilePaths(a.realPath, b.realPath));
    return out;
  }

  // attachedRoot is the committed root of attachment name as the handle knows it now: a service
  // attachment's inner handle's committed snapshot, any other attachment's published root.
  attachedRoot(name: string): Snapshot | undefined {
    const svc = this.attachments.get(name)?.svc ?? null;
    if (svc !== null) return (svc as ServiceAttachment).core.committed;
    return this.attached.get(name);
  }

  // hasSharedCoordinator reports whether other processes may commit to this database or one of its
  // attachments: true under the page service.
  hasSharedCoordinator(): boolean {
    return this.handoff !== null || this.serviceAttachCount > 0;
  }

  // refreshAndPinAll is a read begin over main and every service attachment: main first, then each
  // attachment in real-path order (refreshAndPin for each). It returns main's pinned version and role
  // generation and the attachment pins.
  refreshAndPinAll(): { version: bigint; gen: number; atts: AttachPin[] } {
    const version = this.refreshAndPin();
    const gen = this.roleGen;
    const atts: AttachPin[] = [];
    for (const svc of this.serviceAttachments()) {
      try {
        atts.push(pinServiceAttachment(svc));
      } catch (error) {
        this.deregister(version, gen);
        releaseAttachPins(atts);
        throw error;
      }
    }
    return { version, gen, atts };
  }

  // cursorAttachPins are the attachment pins a cursor takes beside its main pin: one more on each of
  // the reader's pins, and for a writer, one more on each read-only attachment's pin and one on each
  // gated attachment's committed snapshot (its base).
  cursorAttachPins(readerPins: AttachPin[], writer: boolean): AttachPin[] {
    if (!writer) return addAttachPins(readerPins);
    const pins = addAttachPins(this.writerPins);
    for (const gate of this.heldGates) {
      if (gate.att === null) continue;
      const inner = gate.att.core;
      const v = inner.committed.txid;
      inner.register(v);
      pins.push({ name: gate.att.name, core: inner, gen: inner.roleGen, v, snap: inner.committed });
    }
    return pins;
  }

  // writerView returns the attached roots with the writer's service attachments overlaid: a gated
  // attachment's committed snapshot (current: no other process can commit it) and a read-only one's
  // pinned snapshot. Caller holds the writer gates.
  writerView(): Map<string, Snapshot> {
    if (this.heldGates.length === 0 && this.writerPins.length === 0) return this.attached;
    const attached = new Map(this.attached);
    for (const gate of this.heldGates) {
      if (gate.att !== null) attached.set(gate.att.name, gate.att.core.committed);
    }
    for (const pin of this.writerPins) attached.set(pin.name, pin.snap);
    return attached;
  }

  // writerStale throws 08006 when an attachment the writer holds the gate or a pin of changed role
  // since (README §6.5). Caller holds the writer gates.
  writerStale(): void {
    for (const gate of this.heldGates) {
      if (gate.att === null) continue;
      const inner = gate.att.core;
      if (inner.writerRoleChanged() || inner.client?.lost()) throw errStaleRole();
    }
    attachPinsStale(this.writerPins);
  }

  // commitServiceAttachment commits a service attachment's working snapshot through this inner handle,
  // under the gate the outer writer holds (page-service.md §5.5): the role decides the path — an
  // owner's validated-COW commit, or a client's CommitDelta through its owner, with a lost
  // acknowledgement resolved by re-election — then publishes it as a main commit is published. It
  // returns the attachment's committed snapshot afterwards.
  commitServiceAttachment(snap: Snapshot, stagesRows: boolean): Snapshot {
    this.persist(snap, stagesRows);
    if (this.writerRoleChanged()) {
      // A lost acknowledgement resolved as committed under a new role: that role's committed state
      // already holds the commit (README §6.5).
      return this.committed;
    }
    snap.demoteCleanLeaves();
    if (this.client === null || snap.txid > this.committed.txid) this.committed = snap;
    if (this.service !== null) this.publishService(snap.txid);
    if (this.client !== null && this.storage.lastCommitMeta !== null) {
      this.client.adoptOwnCommit(snap.txid, this.storage.lastCommitMeta);
    }
    return this.committed;
  }

  // planEpochNow is the handle's plan-cache epoch: its own, plus every service attachment's inner one,
  // so an inner handle's adoption of another process's catalog invalidates this handle's cached plans.
  planEpochNow(): number {
    let epoch = this.planEpoch;
    for (const svc of this.serviceAttachments()) epoch += svc.core.planEpoch;
    return epoch;
  }

  // domainName is the database name this core's storage-limit errors name: main, or the attachment an
  // inner handle serves.
  domainName(): string {
    return this.name !== "" ? this.name : "main";
  }

  // spillsPlaintext reports whether this handle has a scratch target that is not sealed (scratch
  // encryption off, spec/fileformat/encrypted.md §11): an encrypted database may not join it.
  spillsPlaintext(): boolean {
    const sink = this.storage.spillSink;
    return sink !== null && !(sink.encrypts?.() ?? false);
  }

  // tryAcquireLocalWriter claims the in-process writer flag only if it is free right now, with no
  // coordination work (compaction never waits and does its own coordination — spec/design/api.md §2.6).
  // Released by releaseWriter.
  tryAcquireLocalWriter(): boolean {
    if (this.writerActive) return false;
    this.writerActive = true;
    return true;
  }

  // releaseWriter releases the writer's read-only attachment pins, then its gates in reverse order
  // (main's included; compaction's gate too), then the in-process writer flag.
  releaseWriter(): void {
    if (!this.writerActive) return;
    const pins = this.writerPins;
    const gates = this.heldGates;
    this.writerPins = [];
    this.heldGates = [];
    try {
      releaseAttachPins(pins);
      this.releaseGates(gates);
      this.releaseMainGate();
    } finally {
      this.writerActive = false;
    }
  }

  // --- page-service roles and handoff (spec/pageservice/README.md §6.5) ---

  // installRole publishes a new role with its storage and committed snapshot (none keeps the old,
  // revoked storage), starting an empty live registry.
  installRole(role: ServiceRole, pauseUntilMs = 0): void {
    this.roleGen++;
    this.roleKind = role.kind;
    this.service = role.service;
    this.client = role.client;
    this.coordinator = role.coordinator;
    this.pauseUntilMs = pauseUntilMs;
    this.live = new Map();
    this.serviceEpoch = role.service?.epoch() ?? 0;
    this.serviceLocalOldest = false;
    this.yieldDeadline = null;
    if (this.yieldTimer !== null) clearTimeout(this.yieldTimer);
    this.yieldTimer = null;
    if (role.engine !== null) {
      const engine = role.engine;
      engine.reclaimWithinSession = true;
      engine.storageBudget.limit = this.storage.storageBudget.limit;
      this.storage = engine;
      this.committed = engine.committed;
    }
    this.planEpoch++;
    this.watchRole();
  }

  // watchRole wires the current role's events: an owner's handed gate and accepted ownership request,
  // a client's connection end (re-elected on the event loop). A client releases its open-time pin and
  // asks a yielding owner for ownership when it prefers to own (README §6.5 case 1).
  private watchRole(): void {
    const gen = this.roleGen;
    const service = this.service;
    if (service !== null) {
      service.onHandoff(() => this.serviceHandoff());
      service.onYield(() => this.yieldRequested(gen));
      service.onLocalOldest(() => {
        let oldest: bigint | null = null;
        for (const v of this.live.keys()) if (oldest === null || v < oldest) oldest = v;
        return oldest;
      });
    }
    const client = this.client;
    if (client !== null) {
      this.releaseIdlePins();
      if (this.handoff?.background !== false) client.onLost(() => this.backgroundReelect(gen));
      if (this.handoff?.ownership === "prefer" && client.ownerYields) {
        client.requestOwnershipNoWait();
      }
    }
  }

  // backgroundReelect re-elects on the event loop when role gen's connection ended (README §6.5).
  private backgroundReelect(gen: number): void {
    if (gen !== this.roleGen || this.handoffClosed || this.client === null || !this.client.lost()) {
      return;
    }
    try {
      this.reelect();
    } catch {
      // The next transaction begin re-elects again.
    }
  }

  // reelect replaces the current role — a client whose connection ended, or none after a yield — by
  // running the election under the open timeout. With no owner in time the role stays: 08006.
  reelect(): void {
    const handoff = this.handoff!;
    if (this.handoffClosed) {
      throw engineError("connection_failure", "the database handle is closed");
    }
    const pause = this.client !== null ? this.client.handoffPauseUntil() : this.pauseUntilMs;
    let role: ServiceRole;
    try {
      // A promotion opens the file with the data key this handle already holds (encrypted.md §14).
      role = handoff.elect(pause, this.storage.paging?.codec() ?? null);
    } catch (error) {
      if (isCode(error, "55006")) throw errNoOwner();
      throw error;
    }
    const old = this.client;
    this.installRole(role);
    old?.close();
  }

  // writerRoleChanged reports whether the current writer took its gate under an older role.
  writerRoleChanged(): boolean {
    return this.handoff !== null && (this.writerGen !== this.roleGen || this.roleKind === "none");
  }

  // beginServiceWrite takes the writer gate of the current role (caller set writerActive): the
  // owner's gate, or the owner's through begin_write. A writer meeting a read-only owner asks it for
  // ownership first; a begin the lost connection never answered is sent again after re-electing.
  private beginServiceWrite(timeoutMs: number): void {
    const deadline = timeoutMs > 0 ? Date.now() + timeoutMs : Number.POSITIVE_INFINITY;
    for (;;) {
      const gen = this.roleGen;
      if (this.roleKind === "owner") {
        this.waitAdmission();
        if (gen !== this.roleGen) continue;
        this.serviceTakeGate(timeoutMs);
        this.writerGen = this.roleGen;
        return;
      }
      if (this.roleKind === "none") {
        this.reelect();
        continue;
      }
      const client = this.client!;
      if (client.lost()) {
        this.reelect();
        continue;
      }
      if (client.ownerReadOnly && !client.readOnly) {
        this.requestWriteOwnership(client, deadline);
        continue;
      }
      try {
        this.clientBeginWrite(timeoutMs);
      } catch (error) {
        if (client.lost() && isCode(error, "08006")) continue;
        throw error;
      }
      this.writerClient = client;
      this.writerGen = this.roleGen;
      return;
    }
  }

  // requestWriteOwnership is a writer's request to a read-only owner (README §6.5 case 2): unless this
  // handle never owns or cannot open the file read-write, it asks for ownership and waits, under the
  // session's lock timeout, for the connection to end, then re-elects.
  private requestWriteOwnership(client: PageClientHandle, deadline: number): void {
    const handoff = this.handoff!;
    if (handoff.ownership === "never" || !handoff.canOpenReadWrite()) {
      throw engineError(
        "read_only_sql_transaction",
        "the database owner opened the file read-only",
      );
    }
    try {
      client.askOwnership();
    } catch {
      // A lost connection is the handoff's end too.
    }
    if (!client.waitForLoss(deadline)) {
      throw engineError(
        "lock_not_available",
        "could not obtain the database writer lock: the read-only owner did not hand over ownership in time",
      );
    }
    this.reelect();
  }

  // beginServiceRead is a page-service read begin: it pins the current role's newest snapshot for a
  // local reader, re-electing first when the connection ended and retrying a begin the lost connection
  // never answered.
  private beginServiceRead(): bigint {
    for (;;) {
      const gen = this.roleGen;
      if (this.roleKind === "owner") {
        this.waitAdmission();
        if (gen !== this.roleGen) continue;
        this.serviceRefresh();
      } else if (this.roleKind === "none") {
        this.reelect();
        continue;
      } else {
        const client = this.client!;
        if (client.lost()) {
          this.reelect();
          continue;
        }
        try {
          this.clientRefresh();
        } catch (error) {
          if (client.lost() && isCode(error, "08006")) continue;
          throw error;
        }
      }
      const version = this.committed.txid;
      this.register(version);
      return version;
    }
  }

  // waitAdmission holds an owner's own transaction begin while a yield drains, then lets the drain end
  // (releasing ownership, or not when the yield was cancelled); the caller re-reads the role.
  private waitAdmission(): void {
    const service = this.service!;
    if (service.admitting() || !service.yielding()) return;
    if (this.yieldTimer !== null) clearTimeout(this.yieldTimer);
    this.yieldTimer = null;
    this.yieldDeadline ??= Date.now() + this.handoff!.yieldGraceMs;
    for (;;) {
      const step = this.drainStep();
      if (step === "wait") {
        sleepMs(DRAIN_POLL_MS);
        continue;
      }
      if (step === "release") this.releaseAndReelect();
      return;
    }
  }

  // yieldRequested starts a yield's drain on the event loop (README §6.5): it polls until the running
  // transactions finished or the yield grace passed, then releases ownership.
  private yieldRequested(gen: number): void {
    if (gen !== this.roleGen || this.handoffClosed || this.yieldTimer !== null) return;
    const service = this.service;
    if (service === null || !service.yielding()) return;
    this.yieldDeadline ??= Date.now() + this.handoff!.yieldGraceMs;
    const tick = (): void => {
      this.yieldTimer = null;
      if (gen !== this.roleGen || this.handoffClosed) return;
      const step = this.drainStep();
      if (step === "wait") {
        this.yieldTimer = setTimeout(tick, DRAIN_POLL_MS);
        (this.yieldTimer as { unref?: () => void }).unref?.();
        return;
      }
      if (step === "release") this.releaseAndReelect();
    };
    tick();
  }

  // drainStep checks a yield's drain once: cancelled (the requester disconnected), wait, or release
  // (no own reader, own writer, or client transaction runs, or the grace passed).
  private drainStep(): "wait" | "cancelled" | "release" {
    const service = this.service!;
    if (!service.yielding()) {
      this.yieldDeadline = null;
      return "cancelled";
    }
    this.serviceHandoff(); // a handed gate is a maintenance commit to run, not a transaction to wait for
    const drained = this.live.size === 0 && service.clientsIdle(true);
    if (!drained && Date.now() < (this.yieldDeadline ?? 0)) return "wait";
    this.yieldDeadline = null;
    return service.beginRelease() ? "release" : "cancelled";
  }

  // releaseAndReelect ends a yield: release ownership, then re-elect as a client paused for the
  // handoff pause, so the requester takes ownership.
  private releaseAndReelect(): void {
    this.releaseOwnership(Date.now() + HANDOFF_PAUSE_MS);
    try {
      this.reelect();
    } catch {
      // The next transaction begin re-elects again.
    }
  }

  // releaseOwnership ends the owner role (README §6.5): the service stops serving (every connection
  // closes; a client commit past its point of no return already completed on the service worker) and
  // removes its endpoint and owner record, the role becomes none, the old storage refuses every further
  // access (closing the file), and presence is released. The caller drained first.
  private releaseOwnership(pauseUntilMs: number): void {
    const service = this.service!;
    const storage = this.storage;
    const coordinator = this.coordinator;
    service.close();
    this.installRole(
      { kind: "none", engine: null, service: null, client: null, coordinator: null },
      pauseUntilMs,
    );
    const paging = storage.paging;
    if (paging !== null) {
      // Pending relaxed commits become durable before ownership moves (page-service.md §6). A failed
      // flush poisons the pager; the next owner stabilizes whatever it adopts.
      try {
        paging.flushPending();
      } catch {
        // See above.
      }
      paging.swapStore(new RevokedBlockStore()).close();
      paging.poison();
    }
    coordinator?.close();
  }

  // closeService ends the handle's page-service role at close (README §6.5): a serving owner sends
  // notice kind 3, drains its clients' transactions for the shutdown grace, and stops serving; a
  // client closes its connection. The caller closes the storage and the coordinator afterwards.
  closeService(): void {
    this.handoffClosed = true;
    if (this.yieldTimer !== null) clearTimeout(this.yieldTimer);
    this.yieldTimer = null;
    const service = this.service;
    if (service !== null) {
      // The drain polls through the lock host, whose clock it reads (the simulator's in a simulation).
      const handoff = this.handoff!;
      service.beginClosing();
      const deadline = handoff.nowMicros() + handoff.shutdownGraceMs * 1000;
      for (;;) {
        this.serviceHandoff();
        if (service.clientsIdle(false) || handoff.nowMicros() >= deadline) break;
        handoff.pause();
      }
      service.close();
    }
    this.service = null;
    this.client?.close();
    this.roleKind = "none";
    this.roleGen++;
  }

  // resolveLostCommit settles a client commit whose acknowledgement was lost (README §6.4–§6.5): the
  // re-election promotes this process, which answers from its own resolution window, or reconnects it
  // to the new owner, which it asks; with no owner in time the outcome is unknown (08007).
  private resolveLostCommit(lost: { txid: bigint; digest: bigint; failure: Error }): void {
    const unknown = (detail: string): Error =>
      engineError(
        "transaction_resolution_unknown",
        `lost the connection to the database owner during commit; ${detail}`,
      );
    try {
      this.reelect();
    } catch {
      throw unknown("no owner answered whether it committed");
    }
    let status: number;
    if (this.service !== null) status = this.service.resolve(lost.txid, lost.digest);
    else if (this.client !== null) {
      try {
        status = this.client.commitStatus(lost.txid, lost.digest);
      } catch {
        throw unknown("whether the transaction committed is unknown");
      }
    } else throw unknown("whether the transaction committed is unknown");
    if (status === 0) return; // committed
    if (status === 1) throw lost.failure; // not committed
    throw unknown("whether the transaction committed is unknown");
  }

  // --- the page-service owner (spec/pageservice/README.md §6.4) ---

  // serviceTakeGate waits for the service's writer gate; a gate handed over for a maintenance commit
  // runs it first and is kept. A poisoned service (a failed client commit) refuses the writer.
  private serviceTakeGate(timeoutMs: number): void {
    const service = this.service!;
    const handed = service.acquireGate(timeoutMs);
    this.gateService = service;
    if (handed) this.runMaintenance();
    if (service.poisoned()) {
      this.storage.paging?.poison();
      throw engineError(
        "io_error",
        "a page-service client commit failed to write; close and reopen the database",
      );
    }
    const epoch = service.epoch();
    this.adoptForeign();
    this.serviceEpoch = epoch;
  }

  // tryServiceGate takes the service's gate only if it is free now (compaction never waits).
  tryServiceGate(): boolean {
    if (!this.service!.tryGate()) return false;
    this.gateService = this.service;
    return true;
  }

  private releaseServiceGate(): void {
    const service = this.gateService;
    if (service === null) return;
    this.gateService = null;
    service.releaseGate();
  }

  // serviceRefresh is an owner's transaction begin: one atomic load while no client committed and no
  // gate was handed over; otherwise run a handed-over maintenance commit and adopt client commits.
  serviceRefresh(): void {
    const service = this.service!;
    const epoch = service.epoch();
    if (epoch === this.serviceEpoch) return;
    this.serviceHandoff();
    this.adoptForeign();
    this.serviceEpoch = epoch;
  }

  // serviceHandoff runs a maintenance commit when the service handed the gate over, then releases it
  // (the event-loop handler, and every transaction begin that noticed a new epoch).
  serviceHandoff(): void {
    const service = this.service;
    if (service === null || !service.takeHanded()) return;
    try {
      this.runMaintenance();
    } finally {
      service.releaseGate();
    }
  }

  // runMaintenance adopts the client commits and runs the maintenance commit, holding the gate. A
  // failure here fails nothing: the client commit stands, and the next transaction retries adoption.
  private runMaintenance(): void {
    try {
      this.adoptForeign();
      this.maintenanceCommit();
    } catch {
      // Adoption fails only the transaction that needs it (README §6.4); maintenance is best effort.
    }
  }

  // adoptForeign brings the owner's committed root up to the newest client commit (page-service.md
  // §7): the service published it structurally; its catalog is loaded here, at an owner transaction
  // begin, never in the commit path. Pages the client commits wrote are dropped from the pool (a reused
  // free page may be cached from before it was freed). A failure (a collation this owner's environment
  // lacks, say) fails only the transaction that needed it. The free list's generation and the live
  // count at the last reclamation stay the owner's: a client commit's free list is the committed one
  // minus the pages it reused.
  private adoptForeign(): void {
    const own = this.committed.txid;
    const pending = this.service!.pending(own);
    if (pending.txid <= own) return;
    const paging = this.storage.paging;
    if (paging === null) throw engineError("io_error", "database storage is closed");
    if (pending.all) paging.invalidateAll();
    else for (const index of pending.indices) paging.invalidate(index);
    // The client commit is durable (the service synced it): no recovery barrier is owed for it, and a
    // single commit's written pages are exactly the indices its manifest protects.
    paging.finishDurableCommit(pending.txid, metaChecksum(pending.meta));
    if (!pending.all && pending.txid === own + 1n) {
      paging.cacheValidation(pending.meta, new Set(pending.indices));
    }
    paging.refreshAllocatedPages();
    const loaded = loadEnginePaged(paging, this.environment);
    // A commit published while this reload ran: its written pages were not dropped above.
    if (loaded.committed.txid !== pending.txid) paging.invalidateAll();
    if (loaded.committed.txid <= own) return;
    this.storage.pageCount = loaded.pageCount;
    this.storage.freePages = loaded.freePages;
    this.storage.live = loaded.live;
    this.committed = loaded.committed;
    this.planEpoch++;
  }

  // maintenanceCommit is the owner-only free-list rebuild after client commits (page-service.md §6): a
  // client commit never walks the database, so when the pages off the free list pass the reclamation threshold
  // and no pin holds back reclamation, the owner commits the unchanged snapshot at the next txid with
  // in-commit reclamation. A client commit never lowers the high-water either, so the owner also commits
  // when the free list ends at the high-water, and that commit truncates it (format.md *Trailing
  // truncation*). Called holding the gate, after adoption.
  private maintenanceCommit(): void {
    const st = this.storage;
    const prev = this.committed;
    const newTxid = prev.txid + 1n;
    if (this.oldestLiveVersion(newTxid) !== newTxid) return;
    if (
      !(
        st.liveAtCompaction === 0 ||
        reclaimDue(st.pageCount, st.freePages.length, st.liveAtCompaction) ||
        st.freePages[st.freePages.length - 1] === st.pageCount - 1
      )
    ) {
      return;
    }
    const snap = prev.clone();
    snap.txid = newTxid;
    try {
      persistImpl(st, snap, true, true, {
        name: this.domainName(),
        prev,
        canCompact: true,
        stagesRows: false,
      });
    } catch (error) {
      if (st.paging?.commitRequiresReopen()) this.service!.stopServing();
      throw error;
    }
    snap.demoteCleanLeaves();
    this.committed = snap;
    this.publishService(snap.txid);
    this.planEpoch++;
  }

  // publishService records a main-database commit of this engine in the service (README §4).
  publishService(txid: bigint): void {
    const st = this.storage;
    if (st.lastCommitMeta === null) return;
    let localOldest: bigint | null = null;
    for (const v of this.live.keys()) if (localOldest === null || v < localOldest) localOldest = v;
    this.serviceLocalOldest = localOldest !== null;
    this.service!.published(txid, st.lastCommitMeta, st.lastCommitWritten, {
      freeGenTxid: st.freeGenTxid,
      liveAtCompaction: st.liveAtCompaction,
      localOldest,
    });
  }

  checkPid(): void {
    this.coordinator?.checkPid();
    for (const attachment of this.attachments.values()) attachment.coordinator?.checkPid();
  }

  // refreshShared brings a transaction begin up to date with other processes: a page-service owner
  // adopts its clients' commits (page-service.md §7). A page client sends nothing here: only a read
  // begin that pins a local reader talks to the owner (refreshAndPin), since a pin taken where no
  // reader follows would leak (README §6.3).
  refreshShared(): void {
    if (this.service !== null) this.serviceRefresh();
  }

  // clientRefresh is a page client's read begin (spec/pageservice/README.md §6.3): one begin_read, the
  // cache invalidation it names, and — when the owner's version moved or the cache was invalidated
  // wholesale — the adoption of its meta, catalog/interior skeleton, and a new plan epoch. The pin the
  // reply carries is held for the local reader that follows.
  private clientRefresh(): void {
    this.clientAdopt(this.client!.beginRead());
  }

  // clientAdopt applies a read_begun / write_begun cache answer: it drops the listed pages (or the
  // whole cache) and, when the owner's version is newer than the adopted one (or the cache was dropped
  // wholesale), adopts its meta and reloads the catalog/interior skeleton and page accounting.
  private clientAdopt(reply: ClientReadBegun): void {
    const client = this.client!;
    const paging = this.storage.paging;
    if (paging === null) throw engineError("io_error", "database storage is closed");
    if (reply.invalidateAll) paging.invalidateAll();
    else for (const index of reply.indices) paging.invalidate(index);
    if (!reply.invalidateAll && reply.txid <= client.adopted) return;
    // The owner may have rekeyed the file since this client unlocked it (encrypted.md §14): the meta
    // must still be of the variant the client opened, and authenticate under its key.
    const encryptedMeta = reply.meta.length > 6 && reply.meta[6] !== 0;
    if (paging.codec() === null && encryptedMeta) {
      throw engineError(
        "credential_required",
        "the database was encrypted since this handle opened it; reopen it with a credential",
      );
    }
    if (paging.codec() !== null && !encryptedMeta) throw staleKey();
    client.setMeta(reply.meta);
    paging.refreshAllocatedPages();
    let loaded: Engine;
    try {
      loaded = loadEnginePaged(paging, this.environment);
    } catch (error) {
      if (isCode(error, "XX001") && !heldKeyAuthenticates(paging)) throw staleKey();
      throw error;
    }
    client.markAdopted(reply.incarnation, reply.txid);
    this.storage.pageCount = loaded.pageCount;
    this.storage.freePages = loaded.freePages;
    this.storage.liveAtCompaction = loaded.liveAtCompaction;
    this.storage.freeGenTxid = loaded.freeGenTxid;
    this.storage.live = loaded.live;
    this.committed = loaded.committed;
    this.planEpoch++;
  }

  // clientBeginWrite takes the owner's writer gate for this process's writer (README §3.5) and adopts
  // the snapshot it pins, so the write transaction starts from the owner's newest commit.
  private clientBeginWrite(timeoutMs: number): void {
    const client = this.client!;
    if (client.readOnly) {
      throw engineError(
        "read_only_sql_transaction",
        "cannot write through a read-only page-service connection",
      );
    }
    const reply = client.beginWrite(timeoutMs);
    try {
      this.clientAdopt(reply);
    } catch (error) {
      client.endWrite();
      throw error;
    }
  }

  // persistClient is a page client's commit (page-service.md §5.6, §6): the ordinary validated-COW
  // recipe over the service store, never reclaiming (free-list rebuilds are owner-only), reusing the
  // free list only under the owner's reuse grant, and never compacting for its storage budget. The
  // store's sync sends the CommitDelta. A failure after which the owner certainly did not commit leaves
  // the pager usable; any other (the outcome unknown) keeps it refusing commits until reopen.
  private persistClient(snap: Snapshot, stagesRows: boolean): void {
    const client = this.client!;
    const budget: BudgetCtx = {
      name: this.domainName(),
      prev: this.committed,
      canCompact: false,
      stagesRows,
    };
    try {
      persistImpl(this.storage, snap, false, client.reuseGrant, budget, false);
    } catch (error) {
      // A lost acknowledgement: the outcome is resolved by re-election (README §6.5).
      const lost = client.lostCommit(error);
      if (lost !== null) {
        this.resolveLostCommit(lost);
        return;
      }
      client.discardPending();
      const settled = client.failedCommit(error);
      if (settled.notCommitted) this.storage.paging?.abandonDurableCommit();
      this.storage.paging?.invalidateAll();
      throw settled.error;
    }
  }

  // refreshAndPin is a read begin that pins a local reader: refresh to the latest committed version and
  // register it in the live registry, returning it. On a page client this is the begin_read round
  // trip, and the reply's pin is kept for exactly this registration: the engine thread runs the
  // refresh and the registration with nothing in between, so no end_read for the version can be sent
  // before its reader is registered (README §6.3).
  refreshAndPin(): bigint {
    if (this.handoff !== null) return this.beginServiceRead();
    if (this.client !== null) this.clientRefresh();
    else this.refreshShared();
    const version = this.committed.txid;
    this.register(version);
    return version;
  }

  // releaseIdlePins ends a page client's pins on every version no local reader pins — after an open or
  // a read begin no reader followed (README §6.3: an idle client pins nothing).
  releaseIdlePins(): void {
    const client = this.client;
    if (client === null) return;
    for (const v of [...client.held]) if (!this.live.has(v)) client.releasePin(v);
  }

  // servicePinned is the oldest version a page-service client pins, or null (none, or not an owner).
  private servicePinned(): bigint | null {
    const service = this.service;
    if (service === null) return null;
    service.lock();
    try {
      return service.oldestPinnedLocked();
    } finally {
      service.unlock();
    }
  }

  // compactLockedSteps compacts database lname (`main` or an attachment) under the local writer flag the
  // caller holds (spec/design/api.md §2.6): require presence-exclusive coordination and a drained reader
  // watermark, then rewrite the storage at the next version and publish the reloaded root. JS is
  // single-threaded, and the async runtime starts no other engine operation while a continuation is
  // suspended (ts-runtime.md §11.2), so no reader can pin between the check and the publish.
  *compactLockedSteps(lname: string, name: string, rekey: Rekey | null = null): Steps<void> {
    const main = lname === "main";
    const coordinator = main
      ? this.coordinator
      : (this.attachments.get(lname)?.coordinator ?? null);
    coordinator?.checkPid();
    if (this.live.size > 0) {
      throw engineError(
        "object_in_use",
        `cannot compact database "${name}" while a reader is open`,
      );
    }
    // A page-service owner holds its lock across the compaction, so no client can pin the old file in
    // between; a client pin is a reader (55006), exactly like a local one.
    const service = main ? this.service : null;
    service?.lock();
    try {
      if (service !== null && service.oldestPinnedLocked() !== null) {
        throw engineError(
          "object_in_use",
          `cannot compact database "${name}" while a reader is open`,
        );
      }
      const prev = main ? this.committed : this.attached.get(lname)!;
      // A rebuild's scratch domain spills through the handle's scratch target (encrypted.md §12).
      const compacted =
        rekey === null
          ? yield* compactStorageSteps(this.storageFor(lname), prev, prev.txid + 1n)
          : yield* rekeyStorageSteps(
              this.storageFor(lname),
              prev,
              prev.txid + 1n,
              rekey.target,
              this.storage.spillSink,
              name,
            );
      if (main) {
        this.committed = compacted;
        service?.compactedLocked(compacted.txid, this.storage.paging!, {
          freeGenTxid: this.storage.freeGenTxid,
          liveAtCompaction: this.storage.liveAtCompaction,
          localOldest: null,
        });
      } else {
        const attached = new Map(this.attached);
        attached.set(lname, compacted);
        this.attached = attached;
      }
    } finally {
      service?.unlock();
    }
    this.planEpoch++;
  }

  // pageSize is the byte store's page size (fixed into the file/image at creation). Minted sessions
  // adopt it so their stores' cap matches the physical pages persist writes (CLAUDE.md §8).
  get pageSize(): number {
    return this.storage.pageSize;
  }

  // persist durably publishes snap to the backing store via an incremental copy-on-write commit
  // (persistImpl — the host-independent recipe, transactions.md §9) — the publish chokepoint for
  // every host (bplus-reshape.md B3): a file-backed core pwrites + fdatasyncs; an in-memory core
  // packs the same dirty pages into its MemoryBlockStore, whose sync is a no-op — the file commit
  // minus durability, one code path. Called from Session.publish; pageCount/freePages on the storage
  // engine advance only after both syncs succeed, so a write failure leaves the file's prior meta
  // untouched.
  // stagesRows is whether the commit staged any record version (the committed-storage repair
  // exemption, memory.md §8.3).
  persist(snap: Snapshot, stagesRows: boolean): void {
    runSync(this.persistSteps(snap, stagesRows));
  }

  // persistSteps is persist as a continuation (ts-runtime.md §11): a page client's commit stays on its
  // synchronous channel; an owner or uncoordinated commit awaits an asynchronous storage stack.
  *persistSteps(snap: Snapshot, stagesRows: boolean): Steps<void> {
    // A page-service commit checks the role its writer began under before it writes (README §6.5).
    if (this.writerRoleChanged()) throw errStaleRole();
    if (this.client !== null) {
      this.persistClient(snap, stagesRows);
      return;
    }
    // The reader-liveness watermark (transactions.md §8) gates two things at the main-domain commit:
    //   - canReclaim (oldest_live == the new version, i.e. no reader live at an older version) lets the
    //     periodic COMPACT recompute the free-list (freeing this commit's fresh orphans);
    //   - canReuse (oldest_live ≥ the free-list's generation) lets this commit REUSE the free-list — a
    //     page dead at generation G is only safe to overwrite once no reader pins a version older than G.
    // Both consult the SAME registry; single-handle / all-readers-current keeps oldest_live == committed,
    // so both hold and behavior (and on-disk bytes) are identical to an ungated commit.
    const oldest = this.oldestLiveVersion(snap.txid);
    const budget: BudgetCtx = {
      name: this.domainName(),
      prev: this.committed,
      canCompact: this.canCompactCommitted(),
      stagesRows,
    };
    yield* persistSteps(
      this.storage,
      snap,
      oldest === snap.txid,
      oldest >= this.storage.freeGenTxid,
      budget,
    );
  }

  // committedAttachment is the published committed root of attachment name (exact), if attached.
  committedAttachment(name: string): Snapshot | undefined {
    return this.attachedRoot(name);
  }

  // canCompactCommitted reports whether the committed snapshots may be compacted now (memory.md §8.3):
  // no live reader pins a version older than the currently committed one. A reader AT that version
  // keeps every page the compaction keeps; one pinned earlier could still fault a page it would free.
  canCompactCommitted(): boolean {
    return this.oldest() >= this.committed.txid;
  }

  // precheckBudgets checks every limited domain — in-memory (memory.md §8.3) or durable (§8.7) — of a
  // multi-root commit before any domain writes a page: attachments commit before main, so a later
  // domain's 54P06 must not follow an earlier domain's pack, compaction, or durable write. main is the
  // working main snapshot persisted at publish (null when main is unchanged and does not persist,
  // transactions.md §2.1); attached the dirtied attachments' working snapshots.
  // Attachments are visited in name order, so which domain a 54P06 names never depends on set order.
  *precheckBudgetsSteps(
    main: Snapshot | null,
    mainStagesRows: boolean,
    attached: [string, Snapshot][],
  ): Steps<void> {
    const canCompact = this.canCompactCommitted();
    const sorted = [...attached].sort((a, b) => compareFilePaths(a[0], b[0])); // UTF-8 byte order
    for (const [name, snap] of sorted) {
      const prev = this.attachedRoot(name);
      const att = this.attachments.get(name);
      if (prev === undefined || att === undefined) continue;
      let reuse = true;
      let compactable = canCompact;
      if (att.svc !== null) {
        // A service attachment's commit runs through its inner handle: its reader registry (which
        // counts other processes' pins when it owns the file) gates reuse and compaction.
        const inner = (att.svc as ServiceAttachment).core;
        compactable = inner.canCompactCommitted();
        reuse = inner.oldestLiveVersion(prev.txid + 1n) >= inner.storage.freeGenTxid;
        if (inner.client !== null) {
          reuse = inner.client.reuseGrant;
          compactable = false;
        }
      }
      yield* precheckBudgetSteps(att.storage, snap, reuse, {
        name,
        prev,
        canCompact: compactable,
        stagesRows: snap.stagedBytes() !== 0,
      });
    }
    // An unchanged main does not persist, so it has no budget to check (transactions.md §2.1).
    if (main === null) return;
    const oldest = this.oldestLiveVersion(this.committed.txid + 1n);
    const reuse =
      this.client !== null ? this.client.reuseGrant : oldest >= this.storage.freeGenTxid;
    yield* precheckBudgetSteps(this.storage, main, reuse, {
      name: this.domainName(),
      prev: this.committed,
      canCompact,
      stagesRows: mainStagesRows,
    });
  }

  // storageFor returns the storage Engine of database name — main or an attachment, by exact name (grammar.md §3);
  // the session-local temp domain and an unknown name are 42704.
  storageFor(name: string): Engine {
    const lname = name;
    if (lname === "main") return this.storage;
    const att = lname === "temp" ? undefined : this.attachments.get(lname);
    if (att === undefined) {
      throw engineError("undefined_object", `database "${name}" is not attached`);
    }
    return att.storage;
  }

  // hasLiveReaders reports whether any cross-session reader currently pins a committed snapshot (the
  // live registry, transactions.md §8) — the within-session compaction watermark for a host attachment
  // (attached-databases.md §5): the committing writer holds the writer flag but is not itself in `live`,
  // so an empty registry means no other session can observe a page the commit is about to reclaim.
  hasLiveReaders(): boolean {
    return this.live.size > 0 || this.servicePinned() !== null;
  }

  // adoptAttachments adopts the in-memory attachment commits commitTx staged, after main persisted and
  // the roots published (attached-databases.md §5): advance each attachment's page accounting and run
  // its post-commit compaction relative to its now-published root in attached. Compaction is safe only
  // when no cross-session reader pins an older root (the live registry; the committing writer holds the
  // gate but is not in live) — read here, after publish. The commit is already published, so a
  // compaction walk that cannot read a page only skips this reclamation: the free list stays the
  // plan's, and the read error surfaces on the next read of that page.
  adoptAttachments(staged: StagedCommit[], attached: Map<string, Snapshot>): void {
    if (staged.length === 0) return;
    const canReclaim = !this.hasLiveReaders();
    for (const commit of staged) {
      const att = this.attachments.get(commit.name);
      const snap = attached.get(commit.name);
      if (att === undefined || snap === undefined) continue;
      try {
        adoptStaged(att.storage, snap, commit, canReclaim);
      } catch {
        // See above: an unreadable page skips reclamation; the commit stands.
      }
    }
  }

  // mainIsDurable reports whether MAIN is file-backed (durable) rather than in-memory — the input to the
  // one-durable-writer count (attached-databases.md §5). The storage identity's byte store carries a
  // path only for a file-backed core.
  mainIsDurable(): boolean {
    return this.storage.path !== null;
  }

  register(version: bigint): void {
    this.live.set(version, (this.live.get(version) ?? 0) + 1);
  }

  // deregister drops one reader pin on version registered under role generation gen; a pin of an older
  // page-service role is gone already (the role change started an empty registry).
  deregister(version: bigint, gen: number): void {
    if (gen !== this.roleGen) return;
    const c = (this.live.get(version) ?? 0) - 1;
    if (c <= 0) {
      this.live.delete(version);
      // A page client's owner pin on a version lives only while a local reader pins it (README §6.3).
      if (this.client?.held.has(version)) this.client.releasePin(version);
      // A page-service owner told its service about a local reader at its last commit: none is left.
      if (this.serviceLocalOldest && this.live.size === 0) {
        this.serviceLocalOldest = false;
        this.service?.clearLocalOldest();
      }
    } else this.live.set(version, c);
  }

  // oldest is the oldest still-pinned version, or the committed version when no reader is live. The
  // map scan is order-independent (a minimum), so no map iteration order leaks (CLAUDE.md §8).
  // A page-service owner's client pins count like local readers (spec/pageservice/README.md §4).
  oldest(): bigint {
    let oldest = this.committed.txid;
    for (const v of this.live.keys()) if (v < oldest) oldest = v;
    const pinned = this.servicePinned();
    if (pinned !== null && pinned < oldest) oldest = pinned;
    return oldest;
  }

  // oldestLiveVersion is the oldest still-pinned version, floored at newTxid (the version this commit
  // publishes) so "no live reader" reads as newTxid — the safe case for compaction (temp-tables.md §6).
  // Any live reader pins a version older than newTxid (it opened before this commit), so a non-empty
  // registry yields a value < newTxid and defers compaction. Distinct from oldest(), which floors at the
  // CURRENTLY-committed version; during a persist (before committed swaps) the two differ.
  oldestLiveVersion(newTxid: bigint): bigint {
    let oldest = newTxid;
    for (const v of this.live.keys()) if (v < oldest) oldest = v;
    const pinned = this.servicePinned();
    if (pinned !== null && pinned < oldest) oldest = pinned;
    return oldest;
  }
}

// closeDomain flushes a storage domain's pending relaxed commits (validated-cow.md §4) and closes its
// paging, returning the first error (null when none). The paging is closed whether the flush failed or
// not.
function closeDomain(st: Engine): unknown {
  const paging = st.paging;
  if (paging === null) return null;
  let error: unknown = null;
  try {
    paging.flushPending();
  } catch (e) {
    error = e;
  }
  try {
    paging.close();
  } catch (e) {
    if (error === null) error = e;
  }
  return error;
}

// --- File attachments under the page service (spec/design/page-service.md §5.5; spec/pageservice/
// README.md §6.6). An attachment opened under `locking = "auto"` has its own coordination domain: an
// inner handle elected on the attached file exactly as an open is — the owner, serving it, or a page
// client of whichever process owns it — with its own role, handoff, reader registry, and writer gate.
// The outer handle's sessions use it as a database of the handle:
//   - a read begin pins every such attachment's newest snapshot through its inner handle (an owner
//     adopts its clients' commits; a client sends begin_read) and releases the pins when the reader ends;
//   - a write begin takes, after the in-process writer flag, the writer gate of main (when main is under
//     the page service) and of every read-write such attachment, in real-path byte order, releasing the
//     gates already held in reverse order on failure; read-only such attachments are pinned instead;
//   - an attachment's commit runs through its inner handle under the gate the writer holds.
// The one-durable-writer rule is unchanged (attached-databases.md §5). ---

// ServiceAttachment is a file attachment under the page service: its inner handle (and that handle's
// core) and the attached file's resolved real path, the writer-gate order key.
class ServiceAttachment {
  readonly name: string;
  readonly readOnly: boolean;
  readonly db: Database;
  readonly core: SharedCore;
  readonly realPath: string;

  constructor(name: string, readOnly: boolean, db: Database, core: SharedCore, realPath: string) {
    this.name = name;
    this.readOnly = readOnly;
    this.db = db;
    this.core = core;
    this.realPath = realPath;
  }

  commitServiceAttachment(snap: Snapshot, stagesRows: boolean): Snapshot {
    return this.core.commitServiceAttachment(snap, stagesRows);
  }
}

// AttachPin is one reader pin on a service attachment's inner reader registry, with the snapshot it
// pins and the role generation it was registered under.
type AttachPin = { name: string; core: SharedCore; gen: number; v: bigint; snap: Snapshot };

// GateRef is one writer gate a writer holds: main's (att null) or a service attachment's.
type GateRef = { path: string; att: ServiceAttachment | null };

// coreOf is the shared core of a handle this module built (an attachment's inner handle).
function coreOf(db: Database): SharedCore {
  return (db as unknown as { core: SharedCore }).core;
}

// pinServiceAttachment runs the inner handle's read begin and pins its newest snapshot.
function pinServiceAttachment(svc: ServiceAttachment): AttachPin {
  const inner = svc.core;
  const v = inner.refreshAndPin();
  return { name: svc.name, core: inner, gen: inner.roleGen, v, snap: inner.committed };
}

// overlayAttachPins returns attached with each pin's snapshot as its attachment's root.
function overlayAttachPins(
  attached: Map<string, Snapshot>,
  pins: AttachPin[],
): Map<string, Snapshot> {
  if (pins.length === 0) return attached;
  const out = new Map(attached);
  for (const pin of pins) out.set(pin.name, pin.snap);
  return out;
}

// releaseAttachPins releases reader pins on service attachments, in the order they were taken.
function releaseAttachPins(pins: AttachPin[]): void {
  for (const pin of pins) pin.core.deregister(pin.v, pin.gen);
}

// addAttachPins registers one more pin on each pinned version (a cursor outliving its reader's pin),
// skipping a pin whose inner role has changed: that role's registry is gone.
function addAttachPins(pins: AttachPin[]): AttachPin[] {
  const out: AttachPin[] = [];
  for (const pin of pins) {
    const c = pin.core;
    if (c.roleGen === pin.gen && (c.live.get(pin.v) ?? 0) > 0) {
      c.register(pin.v);
      out.push(pin);
    }
  }
  return out;
}

// attachPinsStale throws 08006 when a pinned attachment's inner handle changed role since the pin.
function attachPinsStale(pins: AttachPin[]): void {
  for (const pin of pins) {
    if (pin.core.roleGen !== pin.gen || pin.core.client?.lost()) throw errStaleRole();
  }
}

// compactStorage replaces storage st with the from-scratch image of snap at txid (host compaction,
// spec/design/api.md §2.6) and returns the committed snapshot reloaded from it. A file is rewritten
// through the host's registered file compactor (file.ts compactFile), a durable store without a path
// through its rewriter (the OPFS host, opfs.ts); an in-memory domain swaps in a new MemoryBlockStore.
// The image has no free list and no garbage, so the page accounting restarts from it, and the storage
// budget forgets the last commit's pages (none of them is dead now).
function* compactStorageSteps(st: Engine, snap: Snapshot, txid: bigint): Steps<Snapshot> {
  const old = st.paging;
  if (old === null) throw engineError("io_error", "database storage is closed");
  old.checkDurableCommit();
  const capacity = old.capacity();
  let paging: SharedPaging;
  if (st.path !== null) {
    paging = fileCompactor!(st.fileHost, st.path, snap, st.pageSize, txid, old, capacity);
  } else if (st.rewriter !== null) {
    const rewriter = st.rewriter;
    paging = yield* withPrefetchSteps(old, snap, () =>
      rewriter(snap, st.pageSize, txid, old, capacity),
    );
  } else {
    const image = toImageBytes(snap, st.pageSize, txid);
    paging = new SharedPaging(Pager.fromStore(new MemoryBlockStore(image)), capacity);
  }
  return yield* adoptRewrittenSteps(st, paging);
}

// withPrefetchSteps runs a storage rewrite of snap over old: under an asynchronous codec or device it
// first reads snap's pages ahead through the awaitable read (format.ts prefetchSnapshotSteps), so the
// synchronous serializer and rebuild never read, and drops them when the rewrite ends.
function* withPrefetchSteps<T>(
  old: SharedPaging,
  snap: Snapshot,
  rewrite: () => Steps<T>,
): Steps<T> {
  if (!old.writeSideAsync()) return yield* rewrite();
  try {
    yield* prefetchSnapshotSteps(snap, old);
    return yield* rewrite();
  } finally {
    old.endPrefetch();
  }
}

// rekeyStorage rotates the data key of file storage st, or converts it between the plaintext and
// encrypted variants (spec/fileformat/encrypted.md §12): rebuild every tree of snap under target's
// layout in a scratch domain (its pages moved to a scratch file of sink once they pass the pool
// budget, sealed under a throwaway key when the target is encrypted), then write the from-scratch image
// under target (null: plaintext) and rename it over the file as compaction does. A record or key the
// target cannot store, or an image over max_storage_bytes, fails before the rename with the file
// untouched. The caller holds the writer gate with the reader watermark drained.
//
// A store without a path (the OPFS host) has no scratch target: its rebuilt trees stay resident and
// are never staged, so the scratch domain holds no page and needs no throwaway key (the browser has
// no synchronous cipher to seal one with). Its pages are read ahead under an asynchronous codec
// (withPrefetchSteps), and the rewriter writes the image.
function* rekeyStorageSteps(
  st: Engine,
  snap: Snapshot,
  txid: bigint,
  target: PageCodec | null,
  sink: SpillSink | null,
  name: string,
): Steps<Snapshot> {
  const old = st.paging;
  if (old === null) throw engineError("io_error", "database storage is closed");
  old.checkDurableCommit();
  const path = st.path;
  const rewriter = st.rewriter;
  if ((path === null || fileCompactor === null) && rewriter === null) {
    throw engineError(
      "feature_not_supported",
      `cannot rekey database "${name}": its storage host cannot replace the file atomically`,
    );
  }
  const capacity = old.capacity();
  const resident = path === null;
  const build = newScratchStorage(
    st.pageSize,
    target === null || resident ? null : scratchCodec(),
    st.environment,
  );
  const rekey = { codec: target, limit: st.storageBudget.limit, name };
  let paging: SharedPaging;
  try {
    paging = yield* withPrefetchSteps(old, snap, function* () {
      // The scratch domain spills past the handle's page-cache budget, like a temp domain.
      const budget = Math.max(Math.min(capacity * st.pageSize, Number.MAX_SAFE_INTEGER), 1 << 20);
      const built = snap.emptied(capOf(st.pageSize, target !== null), build.paging!);
      snap.rebuildRows(built, resident ? Infinity : Math.floor(budget / 4), (s) => {
        if (resident) return;
        const staged = stageTemp(build, s, budget, sink);
        adoptStaged(build, s, staged, true);
        s.clearStaged();
      });
      if (path !== null) {
        return fileCompactor!(st.fileHost, path, built, st.pageSize, txid, old, capacity, rekey);
      }
      return yield* rewriter!(built, st.pageSize, txid, old, capacity, rekey);
    });
  } finally {
    // The scratch domain (and any scratch file it moved to) is released whatever happened.
    build.paging?.close();
  }
  return yield* adoptRewrittenSteps(st, paging);
}

// adoptRewritten makes paging, over st's rewritten storage, its storage and returns the committed
// snapshot reloaded from it (compaction and rekey). The image has no free list and no garbage, so the
// page accounting restarts from it, and the storage budget forgets the last commit's pages.
function* adoptRewrittenSteps(st: Engine, paging: SharedPaging): Steps<Snapshot> {
  // A file's old pager is already closed and poisoned here, so a failed reload leaves the handle
  // needing a reopen; an in-memory domain keeps its old store untouched.
  let loaded: Engine;
  try {
    loaded = yield* loadEnginePagedSteps(paging, st.environment);
  } catch (error) {
    paging.close();
    throw error;
  }
  st.paging = paging;
  st.pageCount = loaded.pageCount;
  st.freePages = loaded.freePages;
  st.liveAtCompaction = loaded.liveAtCompaction;
  st.freeGenTxid = loaded.freeGenTxid;
  st.live = loaded.live;
  st.storageBudget.lastCatRoot = 0;
  st.storageBudget.lastWritten = [];
  st.storageBudget.lastCatalogPages = 0;
  st.dirSynced = true; // compaction's own directory barrier made the new entry durable
  return loaded.committed;
}

// staleKey is 28P04: the owner rekeyed the database, so this client's data key no longer opens it
// (spec/fileformat/encrypted.md §14).
function staleKey(): EngineError {
  return engineError(
    "stale_key",
    "the database was rekeyed since this handle opened it; reopen it",
  );
}

// heldKeyAuthenticates reports whether the client's held key authenticates the meta it was just
// served, under either slot index (a read failure counts as authenticating: the load's error stands).
function heldKeyAuthenticates(paging: SharedPaging): boolean {
  try {
    return metaAuthenticates(paging);
  } catch {
    return true;
  }
}

// notEncrypted is a credential operation on a plaintext file: 28P03 (encryption.md §5).
function notEncrypted(): EngineError {
  return engineError("credential_not_applicable", "the database is not encrypted");
}

// readOnlyCredential is a credential change on a read-only database: 25006.
function readOnlyCredential(name: string): EngineError {
  return engineError(
    "read_only_sql_transaction",
    `cannot change the credential of read-only database "${name}"`,
  );
}

// fileCompactor is the host-injected file rewrite behind Database.compact (spec/design/api.md §2.6): the
// Node host (file.ts) registers compactFile at load, so shared.ts stays browser-clean. null until a host
// registers one; a file-backed storage cannot exist without file.ts, which registers it.
type FileCompactor = (
  files: FileHost | null,
  path: string,
  snap: Snapshot,
  pageSize: number,
  txid: bigint,
  old: SharedPaging,
  capacity: number,
  rekey?: RekeyTarget,
) => SharedPaging;

// StorageRewriter replaces a durable store that has no file path (the OPFS host, opfs.ts — hosts.md
// §5.1) with the from-scratch image of snap at txid, the counterpart of FileCompactor: written under
// rekey's codec when given (else old's), and returning the replacement paging context, a pool of
// capacity leaves. It reads snap's pages only through the synchronous read (the caller prefetches
// them under an asynchronous codec) and awaits its own device and codec stages.
export type StorageRewriter = (
  snap: Snapshot,
  pageSize: number,
  txid: bigint,
  old: SharedPaging,
  capacity: number,
  rekey?: RekeyTarget,
) => Steps<SharedPaging>;

// RekeyTarget is what a rekey writes its image under (spec/fileformat/encrypted.md §12): the target
// codec (null: plaintext) and the database's max_storage_bytes (0 or less: unlimited), checked against
// the image's live pages before the rename, with name the database the 54P06 names.
export type RekeyTarget = { codec: PageCodec | null; limit: bigint; name: string };

// Rekey is a rekey request threaded through the compaction path: its target codec (null: plaintext).
type Rekey = { target: PageCodec | null };

let fileCompactor: FileCompactor | null = null;

// canRewrite reports whether durable storage st can be replaced by compaction or rekey: a file through
// the registered file compactor, a store without a path through its rewriter (the OPFS host).
function canRewrite(st: Engine): boolean {
  return st.path !== null ? fileCompactor !== null : st.rewriter !== null;
}

// registerFileCompactor installs the host file rewrite that Database.compact uses for a file-backed
// database (called once, at file.ts module load).
export function registerFileCompactor(fn: FileCompactor): void {
  fileCompactor = fn;
}

// AttachSource selects the backing for a database attached via Database.attach
// (spec/design/attached-databases.md §4). A MEMORY source is a fresh, empty in-memory database
// (Slice 1b); a FILE source opens an existing single-file jed database on disk (Slice 2). Build one with
// attachMemory() or attachFile(path).
// maxStorageBytes limits the attachment's committed storage (spec/design/memory.md §8.1): an in-memory
// attachment's page high-water (§8.2), a file attachment's live pages (§8.7). Zero, negative, or absent
// is unlimited.
// credential unlocks an encrypted file attachment (spec/design/encryption.md §5): 28P02 without one,
// 28P01 when it does not unwrap the key, 28P03 for a plaintext file, 55000 for an in-memory source
// (nothing is at rest) or on a handle whose scratch files are not sealed (scratch encryption,
// spec/fileformat/encrypted.md §11).
export type AttachSource = {
  file: boolean;
  path?: string;
  locking?: "auto" | "exclusive" | "none";
  fileLockTimeoutMs?: number;
  maxStorageBytes?: bigint;
  credential?: Credential;
};

// attachMemory returns a source for a fresh, empty in-memory attachment (attached-databases.md §6),
// optionally with a committed-storage limit (memory.md §8.1).
export function attachMemory(
  options: Pick<AttachSource, "maxStorageBytes" | "credential"> = {},
): AttachSource {
  return { file: false, ...options };
}

// attachFile returns a source for a file-backed attachment: an existing single-file jed database at path
// (attached-databases.md §4, Slice 2). The file's own page size is honored (each attachment is its own
// page space, §2). With readOnly=true it is opened read-only (as well as write-rejected, 25006);
// readOnly=false opens it read-write so DDL/DML can target it (subject to the one-durable-writer rule, §5).
// options.maxStorageBytes limits the file's live pages (memory.md §8.7).
export function attachFile(
  path: string,
  options: Pick<
    AttachSource,
    "locking" | "fileLockTimeoutMs" | "maxStorageBytes" | "credential"
  > = {},
): AttachSource {
  return { file: true, path, ...options };
}

// fileAttachOpener is the host-injected file opener for a file-backed attachment (attached-databases.md
// §4, Slice 2): the node host (file.ts) registers it at load, so Database.attach can open a file-backed
// attachment WITHOUT shared.ts importing a host module (keeping it browser-clean — the same reason
// file.ts, not shared.ts, owns open/create). Returns the opened storage Engine (its committed snapshot +
// paging become the attachment). null until a host registers one → a file attach then throws
// feature_not_supported (a pure in-memory build has no file layer to reach).
// files is main's file host (null is the host's own), so an attachment opens through the same
// filesystem as main — the deterministic simulator's (simulation-testing.md §14 S4) included.
// relaxed is main's synchronous = off, which a file attachment follows (validated-cow.md §4), under
// the page service too.
// mainHandoff is main's page-service handoff (null when main is not under the page service): a file
// attachment under the page service copies its hosts and limits (page-service.md §5.5). The opener
// returns an opened file (its storage Engine and coordinator), or, under the page service, the inner
// handle elected on the attached file and the file's resolved real path (the writer-gate order key).
export type OpenedAttachment =
  | { kind: "file"; engine: Engine; coordinator: FileCoordinatorHost | null }
  | { kind: "service"; service: Database; realPath: string };
type FileAttachOpener = (
  source: AttachSource,
  readOnly: boolean,
  files: FileHost | null,
  environment: Environment,
  relaxed: boolean,
  mainHandoff: ServiceHandoffHost | null,
  // seals is whether the handle's scratch files are sealed (encrypted.md §11): a service attachment
  // then seals its own.
  seals: boolean,
) => OpenedAttachment;
let fileAttachOpener: FileAttachOpener | null = null;

// registerFileAttachOpener installs the host file layer that Database.attach uses for a file source
// (called once, at file.ts module load). The OPFS host would register its own.
export function registerFileAttachOpener(fn: FileAttachOpener): void {
  fileAttachOpener = fn;
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

  // fromEngine lifts a freshly opened/created/loaded Engine (file.ts / loadEngine) into a host
  // handle: its committed snapshot becomes the published root and it becomes the storage owner
  // (paging + page accounting). Since B3 every engine carries a paging context — a file's
  // FileBlockStore or an in-memory MemoryBlockStore — so this is the one constructor for both hosts.
  // Called by file.ts's createDatabase/openDatabase wrappers (file.ts is the node host module;
  // shared.ts stays browser-clean by not importing it). The committed snapshot's stores already
  // carry the shared paging, so every pinned/cloned snapshot faults clean pages through the one pool
  // (spec/design/pager.md).
  // The engine carries the handle's Environment (its session's), which the shared core adopts.
  // pageService carries a page-service owner or client (`locking = "auto"`, file.ts).
  static fromEngine(
    engine: Engine,
    coordinator: FileCoordinatorHost | null = null,
    pageService: {
      service?: ServiceOwnerHandle;
      client?: PageClientHandle;
      handoff?: ServiceHandoffHost;
    } = {},
  ): Database {
    // v25: the main domain (file or in-memory) reclaims within-session — the open path reads the
    // persisted free-list and no longer reconstructs it, so mid-session orphans must be returned at each
    // commit or they would leak permanently (format.md *Reclamation*).
    engine.reclaimWithinSession = true;
    const core = new SharedCore(engine.committed, engine, coordinator);
    core.readOnly = engine.readOnly;
    if (pageService.handoff !== undefined) {
      // Under the page service the opened role is the first one (README §6.5): its events are wired,
      // and a client's open-time pin is released at once (no local reader follows an open).
      core.handoff = pageService.handoff;
      core.installRole({
        kind: pageService.service !== undefined ? "owner" : "client",
        engine: null,
        service: pageService.service ?? null,
        client: pageService.client ?? null,
        coordinator,
      });
      return Database.over(core);
    }
    core.service = pageService.service ?? null;
    core.client = pageService.client ?? null;
    // A gate the service hands over between transactions is taken at the next event-loop turn.
    core.service?.onHandoff(() => core.serviceHandoff());
    // No local reader follows an open, so a client's open-time pin is released at once.
    core.releaseIdlePins();
    return Database.over(core);
  }

  // version is the committed version currently published (the monotonic commit counter,
  // transactions.md §8). Advances by 1 on every WriteHandle.commit.
  get version(): bigint {
    this.core.checkPid();
    this.core.refreshShared();
    return this.core.committed.txid;
  }

  // oldestLiveTxid is the oldest still-live snapshot version (transactions.md §8) — the Phase-6
  // reclamation watermark. With live readers it is the minimum version any of them pinned; with
  // none it is the committed version (nothing older is reachable).
  oldestLiveTxid(): bigint {
    return this.core.oldest();
  }

  // attach adds a database named `name` to this handle, reachable by the database qualifier
  // `name.table` (spec/design/attached-databases.md §4). Attaching is a HOST-API act, never SQL — an
  // untrusted, SQL-only session cannot attach anything (the pure-SQL safety spine, §4/§13). `source` is
  // either attachMemory() (a fresh, empty in-memory database) or attachFile(path) (an existing single-file
  // jed database on disk, Slice 2 — its committed state becomes the attachment's initial root, its own
  // page size honored). `readOnly` attaches it read-only: every write to it (DML or DDL) is 25006, it
  // never competes for the one-durable-writer slot (§5), and a file source is additionally opened
  // read-only. The name is exact (verbatim, never folded — grammar.md §3); it must not name a reserved database (`main` / `temp`) or one
  // already attached (42710). Opening a file surfaces the same host/file codes as opening `main`
  // (58P01/XX001/…). Publishing the new root replaces the attached map (a pinned reader keeps its old map).
  attach(name: string, source: AttachSource = attachMemory(), readOnly = false): void {
    const lname = name;
    if (lname === "") {
      throw engineError("duplicate_object", "attachment name must not be empty");
    }
    const c = this.core;
    let storage: Engine;
    let root: Snapshot;
    let coordinator: FileCoordinatorHost | null = null;
    let svc: ServiceAttachment | null = null;
    if (!source.file && source.credential !== undefined) {
      // Nothing is at rest in an in-memory database (spec/design/encryption.md §5).
      throw engineError(
        "object_not_in_prerequisite_state",
        "an in-memory database cannot be encrypted",
      );
    }
    if (source.file) {
      if (fileAttachOpener === null) {
        // A pure in-memory build (no node/OPFS host imported) has no file layer to reach.
        throw engineError("feature_not_supported", "file attachment needs a host file layer");
      }
      // Open the file BEFORE the dup check (an open may throw 58P01/XX001); on a name conflict close it.
      // The attachment resolves against, and runs with, main's environment (environment.md §3).
      const opened = fileAttachOpener(
        source,
        readOnly,
        c.storage.fileHost,
        c.environment,
        // A page client's own storage never batches; the handle's setting is its handoff's.
        c.handoff !== null ? c.handoff.relaxed : c.storage.relaxed,
        c.handoff,
        c.storage.spillSink?.encrypts?.() ?? false,
      );
      if (opened.kind === "service") {
        // Under the page service the attachment is owner or page client of its file, elected as an
        // open is (page-service.md §5.5); its storage is the inner handle's, whatever its role.
        const inner = coreOf(opened.service);
        // An encrypted attachment may not join a handle that spills to plaintext scratch files
        // (spec/fileformat/encrypted.md §11).
        if ((inner.storage.paging?.codec() ?? null) !== null && c.spillsPlaintext()) {
          opened.service.close();
          throw engineError(
            "object_not_in_prerequisite_state",
            "an encrypted database cannot be attached to a handle whose scratch files are not encrypted",
          );
        }
        inner.name = lname;
        if (lname === "main" || lname === "temp" || c.attachments.has(lname)) {
          opened.service.close(); // release the just-opened file — the name is taken
          throw engineError("duplicate_object", `database "${name}" already exists`);
        }
        inner.storage.storageBudget.limit = source.maxStorageBytes ?? 0n;
        svc = new ServiceAttachment(lname, readOnly, opened.service, inner, opened.realPath);
        try {
          c.acquireWriterScoped(0, false);
        } catch (error) {
          opened.service.close();
          throw error;
        }
        try {
          const service = svc;
          c.attachments.set(lname, {
            name: lname,
            readOnly,
            get storage(): Engine {
              return service.core.storage;
            },
            coordinator: null,
            svc: service,
          });
          c.serviceAttachCount++;
          // Publish a NEW attached map so a live reader's pinned map is unaffected.
          const na = new Map(c.attached);
          na.set(lname, inner.committed);
          c.attached = na;
        } finally {
          c.releaseWriter();
        }
        return;
      }
      const engine = opened.engine;
      // An encrypted attachment may not join a handle that spills to plaintext scratch files
      // (spec/fileformat/encrypted.md §11).
      const encrypted = engine.paging !== null && engine.paging.codec() !== null;
      if (encrypted && c.spillsPlaintext()) {
        engine.paging?.close();
        opened.coordinator?.close();
        throw engineError(
          "object_not_in_prerequisite_state",
          "an encrypted database cannot be attached to a handle whose scratch files are not encrypted",
        );
      }
      // The attachment's storage reaches the same file host as main (compaction, directory barrier).
      engine.fileHost = c.storage.fileHost;
      coordinator = opened.coordinator;
      if (lname === "main" || lname === "temp" || c.attachments.has(lname)) {
        engine.paging?.close(); // release the just-opened file — the name is taken
        coordinator?.close();
        throw engineError("duplicate_object", `database "${name}" already exists`);
      }
      // v25: a file attachment persists + reclaims like the main file domain.
      engine.reclaimWithinSession = true;
      engine.storageBudget.limit = source.maxStorageBytes ?? 0n;
      storage = engine; // its stores fault through engine.paging (bound at load); storePaging stays unset
      root = engine.committed;
    } else {
      if (lname === "main" || lname === "temp" || c.attachments.has(lname)) {
        throw engineError("duplicate_object", `database "${name}" already exists`);
      }
      storage = newAttachedStorage(c.pageSize, c.environment);
      storage.storageBudget.limit = source.maxStorageBytes ?? 0n;
      // The fresh attachment's committed root: an empty snapshot whose NEW stores attach to its own paging
      // (the same storePaging seam session-local temp uses — a snapshot's storePaging is "the paging new
      // stores bind to").
      const empty = new Snapshot(c.environment, 0n);
      empty.storePaging = storage.paging;
      root = empty;
    }
    try {
      c.acquireWriterScoped(0, false);
    } catch (error) {
      storage.paging?.close();
      coordinator?.close();
      throw error;
    }
    try {
      // Publish a NEW attached map so a live reader's pinned map is unaffected.
      c.attachments.set(lname, { name: lname, readOnly, storage, coordinator, svc: null });
      const na = new Map(c.attached);
      na.set(lname, root);
      c.attached = na;
    } finally {
      c.releaseWriter();
    }
  }

  // detach removes a previously attached database (spec/design/attached-databases.md §4/§8). A host-API
  // act. It is 55006 (object_in_use) while any live reader session / cursor still pins a committed
  // snapshot (the reader-liveness watermark, §5 — a reader pins the whole roots, so an open reader pins
  // every attachment), and 42704 if no database of that name is attached (`main` / `temp` are not
  // detachable). On success the attachment's root is dropped from the published roots and its storage
  // released.
  detach(name: string): void {
    const lname = name;
    const c = this.core;
    c.acquireWriterScoped(0, false);
    let att: Attachment;
    try {
      if (lname === "main" || lname === "temp" || !c.attachments.has(lname)) {
        throw engineError("undefined_object", `database "${name}" is not attached`);
      }
      if (c.hasLiveReaders()) {
        throw engineError("object_in_use", `cannot detach database "${name}" while it is in use`);
      }
      att = c.attachments.get(lname)!;
      c.attachments.delete(lname);
      if (att.svc !== null) {
        c.serviceAttachCount--;
        // Keep the plan-cache epoch monotone: planEpochNow no longer sums this inner handle's epoch.
        c.planEpoch += (att.svc as ServiceAttachment).core.planEpoch + 1;
      }
      const na = new Map(c.attached);
      na.delete(lname);
      c.attached = na;
    } finally {
      c.releaseWriter();
    }
    if (att.svc !== null) {
      // Close the inner handle: an owner drains its clients and hands ownership on; a client closes
      // its connection (README §6.5).
      (att.svc as ServiceAttachment).db.close();
      return;
    }
    // Release a file attachment's OS handle once it is unpublished and unreferenced (a no-op for an
    // in-memory attachment), its pending relaxed commits flushed first (validated-cow.md §4). No live
    // reader can still fault it — detach-in-use was rejected above. A failed flush is what detach
    // reports.
    const error = closeDomain(att.storage);
    att.coordinator?.close();
    if (error !== null) throw error;
  }

  // compact compacts database name — `main` or an attachment, by exact name — returning its dead
  // space (spec/design/api.md §2.6): rewrite it as the garbage-free from-scratch image of its committed
  // snapshot at the next version and swap the storage atomically (a file through temp file + rename, an
  // in-memory database by replacing its byte store). Rows, catalog, and later results and costs are
  // unchanged. Never waits: 42704 for a name that is not attached, 25006 for a read-only database, 0A000
  // on a host that cannot replace its storage, and 55006 while a write transaction or reader is open on
  // this handle or another process has the file open. Not metered and not reachable from SQL. On OPFS
  // the image is written to a journal file and copied over the database (hosts.md §5.1).
  compact(name: string): void {
    runSync(this.compactSteps(name));
  }

  // compactSteps is compact as a continuation (ts-runtime.md §11.1): an asynchronous codec or device
  // (the browser's WebCrypto, OPFS journal acquisition) is awaited. It cannot be canceled.
  *compactSteps(name: string): Steps<void> {
    yield* uncancelableSteps(this.compactAsSteps(name, name, null));
  }

  // rekey rewrites the file database name (`main` or a file attachment) under a NEW data key
  // (spec/fileformat/encrypted.md §12): encrypted for credential when it is given — rotating an
  // encrypted file's key, or encrypting a plaintext one — and plaintext when it is null, decrypting an
  // encrypted file. Every tree is rebuilt under the target page layout and the result replaces the file
  // as compact does, with its limits: it never waits, and a reader, a write transaction or another
  // process holding the file is 55006. When it returns, only credential unlocks the file; any copy made
  // before still opens with its old credential.
  //
  // A row or index key the target layout cannot store, or a result over max_storage_bytes (54P06),
  // fails before the file is replaced, leaving it untouched. Other errors: 42704 for a name that is not
  // attached, 25006 read-only, 55000 in memory or when encrypting through a handle whose scratch files
  // are plaintext, 0A000 for a page size below 1024 under encryption, 22023 for Argon2id parameters out
  // of range. Under the page service only the owner rekeys, with no client pinning a snapshot; a client
  // connected meanwhile fails 28P04 at its next transaction (spec/fileformat/encrypted.md §14). Not
  // reachable from SQL.
  rekey(
    name: string,
    credential: Credential | null,
    kdf: Argon2Params = DEFAULT_ARGON2_PARAMS,
  ): void {
    runSync(this.rekeySteps(name, credential, kdf));
  }

  // rekeySteps is rekey as a continuation (ts-runtime.md §11.1): the KDF, and an asynchronous codec or
  // device, are awaited. A cancellation can stop it only before the rewrite begins.
  *rekeySteps(
    name: string,
    credential: Credential | null,
    kdf: Argon2Params = DEFAULT_ARGON2_PARAMS,
  ): Steps<void> {
    const c = this.core;
    let storage: Engine;
    if (name === "main") {
      storage = c.storage;
    } else {
      const att = name === "temp" ? undefined : c.attachments.get(name);
      if (att === undefined) {
        throw engineError("undefined_object", `database "${name}" is not attached`);
      }
      storage = att.storage;
    }
    if (storage.persistHook === null) {
      // Nothing is at rest in an in-memory database (encryption.md §5).
      throw engineError(
        "object_not_in_prerequisite_state",
        "an in-memory database has no credential",
      );
    }
    // A durable store that cannot replace its storage is refused before the KDF runs rather than after.
    if (!canRewrite(storage)) {
      throw engineError(
        "feature_not_supported",
        `cannot rekey database "${name}": its storage host cannot replace the file atomically`,
      );
    }
    let target: PageCodec | null = null;
    if (credential !== null) {
      if (storage.pageSize < ENCRYPTED_MIN_PAGE_SIZE) {
        throw engineError(
          "feature_not_supported",
          "an encrypted database needs a page size of at least 1024",
        );
      }
      if (c.spillsPlaintext()) {
        throw engineError(
          "object_not_in_prerequisite_state",
          "cannot encrypt a database through a handle whose scratch files are not encrypted",
        );
      }
      // The new key block's data key, salt and nonce are the next draws of the database's
      // key-entropy stream (a new stream for a plaintext file); the KDF runs before any gate is
      // taken (encrypted.md §12).
      const source = storage.paging?.codec() ?? null;
      const rng = source !== null ? source.keyEntropy() : c.environment.keyStream();
      target = yield* createCodecSteps(credential, kdf, rng);
    }
    yield* uncancelableSteps(this.compactAsSteps(name, name, { target }));
  }

  // changeCredential changes the credential of the encrypted file database name (`main` or a file
  // attachment) to credential (spec/design/encryption.md §5): the data key is rewrapped in the key block
  // and no page is rewritten, so the change is quick apart from kdf's Argon2id for a passphrase, which
  // runs before the writer gate is taken. Once it returns, only credential unlocks the file; if it fails
  // partway, the handle must be reopened and exactly one of the two credentials unlocks it.
  //
  // Never waits: 42704 for a name that is not attached, 25006 for a read-only database, 55000 for an
  // in-memory database, 28P03 for a plaintext file (a database the page service shares is never
  // encrypted), 55006 while a write transaction is open, and 22023 for Argon2id parameters out of range
  // or a malformed credential. Not reachable from SQL.
  changeCredential(
    name: string,
    credential: Credential,
    kdf: Argon2Params = DEFAULT_ARGON2_PARAMS,
  ): void {
    runSync(this.changeCredentialSteps(name, credential, kdf));
  }

  // changeCredentialSteps is changeCredential as a continuation (spec/design/ts-runtime.md §11): an
  // asynchronous page codec (the browser's WebCrypto) awaits the rewrap, and an asynchronous device the
  // key-block writes and barriers. Cancellation can stop it only before the writer gate is taken; the
  // two-barrier write runs to its end.
  *changeCredentialSteps(
    name: string,
    credential: Credential,
    kdf: Argon2Params = DEFAULT_ARGON2_PARAMS,
  ): Steps<void> {
    const c = this.core;
    let storage: Engine;
    if (name === "main") {
      if (c.readOnly) throw readOnlyCredential(name);
      storage = c.storage;
    } else {
      const att = name === "temp" ? undefined : c.attachments.get(name);
      if (att === undefined) {
        throw engineError("undefined_object", `database "${name}" is not attached`);
      }
      if (att.readOnly) throw readOnlyCredential(name);
      // A file attachment under the page service changes its credential through its own handle.
      if (att.svc !== null) {
        yield* (att.svc as ServiceAttachment).db.changeCredentialSteps("main", credential, kdf);
        return;
      }
      storage = att.storage;
    }
    const codec = storage.paging?.codec() ?? null;
    if (codec === null) {
      // Nothing is at rest in an in-memory database (encryption.md §5).
      if (storage.persistHook === null) {
        throw engineError(
          "object_not_in_prerequisite_state",
          "an in-memory database has no credential",
        );
      }
      throw notEncrypted();
    }
    // Only the owner of a database the page service shares writes its key block; a page client asks
    // the host to change it there (spec/fileformat/encrypted.md §14).
    if (name === "main" && (c.client !== null || (c.handoff !== null && c.service === null))) {
      throw engineError(
        "object_not_in_prerequisite_state",
        "only the process that owns the database can change its credential",
      );
    }
    // Derive the new wrapping key first: Argon2id takes long enough that nobody should wait for it
    // behind the writer gate (encryption.md §5).
    const block = yield* codec.rewrapSteps(credential, kdf);
    if (!c.tryAcquireLocalWriter()) {
      throw engineError(
        "object_in_use",
        `cannot change the credential of database "${name}" while a write transaction is open`,
      );
    }
    try {
      yield* uncancelableSteps(writeKeyBlockSteps(storage, block));
    } finally {
      c.releaseWriter();
    }
  }

  // compactAs compacts database lname of this handle, naming it name in errors (an inner handle of a
  // service attachment compacts its main under the attachment's name).
  private *compactAsSteps(lname: string, name: string, rekey: Rekey | null): Steps<void> {
    const c = this.core;
    let storage: Engine;
    let readOnly: boolean;
    let service: ServiceAttachment | null = null;
    if (lname === "main") {
      storage = c.storage;
      readOnly = c.readOnly;
    } else {
      const att = lname === "temp" ? undefined : c.attachments.get(lname);
      if (att === undefined) {
        throw engineError("undefined_object", `database "${name}" is not attached`);
      }
      storage = att.storage;
      readOnly = att.readOnly;
      service = att.svc as ServiceAttachment | null;
    }
    if (readOnly) {
      throw engineError("read_only_sql_transaction", `cannot compact read-only database "${name}"`);
    }
    if (service !== null) {
      // A service attachment is compacted by its inner handle, under its own gate and watermark
      // (page-service.md §6): only its owner can, and only with no pin from any process.
      if (!c.tryAcquireLocalWriter()) {
        throw engineError(
          "object_in_use",
          `cannot compact database "${name}" while a write transaction is open`,
        );
      }
      try {
        yield* service.db.compactAsSteps("main", name, rekey);
      } finally {
        c.releaseWriter();
      }
      return;
    }
    // A durable store is replaced through its file host or rewriter (hosts.md §5.1).
    const durable = storage.path !== null || storage.persistHook !== null;
    if (durable && !canRewrite(storage)) {
      throw engineError(
        "feature_not_supported",
        `cannot compact database "${name}": its storage host cannot replace the file atomically`,
      );
    }
    if ((c.client !== null || (c.handoff !== null && c.service === null)) && lname === "main") {
      // Compaction is an owner operation (page-service.md §6).
      throw engineError(
        "object_in_use",
        `cannot compact database "${name}" while another process has it open`,
      );
    }
    if (c.service !== null && lname === "main") c.serviceRefresh();
    if (!c.tryAcquireLocalWriter()) {
      throw engineError(
        "object_in_use",
        `cannot compact database "${name}" while a write transaction is open`,
      );
    }
    try {
      if (c.service !== null && lname === "main") {
        // The owner's gate is shared with client write transactions (page-service.md §6): taken
        // without waiting, then any client commit is adopted before the rewrite.
        if (!c.tryServiceGate()) {
          throw engineError(
            "object_in_use",
            `cannot compact database "${name}" while a write transaction is open`,
          );
        }
        c.serviceRefresh();
      }
      yield* c.compactLockedSteps(lname, name, rekey);
    } finally {
      c.releaseWriter();
    }
  }

  // readSession opens a READ ONLY session over a consistent snapshot (spec/design/session.md §2.4,
  // transactions.md §10). Pins the committed snapshot now and registers its version in the live set;
  // the session serves reads from that snapshot for its life — observing one stable version even as a
  // writer interleaves commits — and a write through it is 25006. The caller must close() it to
  // deregister (advancing the watermark), since JS has no destructor. (The old SharedDb.read().)
  readSession(opts?: SessionOptions): Session {
    this.core.checkPid();
    const pinned = this.core.refreshAndPinAll();
    const snap = this.core.committed; // pinned (immutable — no clone)
    const engine = databaseFromSnapshot(snap, this.core);
    if (opts !== undefined) engine.session = new SessionState(this.core.environment, opts);
    // The attached roots are pinned together with the committed root (attached-databases.md §5), so the
    // session sees a consistent cross-database snapshot; it routes attachment persists via the core.
    // Each service attachment's pinned snapshot is overlaid (page-service.md §5.5).
    engine.core = this.core;
    engine.attachedCommitted = overlayAttachPins(this.core.attached, pinned.atts);
    return new Session(this.core, engine, "ro", snap.txid, snap.txid, false, pinned.atts);
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
    this.core.acquireWriter(0);
    try {
      const base = this.core.committed;
      // committed = the immutable base; beginTx clones it to working.
      const engine = databaseFromSnapshot(base, this.core);
      engine.core = this.core;
      engine.attachedCommitted = this.core.writerView(); // pin the attached roots together (§5)
      engine.beginTx(true);
      return new Session(this.core, engine, "rw", base.txid, null, true);
    } catch (error) {
      this.core.releaseWriter();
      throw error;
    }
  }

  // session mints an ADDITIONAL configured session over this database (spec/design/session.md
  // §2.1/§2.4), with its own envelope from opts. The session shares committed storage with every
  // other session over this Database, and runs autocommit with the lazy gate: an autocommit read
  // pins the latest committed for that one statement (no gate); an autocommit write takes the gate
  // per statement (throwing 25001 if another writer is open), publishes, releases;
  // BEGIN/COMMIT/ROLLBACK open and end an explicit block. (The old db.newSession swap → an
  // independent owns-its-Engine session.)
  session(opts: SessionOptions = {}): Session {
    this.core.checkPid();
    // A read-only file-backed core mints read-only sessions, pinned at the refreshed version.
    const atts = this.core.readOnly ? this.core.refreshAndPinAll().atts : [];
    const snap = this.core.committed;
    const engine = databaseFromSnapshot(snap, this.core);
    engine.session = new SessionState(this.core.environment, opts);
    engine.core = this.core;
    // pin the attached roots together (§5), each service attachment's pinned snapshot overlaid
    engine.attachedCommitted = overlayAttachPins(this.core.attached, atts);
    // A read-only file-backed core mints read-only sessions (a write is 25006); it pins the committed
    // version in the watermark like a read session. A writable core mints the autocommit lazy-gate one.
    if (this.core.readOnly) {
      return new Session(this.core, engine, "ro", snap.txid, snap.txid, false, atts);
    }
    return new Session(this.core, engine, "rw", snap.txid, null);
  }

  // --- Bare convenience methods (CLAUDE.md §2 / spec/design/session.md §2.4): each mints a FRESH
  // autocommit session, runs the statement, and discards it. Committed data persists through the shared
  // core; session-local state (an open block, session variables, currval, session-local temp tables)
  // does NOT carry to the next call — for durable connection state mint an explicit session(). ---

  // execute runs a (possibly mutating) statement on a fresh autocommit session, binding $N params, and
  // returns its command tag (exec-side sugar over the total query seam).
  execute(sql: string, params: Value[] = []): RunResult {
    const s = this.session({});
    try {
      return s.execute(sql, params);
    } finally {
      s.close();
    }
  }
  // query runs a query on a fresh autocommit session, returning a row cursor. A streaming cursor owns
  // its snapshot (streaming.md §5), so it stays valid after the transient session is closed; its
  // watermark pin is held by the Rows (released on its close), not by the session.
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
  executeCancelable(sql: string, params: Value[] = [], signal?: AbortSignal): RunResult {
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

  // prepareStatement parses sql once into a reusable PreparedStatement (spec/design/api.md §2.4): a
  // standalone value bound to no session — run it with queryPrepared / executePrepared on any handle
  // over this database (the statement outlives the transient session used to parse it). (Named
  // prepareStatement because prepare() is the better-sqlite3-style ergonomic Statement below — a
  // TS-only naming divergence, api.md §6.)
  prepareStatement(sql: string): PreparedStatement {
    const s = this.session({});
    try {
      return s.prepareStatement(sql);
    } finally {
      s.close();
    }
  }

  // executePrepared runs a prepared statement on a fresh autocommit session, binding $N params, and
  // returns its command tag — the prepared analogue of execute (spec/design/api.md §2.4).
  executePrepared(stmt: PreparedStatement, params: Value[] = []): RunResult {
    const s = this.session({});
    try {
      return s.executePrepared(stmt, params);
    } finally {
      s.close();
    }
  }

  // queryPrepared runs a prepared query on a fresh autocommit session, returning a row cursor — the
  // prepared analogue of query (the streaming cursor owns its snapshot, so it stays valid after the
  // transient session is closed).
  queryPrepared(stmt: PreparedStatement, params: Value[] = []): Rows {
    const s = this.session({});
    try {
      return s.queryPrepared(stmt, params);
    } finally {
      s.close();
    }
  }

  // --- better-sqlite3-style ergonomic methods (spec/design/api.md §11): a reusable prepared
  // Statement, or one-shot run/get/all over native JS params + rows-as-objects. Like execute/query
  // above, each one-shot mints a fresh autocommit session under the hood (via the Statement). ---

  // prepare returns a reusable Statement bound to this handle (better-sqlite3's db.prepare).
  prepare(sql: string): ErgoStatement {
    return new ErgoStatement(this, sql);
  }
  // run is the one-shot Statement.run: execute a statement with native params, return its command tag.
  run(sql: string, ...params: JsParam[]): RunResult {
    return new ErgoStatement(this, sql).run(...params);
  }
  // get is the one-shot Statement.get: the first row of a query as an object, or undefined.
  get(sql: string, ...params: JsParam[]): ErgoRow | undefined {
    return new ErgoStatement(this, sql).get(...params);
  }
  // all is the one-shot Statement.all: every row of a query as an object.
  all(sql: string, ...params: JsParam[]): ErgoRow[] {
    return new ErgoStatement(this, sql).all(...params);
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
  // close closes the backing byte store (an in-memory store's close is a no-op). The bare
  // convenience methods autocommit, so there is never uncommitted work to discard. Idempotent.
  close(): void {
    const handoff = this.core.handoff;
    if (handoff !== null) {
      // A serving owner drains its clients and stops serving while presence EX is still held; a client
      // closes its connection (page-service.md §4.1; README §6.5). The path claim goes last.
      if (this.core.handoffClosed) return;
      this.core.closeService();
      try {
        this.closeStorage();
      } finally {
        handoff.releasePath();
      }
      return;
    }
    // A page-service owner stops serving first: clients see EOF while presence EX is still held
    // (page-service.md §4.1). A client's connection closes with its byte store below.
    this.core.service?.close();
    this.core.service = null;
    this.core.client?.close();
    this.closeStorage();
  }

  // closeSteps is close with an awaitable flush of each relaxed domain's pending commits over an
  // asynchronous storage stack (ts-runtime.md §11.1); close then finds nothing pending. Page-service
  // handles keep close's synchronous order (stop serving, then flush).
  *closeSteps(): Steps<void> {
    let error: unknown = null;
    const core = this.core;
    if (core.handoff === null && core.service === null && core.client === null) {
      const domains = [core.storage];
      for (const att of core.attachments.values()) if (att.svc === null) domains.push(att.storage);
      for (const st of domains) {
        const paging = st.paging;
        if (paging === null || !paging.writeSideAsync()) continue;
        try {
          yield* paging.flushPendingSteps();
        } catch (e) {
          error ??= e;
        }
      }
    }
    try {
      this.close();
    } catch (e) {
      error ??= e;
    }
    if (error !== null) throw error;
  }

  // closeStorage releases main and every attachment, each relaxed domain's pending commits flushed
  // first (validated-cow.md §4); it throws the first failed flush or close after releasing them all.
  private closeStorage(): void {
    const st = this.core.storage;
    let error = closeDomain(st);
    st.paging = null;
    // Release any still-attached file databases (an in-memory attachment's close is a no-op), so the host
    // need not detach before close (attached-databases.md §4). Order-independent (just closing).
    // A service attachment's inner handle closes last, in real-path order: an owner drains its clients
    // and hands ownership on, a client closes its connection (README §6.5).
    const inners = this.core.serviceAttachments();
    for (const att of this.core.attachments.values()) {
      if (att.svc !== null) continue;
      const e = closeDomain(att.storage);
      if (error === null) error = e;
      att.coordinator?.close();
    }
    this.core.attachments = new Map();
    this.core.serviceAttachCount = 0;
    for (const inner of inners) {
      try {
        inner.db.close();
      } catch (e) {
        // The other handles still close; the first failure (a failed flush) is what close reports.
        if (error === null) error = e;
      }
    }
    this.core.coordinator?.close();
    this.core.coordinator = null;
    if (error !== null) throw error;
  }

  // fromImage lifts a from-scratch on-disk image (spec/fileformat/format.md) into an in-memory host
  // handle — the inverse of toImage, used by the byte-level golden round-trip tests and by a host
  // rehydrating an in-memory database from bytes. Since B3 the image becomes the core's
  // MemoryBlockStore, demand-paged like a file (one read path); there is no backing file (path stays
  // null) and a commit packs pages into the memory store.
  // environment: absent ⇒ a fresh empty one (environment.md §7).
  static fromImage(image: Uint8Array, environment: Environment = new Environment()): Database {
    return Database.fromEngine(loadEngine(image, environment));
  }

  // --- Catalog / storage introspection (spec/design/api.md §6): reads the latest committed snapshot.
  // Not the embedding API — hosts introspect through SQL (the jed_ catalog relations,
  // introspection.md). table()/compositeType()/rowsInKeyOrder() return the doc-hidden introspection
  // detail white-box tests + the in-repo tools reach for (CLAUDE.md §10). ---

  private committedEngine(): Engine {
    return databaseFromSnapshot(this.core.committed, this.core);
  }
  table(name: string): Table | undefined {
    return this.committedEngine().table(name);
  }
  compositeType(name: string): CompositeType | undefined {
    return this.committedEngine().compositeType(name);
  }
  rowsInKeyOrder(name: string): Row[] {
    return this.committedEngine().rowsInKeyOrder(name);
  }
  // toImage serializes the whole committed state to a from-scratch on-disk image (the inverse of
  // fromImage), used by the byte-level golden round-trip tests and by hosts snapshotting to bytes.
  toImage(pageSize: number, txid: bigint): Uint8Array {
    return toImageBytes(this.core.committed, pageSize, txid);
  }
  // txid is the latest committed transaction id (equal to version).
  get txid(): bigint {
    // A page-service owner first adopts any client commit (one atomic load when there is none).
    if (this.core.service !== null) this.core.serviceRefresh();
    return this.core.committed.txid;
  }
  // pageSize is the page payload size this database serializes at.
  get pageSize(): number {
    return this.core.pageSize;
  }
  // pageCount is the byte store's page high-water (since B3 an in-memory database has a real one —
  // its MemoryBlockStore's committed page count).
  get pageCount(): number {
    if (this.core.service !== null) this.core.serviceRefresh();
    return this.core.storage.pageCount;
  }
  // setMaxStorageBytes sets the committed-storage limit of database name — main or an attachment — in
  // bytes (spec/design/memory.md §8.1); zero or negative is unlimited. Shared by every session on the
  // handle and checked at each later commit: an in-memory database's page high-water (§8.2), a
  // file-backed database's live pages (§8.7). A name that is not attached is 42704.
  setMaxStorageBytes(name: string, bytes: bigint): void {
    this.core.storageFor(name).storageBudget.limit = bytes;
  }

  // storageBytes is the committed storage of database name — main or an attachment — in bytes, the
  // measure maxStorageBytes limits: an in-memory database's logical page high-water times its page size
  // (spec/design/memory.md §8.2), a file-backed database's live pages times its page size (§8.7).
  // Deterministic; not RSS, and not the file length.
  storageBytes(name: string): bigint {
    return storageBytes(this.core.storageFor(name));
  }

  // path is the backing file path for a file-backed database; null in-memory.
  get path(): string | null {
    return this.core.storage.path;
  }
  // readOnly reports whether this database was opened read-only. In-memory databases are writable.
  get readOnly(): boolean {
    return this.core.readOnly;
  }
  // collations / loadedCollations / defaultCollation report the collation catalog (collation.md §12).
  collations(): CollationInfo[] {
    return this.committedEngine().collations();
  }
  loadedCollations(): CollationInfo[] {
    return this.core.environment.loadedCollations();
  }
  // loadedTimeZones lists the zones the handle's environment loaded (timezones.md §3.3).
  loadedTimeZones(): TimeZoneInfo[] {
    return this.core.environment.loadedTimeZones();
  }
  // environment is the handle's Environment (spec/design/environment.md §6): fixed at open/create,
  // shared by every session, attachment, and temp domain.
  environment(): Environment {
    return this.core.environment;
  }
  defaultCollation(): string {
    return this.committedEngine().defaultCollation();
  }
  // setDefaultCollation / upgradeCollations mint a fresh write session, apply the collation change
  // (which commits through the shared core), and discard it — the bare-convenience form of the Session
  // methods (collation.md §12).
  setDefaultCollation(name: string): void {
    const s = this.session({});
    try {
      s.setDefaultCollation(name);
    } finally {
      s.close();
    }
  }
  upgradeCollations(): number {
    const s = this.session({});
    try {
      return s.upgradeCollations();
    } finally {
      s.close();
    }
  }
}

// buildInMemory builds a fresh, empty in-memory Database whose stores serialize/split at pageSize —
// the in-memory backing of the unified createDatabase (spec/design/api.md §2.1.1). NOT part of the
// public API: it is a module-level function, NOT re-exported by lib.ts. Its callers are file.ts's
// createDatabase (the in-memory branch) and the tests' memDb helper; keeping it here (not in file.ts)
// keeps shared.ts browser-clean (file.ts imports from shared.ts, never the reverse). The page-backed
// B-tree's fan-out tracks the page size (spec/fileformat/format.md), so an in-memory tree must be
// built at the size it will serialize to — a caller that round-trips through toImage(pageSize) passes
// that size.
//
// B3 (bplus-reshape.md): an in-memory database is a MemoryBlockStore seeded with the empty
// from-scratch image, read/written through the same pager + Packed path as a file (loadEngine is the
// paged open over a memory store). txid 0 is the pre-first-commit version (the same committed version
// an in-memory core always started at); the first commit publishes txid 1 into the alternate meta slot.
// maxStorageBytes is the database's committed-storage limit (memory.md §8.1); <= 0n is unlimited.
export function buildInMemory(
  pageSize: number,
  environment: Environment = new Environment(),
  maxStorageBytes = 0n,
): Database {
  const image = toImageBytes(new Snapshot(environment, 0n), pageSize, 0n);
  const engine = loadEngine(image, environment);
  engine.storageBudget.limit = maxStorageBytes;
  return Database.fromEngine(engine, null);
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
  private planEpoch: number;
  private closed = false;
  // roleGen is the page-service role generation the open transaction or pinned snapshot began under,
  // pinGen the one pinnedVersion was registered under (spec/pageservice/README.md §6.5).
  private roleGen: number;
  private pinGen: number;
  // pinAtts are the pins on file attachments under the page service taken with pinnedVersion, released
  // with it; stmtAtts are an autocommit read's, for the one statement (page-service.md §5.5).
  private pinAtts: AttachPin[];
  private stmtAtts: AttachPin[] | null = null;

  constructor(
    core: SharedCore,
    engine: Engine,
    access: Access,
    baseVersion: bigint,
    pinnedVersion: bigint | null,
    gateHeld = false,
    pinAtts: AttachPin[] = [],
  ) {
    this.core = core;
    this.engine = engine;
    this.access = access;
    this.baseVersion = baseVersion;
    this.pinnedVersion = pinnedVersion;
    this.gateHeld = gateHeld;
    this.pinAtts = pinAtts;
    this.planEpoch = core.planEpochNow();
    this.roleGen = core.roleGen;
    this.pinGen = core.roleGen;
  }

  // checkRole fails a statement of a transaction (or a read-only session's pinned snapshot) that began
  // under a page-service role the handle no longer has, or whose connection ended (README §6.5) — main's,
  // or that of a file attachment under the page service the transaction pinned or took the gate of.
  private checkRole(): void {
    const core = this.core;
    if (!core.hasSharedCoordinator()) return;
    const open =
      this.engine.session.tx !== null || (this.access === "ro" && this.pinnedVersion !== null);
    if (!open) return;
    if (core.handoff !== null && (this.roleGen !== core.roleGen || core.client?.lost())) {
      throw errStaleRole();
    }
    // A file attachment under the page service changes role on its own.
    if (this.gateHeld) core.writerStale();
    else if (this.pinnedVersion !== null) attachPinsStale(this.pinAtts);
  }

  // execute runs a (possibly mutating) statement on this session, binding $N params (spec/design/
  // api.md §5). Routes by the session's state (read-only / open block / autocommit) with the
  // lazy-gate lifecycle (§2.4).
  execute(sql: string, params: Value[] = []): RunResult {
    return runSync(this.executeSteps(sql, params));
  }

  *executeSteps(sql: string, params: Value[] = []): Steps<RunResult> {
    return yield* drainRunSteps(yield* this.querySteps(sql, params));
  }

  // query runs a query on this session, returning a row cursor. A single-table no-blocking-operator
  // read is served by a lazy STREAMING cursor (spec/design/streaming.md §4, S3); a blocking read
  // (ORDER BY/DISTINCT/aggregate/window/join) by a lazy BUFFERED cursor (S4) that buffers its input but
  // yields the output one row at a time. The read is routed first (an autocommit read re-pins the latest
  // committed, PG-faithful), then the lazy cursor runs over the pinned snapshot — bounded peak output
  // memory, early-exit — and its snapshot version is registered in the reader-liveness watermark
  // (streaming.md §5), released on close. A top-level set operation / pure-query WITH is served by a lazy
  // DEFERRED cursor (streaming.md §7) that defers the whole run to the first pull and yields the result
  // one row at a time; a data-modifying WITH (a write) still falls back to the materialized dispatch path.
  query(sql: string, params: Value[] = []): Rows {
    return runSync(this.querySteps(sql, params)); // one-shot: no cross-call plan cache (still plans once)
  }

  *querySteps(sql: string, params: Value[] = []): Steps<Rows> {
    return yield* this.queryStmtSteps(this.engine.parse(sql), params, null, null);
  }

  // queryStmt routes an already-parsed query AST through the session's lazy lanes — the autocommit
  // re-pin, the plan-once scan (streaming/buffered) then deferred cursors, and the reader-liveness
  // watermark pin — falling back to the materialized dispatch for a shape no lazy lane covers (a
  // write, a data-modifying WITH). Shared by query (parse-then-route, holder null) and a prepared
  // query (queryPrepared passes the statement's ScanCacheHolder), so a prepared query streams and
  // pins its snapshot exactly like an ad-hoc one but reuses its cached plan across executes.
  private *queryStmtSteps(
    stmt: Statement,
    params: Value[],
    holder: ScanCacheHolder | null,
    insertHolder: InsertCacheHolder | null,
  ): Steps<Rows> {
    // Route the read before building the lazy cursor: an autocommit (non-block, writable access) read
    // re-pins the latest committed so the snapshot is current; a read-only session uses its existing
    // pin, and an open block uses its working set.
    // On a page client that re-pin is a begin_read whose pin stays registered for this statement
    // (spec/pageservice/README.md §6.3); a cursor that outlives it registers its own.
    if (stmt.kind !== "rollback") this.checkRole();
    let autoPin: bigint | null = null;
    let autoPinGen = 0;
    if (
      this.access !== "ro" &&
      this.engine.session.tx === null &&
      !this.engine.stmtIsWriteWithViews(stmt)
    ) {
      // Under the page service the read begins on the current role (re-electing when needed), and on
      // every file attachment under it (page-service.md §5.5).
      if (
        this.core.handoff !== null ||
        this.core.client !== null ||
        this.core.serviceAttachCount > 0
      ) {
        const pinned = this.core.refreshAndPinAll();
        autoPin = pinned.version;
        autoPinGen = pinned.gen;
        this.stmtAtts = pinned.atts;
      } else this.core.refreshShared();
      this.refreshCommitted();
    }
    if (autoPin === null) return yield* this.routeStmtSteps(stmt, params, holder, insertHolder);
    try {
      return yield* this.routeStmtSteps(stmt, params, holder, insertHolder);
    } finally {
      // Main's pin first, then the attachments' in the order they were taken.
      const atts = this.stmtAtts ?? [];
      this.stmtAtts = null;
      this.core.deregister(autoPin, autoPinGen);
      releaseAttachPins(atts);
    }
  }

  // routeStmt is queryStmt after the autocommit re-pin: the plan-cache epoch check, the lazy lanes,
  // and the materialized dispatch.
  private *routeStmtSteps(
    stmt: Statement,
    params: Value[],
    holder: ScanCacheHolder | null,
    insertHolder: InsertCacheHolder | null,
  ): Steps<Rows> {
    const epoch = this.core.planEpochNow();
    if (this.planEpoch !== epoch) {
      if (holder !== null) holder.cache = null;
      if (insertHolder !== null) insertHolder.cache = null;
      this.planEpoch = epoch;
    }
    // A read served by a lazy lane never reaches the materialized dispatch, so enforce the read-path
    // admission gates (failed-block 25P02 / lifetime 54P02 / privilege 42501) here — after refreshing so
    // privilege resolution sees the snapshot the read will use. Reads only: transaction control must
    // still work in a failed block, and a write is gated inside dispatch on the fall-through below
    // (executor.ts gateReadLanes — the safe-total-query contract, CLAUDE.md §13).
    const isRead =
      stmt.kind !== "begin" &&
      stmt.kind !== "commit" &&
      stmt.kind !== "rollback" &&
      stmt.kind !== "savepoint" &&
      !this.engine.stmtIsWriteWithViews(stmt);
    // pin registers the cursor's snapshot version in the watermark (streaming.md §5); the deregister
    // runs on cursor close (JS has no destructor), advancing oldestLiveTxid.
    const pin = (built: { columnNames: string[]; columnTypes: string[]; cursor: Cursor }): Rows => {
      const version = this.baseVersion;
      const gen = this.core.roleGen;
      this.core.register(version);
      // The cursor pins the service attachments it reads beside main (page-service.md §5.5).
      const atts = this.core.cursorAttachPins(this.stmtAtts ?? this.pinAtts, this.gateHeld);
      // A live streaming cursor also blocks within-session temp compaction: it faults its pinned temp
      // tree lazily, so a temp commit must not reclaim a page it may still read (temp-tables.md §6). The
      // counter is on the session's engine (single-threaded), like the write path it gates.
      this.engine.openStreams++;
      const rows = new Rows(built.columnNames, built.columnTypes, built.cursor, null);
      rows.attachPin(() => {
        this.engine.openStreams--;
        this.core.deregister(version, gen);
        releaseAttachPins(atts);
      });
      // A drain-time fault inside an open block aborts it (the open-time lane errors are poisoned at the
      // catch below); a no-op for an autocommit read.
      if (this.engine.session.tx !== null) {
        rows.attachErrHook(() => this.engine.failOpenBlock());
      }
      return rows;
    };
    try {
      if (isRead) this.engine.gateReadLanes(stmt);
      // One plan-once scan lane serves both streaming and buffered shapes (an ad-hoc call plans once,
      // holder null; a prepared statement reuses its cached plan). Both are live readers and pin their
      // snapshot in the watermark.
      const scanned = yield* this.engine.tryScanQuerySteps(stmt, params, holder);
      if (scanned !== null) return pin(scanned);
      // A top-level set operation / pure-query WITH is served by a lazy DEFERRED cursor (streaming.md
      // §7): it defers the whole run to the first pull and yields the result one row at a time; it is a
      // live reader too and pins its snapshot in the watermark.
      const deferred = this.engine.tryDeferredQuery(stmt, params);
      if (deferred !== null) return pin(deferred);
    } catch (e) {
      // An open-time lane error (a missing table, a denied read, a plan-time trap, or a gate rejection)
      // aborts an open block — the counterpart to the drain-time hook above.
      this.engine.failOpenBlock();
      throw e;
    }
    // The dispatch fall-through handles transaction control (a nested BEGIN's 25001 must NOT poison) and
    // self-poisons on a regular statement error (executeStmtParams), so its nuanced poisoning is left
    // intact — only the lazy-lane reads above, which bypass it, are poisoned here.
    const outcome = yield* this.dispatchSteps(stmt, params, insertHolder);
    return rowsFromOutcome(outcome, this.engine.session);
  }

  // executeCancelable runs a statement under an AbortSignal (spec/design/api.md §11.4): if the signal
  // is already aborted it throws 57014 query_canceled before any work, else it runs normally. TS is
  // synchronous (one event loop), so the signal cannot flip mid-statement — the check is at this
  // boundary only, the deliberate per-language divergence from Go/Rust's mid-statement meter poll (the
  // cancel.ts note). Useful for skipping work an already-canceled caller no longer wants.
  executeCancelable(sql: string, params: Value[] = [], signal?: AbortSignal): RunResult {
    throwIfAborted(signal);
    return this.execute(sql, params);
  }

  // queryCancelable is the query sibling of executeCancelable (spec/design/api.md §11.4).
  queryCancelable(sql: string, params: Value[] = [], signal?: AbortSignal): Rows {
    throwIfAborted(signal);
    return this.query(sql, params);
  }

  // prepareStatement parses sql once into a reusable PreparedStatement (spec/design/api.md §2.4): a
  // standalone value bound to no session — this session only supplies the parse (its 54000 input-size
  // limit). Run it with queryPrepared / executePrepared on any handle over this database; the
  // executing handle supplies the session each run observes (privileges, snapshot, temp domain).
  // (Named prepareStatement because prepare() is the better-sqlite3-style ergonomic Statement below —
  // a TS-only naming divergence, api.md §6.)
  prepareStatement(sql: string): PreparedStatement {
    return new PreparedStatement(this.engine.parse(sql));
  }

  // The async worker scheduler parks a statement BEFORE its first execution when another local
  // session holds the writer gate. This is an admission check, never an execute/catch/retry path:
  // SQL, entropy, host functions and cost must run exactly once. Existing transaction errors still
  // reach the normal dispatcher; only a fresh autocommit write or writable BEGIN needs to wait.
  // @internal The synchronous embedding API keeps its existing immediate-gate behavior.
  waitsForWriter(stmt: PreparedStatement): boolean {
    if (
      this.access === "ro" ||
      this.gateHeld ||
      this.engine.session.tx !== null ||
      !this.core.writerActive
    ) {
      return false;
    }
    if (stmt.ast.kind === "begin") return stmt.ast.writable !== false;
    // A view may have acquired sequence-mutating dependencies since this session's last query.
    // Classify against the same committed catalog its eventual autocommit statement will use.
    this.refreshCommitted();
    return this.engine.stmtIsWriteWithViews(stmt.ast);
  }

  // @internal Cancellation can arrive while a worker request is parked or parsing, before the
  // ordinary executor error boundary. Preserve the same failed-block rule as a metered SQL error.
  failCurrentStatement(): void {
    this.engine.failOpenBlock();
  }

  // executePrepared runs a prepared statement on this session, binding $N params, and returns its
  // command tag — the prepared analogue of execute (spec/design/api.md §2.4).
  executePrepared(stmt: PreparedStatement, params: Value[] = []): RunResult {
    return runSync(this.executePreparedSteps(stmt, params));
  }

  // queryPrepared runs a prepared query on this session (its pinned snapshot, privileges, temp
  // domain), returning a row cursor. The prepared AST routes through the same lazy lanes as the
  // ad-hoc query — so a prepared query streams and pins its snapshot identically — but reuses its
  // cached plan across executes (and across sessions of one database, spec/design/api.md §2.4).
  queryPrepared(stmt: PreparedStatement, params: Value[] = []): Rows {
    return runSync(this.queryPreparedSteps(stmt, params));
  }

  *queryPreparedSteps(stmt: PreparedStatement, params: Value[] = []): Steps<Rows> {
    return yield* this.queryStmtSteps(stmt.ast, params, stmt.scHolder, stmt.icHolder);
  }

  *executePreparedSteps(stmt: PreparedStatement, params: Value[] = []): Steps<RunResult> {
    return yield* drainRunSteps(yield* this.queryPreparedSteps(stmt, params));
  }

  // --- better-sqlite3-style ergonomic methods (spec/design/api.md §11): a reusable prepared
  // Statement, or one-shot run/get/all over native JS params + rows-as-objects, on this durable
  // session (so an open block / session variables persist across calls, unlike the Database shims). ---

  // prepare returns a reusable Statement bound to this session (better-sqlite3's db.prepare).
  prepare(sql: string): ErgoStatement {
    return new ErgoStatement(this, sql);
  }
  // run is the one-shot Statement.run: execute a statement with native params, return its command tag.
  run(sql: string, ...params: JsParam[]): RunResult {
    return new ErgoStatement(this, sql).run(...params);
  }
  // get is the one-shot Statement.get: the first row of a query as an object, or undefined.
  get(sql: string, ...params: JsParam[]): ErgoRow | undefined {
    return new ErgoStatement(this, sql).get(...params);
  }
  // all is the one-shot Statement.all: every row of a query as an object.
  all(sql: string, ...params: JsParam[]): ErgoRow[] {
    return new ErgoStatement(this, sql).all(...params);
  }

  private *dispatchSteps(
    stmt: Statement,
    params: Value[],
    insertHolder: InsertCacheHolder | null,
  ): Steps<Outcome> {
    if (this.access === "ro") {
      if (this.engine.stmtIsWriteWithViews(stmt)) {
        throw engineError(
          "read_only_sql_transaction",
          "cannot execute a write statement against a read-only snapshot",
        );
      }
      return yield* this.engine.executeStmtParamsSteps(stmt, params, insertHolder);
    }
    if (stmt.kind === "begin") return this.beginBlock(stmt.writable);
    if (stmt.kind === "commit") return yield* this.endBlockSteps(true);
    if (stmt.kind === "rollback") return yield* this.endBlockSteps(false);
    if (this.engine.session.tx !== null) {
      // Inside an open block (an eager write session, or this session after BEGIN): run on the
      // working set. The gate is already held for a writable block.
      return yield* this.engine.executeStmtParamsSteps(stmt, params, insertHolder);
    }
    if (!this.engine.stmtIsWriteWithViews(stmt)) {
      // Autocommit read: pin the latest committed for this one statement (PG-faithful); no gate.
      this.refreshCommitted();
      return yield* this.engine.executeStmtParamsSteps(stmt, params, insertHolder);
    }
    // Autocommit write — the lazy gate (§2.4): take it (throwing 25001 if another writer is open),
    // capture the latest committed as the working base, run, publish on success, release.
    this.acquireGate();
    try {
      this.refreshCommitted();
      const out = yield* this.engine.executeStmtParamsSteps(stmt, params, insertHolder);
      yield* this.publishSteps();
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
      const pinned = this.core.refreshAndPinAll();
      this.pinAtts = pinned.atts;
      this.refreshCommitted();
      this.pinnedVersion = this.baseVersion;
      this.pinGen = pinned.gen;
    }
    this.roleGen = this.core.roleGen;
    return this.engine.beginTx(writable);
  }

  // endBlock ends the open block (spec/design/session.md §2.4). commit: a clean writable block
  // publishes its working set at the next version; a failed/read-only block publishes nothing (a
  // failed COMMIT is a ROLLBACK, PostgreSQL). Either way the gate is released and any pin deregistered
  // — finishBlock runs in `finally` so a persist I/O failure (file-backed) never leaks the writer gate.
  private endBlock(commit: boolean): Outcome {
    return runSync(this.endBlockSteps(commit));
  }

  private *endBlockSteps(commit: boolean): Steps<Outcome> {
    try {
      if (commit && this.engine.session.tx !== null) {
        try {
          this.checkRole();
        } catch (error) {
          // The block began under a page-service role that has since changed: COMMIT fails and ends it.
          this.engine.rollbackTx();
          throw error;
        }
      }
      if (commit) {
        const failed = this.engine.session.tx?.failed ?? false;
        const out = yield* this.engine.commitTxSteps(); // inner in-memory swap: committed := working
        if (!failed && this.gateHeld)
          yield* this.publishSteps(); // persist + publish; may throw on I/O failure
        else this.engine.stagedAttachments = [];
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
    this.core.acquireWriter(this.engine.session.lockTimeoutMs);
    this.gateHeld = true;
  }

  // releaseGate releases the single-writer flag (if held).
  private releaseGate(): void {
    if (this.gateHeld) {
      this.core.releaseWriter();
      this.gateHeld = false;
    }
  }

  // finishBlock releases the writer flag (if held) and deregisters the watermark pin (if registered)
  // — the shared-core bookkeeping common to ending a block, closing, and an un-ended session.
  private finishBlock(): void {
    this.releaseGate();
    if (this.pinnedVersion !== null) {
      // Main's pin first, then the attachments' in the order they were taken.
      const atts = this.pinAtts;
      this.pinAtts = [];
      this.core.deregister(this.pinnedVersion, this.pinGen);
      this.pinnedVersion = null;
      releaseAttachPins(atts);
    }
  }

  // refreshCommitted re-pins the latest committed root as this session's base (spec/design/session.md
  // §2.4): the autocommit read/write path always works against the newest committed state. The
  // attached roots are pinned together (§5), with the file attachments under the page service
  // overlaid: the writer's view when this session holds the gate, else the reader's pins.
  private refreshCommitted(): void {
    this.baseVersion = this.core.committed.txid;
    this.engine.committed = this.core.committed;
    this.engine.attachedCommitted = this.gateHeld
      ? this.core.writerView()
      : overlayAttachPins(this.core.attached, this.stmtAtts ?? this.pinAtts);
  }

  // publish stores the engine's committed root into the shared cell at the next version (the §3 commit
  // window — transactions.md §2). Called after a clean autocommit write or an explicit COMMIT of a
  // writable block.
  //
  // File-backed: the new file snapshot is persisted durably first (core.persist) and the cell is
  // updated only on success, so a persist I/O failure throws and leaves the shared committed state (and
  // this session's version) unchanged. In-memory persist is a no-op.
  private publish(): void {
    runSync(this.publishSteps());
  }

  // Engine.commitTx already passed the last safe cancellation point. A shared-core publication follows
  // its in-memory swap and must finish the same commit transition (ts-runtime.md §11.2).
  private *publishSteps(): Steps<void> {
    yield* uncancelableSteps(this.publishUncancelableSteps());
  }

  private *publishUncancelableSteps(): Steps<void> {
    // Taken first, so any failure below drops the staged attachment commits unadopted.
    const staged = this.engine.stagedAttachments;
    this.engine.stagedAttachments = [];
    // Only changed domains publish (transactions.md §2.1). Consume the changes the preceding commit
    // reported, so no later publish can reuse them; an unchanged commit writes nothing and keeps main's
    // version.
    const changes = this.engine.commitChanges;
    this.engine.commitChanges = { main: false, attached: false };
    if (!changes.main && !changes.attached) return;
    this.core.checkPid();
    const snap = this.engine.committed;
    if (changes.main) {
      snap.txid = this.baseVersion + 1n; // main-domain version and reclamation watermark
      // durable before publish (packs into the byte store, any host)
      yield* this.core.persistSteps(snap, this.engine.commitStagesRows);
      if (this.core.writerRoleChanged()) {
        // A lost acknowledgement resolved as committed under a new role (README §6.5): that role's
        // committed state already holds the commit, so this copy of it is not published.
        this.engine.committed = this.core.committed;
        this.baseVersion = this.core.committed.txid;
        return;
      }
      if (afterPersistHook !== null) {
        // The persist→publish window (test seam; §8 fallback-reader race point). core.committed is still the
        // PRIOR published root here — a reader pinned inside the hook gets that fallback version.
        afterPersistHook(this.core.committed.txid, this.core.storage.freeGenTxid);
      }
      // The post-commit residency flip (bplus-reshape.md B4): the persist above assigned page ids to
      // every dirty node it wrote, so the committed tree can shed its leaf payloads — clean leaves
      // demote to OnDisk references faulted back through the pool on next touch. The session's own
      // committed base IS this snapshot (one object, single-threaded), so a long-lived writer sheds
      // residency too (read-your-writes for the NEXT statement re-faults — one read path).
      yield* snap.demoteCleanLeavesSteps();
      this.engine.committed = snap;
      this.baseVersion += 1n;
    }
    // A page client's reader may already have adopted a newer version through begin_read; never move
    // the root back.
    if (this.core.client === null || snap.txid > this.core.committed.txid) {
      this.core.committed = snap;
    }
    if (changes.main && this.core.service !== null) {
      // The page service publishes the same version with its meta and written pages (README.md §4).
      this.core.publishService(snap.txid);
    }
    if (changes.main && this.core.client !== null && this.core.storage.lastCommitMeta !== null) {
      // A page client adopts its own commit: the owner published exactly this version, and no other
      // commit came between its base and it (README.md §6.4).
      this.core.client.adoptOwnCommit(snap.txid, this.core.storage.lastCommitMeta);
    }
    // The N-root commit (attached-databases.md §5): publish the new attached roots the commit adopted
    // (commitTx already staged each dirtied attachment's working root into its in-RAM store and adopted
    // it into engine.attachedCommitted) together with the new main root, so a reader pins a consistent
    // cross-database snapshot, then adopt the staged attachment storage accounting. An unchanged
    // attachment carries its prior root through; an empty map (nothing attached) is the pre-attachment
    // single-root publish.
    this.core.attached = this.engine.attachedCommitted;
    this.core.adoptAttachments(staged, this.engine.attachedCommitted);
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
    runSync(this.commitSteps());
  }

  // commitSteps is commit as a continuation: the worker awaits an asynchronous storage stack.
  *commitSteps(): Steps<void> {
    if (this.engine.session.tx !== null) yield* this.endBlockSteps(true);
  }

  // rollback rolls back an open write block / write session (discard the working set + release the
  // gate, §2.4). With no open block this is a no-op.
  rollback(): void {
    if (this.engine.session.tx !== null) this.endBlock(false);
  }

  // close closes the session (spec/design/session.md §2.3): roll back any open block, deregister its
  // snapshot pin (advancing the watermark), and drop its session-local temp domain — closing a spilled
  // domain's scratch file (temp-tables.md §6). Idempotent; the caller must call it (no destructor in
  // JS), idiomatically in a finally.
  close(): void {
    if (this.closed) return;
    this.closed = true;
    try {
      if (this.engine.session.tx !== null) this.endBlock(false);
      else this.finishBlock();
    } finally {
      this.engine.releaseTempStorage();
    }
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
    return runSync(this.executeScriptSteps(sql));
  }

  *executeScriptSteps(sql: string): Steps<ScriptSummary> {
    const ownsWrapper = this.engine.session.tx === null;
    if (!ownsWrapper) this.checkRole();
    if (ownsWrapper) this.beginBlock(true);
    try {
      const summary = yield* this.engine.runScriptBodySteps(sql);
      if (ownsWrapper) yield* this.endBlockSteps(true);
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

  get lockTimeoutMs(): number {
    return this.engine.session.lockTimeoutMs;
  }
  set lockTimeoutMs(milliseconds: number) {
    this.engine.session.lockTimeoutMs = milliseconds;
  }
  setMaxSqlLength(bytes: number): void {
    this.engine.session.maxSqlLength = bytes;
  }
  get workMem(): number {
    return this.engine.session.workMem;
  }
  setMaxScalarBytes(bytes: bigint): void {
    this.engine.session.maxScalarBytes = bytes;
  }
  get maxScalarBytes(): bigint {
    return this.engine.session.scalarLimit();
  }
  // setMaxQueryMemoryBytes sets the live query-memory budget per statement (spec/design/memory.md §2);
  // non-positive restores the default, unlimited. Over-budget fails 54P05. A cursor already open keeps
  // the budget it opened with.
  setMaxQueryMemoryBytes(bytes: bigint): void {
    this.engine.session.setMaxQueryMemoryBytes(bytes);
  }
  // maxQueryMemoryBytes is the live query-memory budget, or 0n for unlimited.
  get maxQueryMemoryBytes(): bigint {
    return this.engine.session.queryMemoryLimitSetting();
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
  resetPrivileges(): void {
    this.engine.resetPrivileges();
  }
  setAllowTempDdl(allow: boolean): void {
    this.engine.setAllowTempDdl(allow);
  }
  // setTempBuffers sets the per-session temporary-table resident budget in bytes; 0 ⇒ never spill
  // (temp-tables.md §6).
  setTempBuffers(bytes: number): void {
    this.engine.setTempBuffers(bytes);
  }
  // setMaxTempStorageBytes sets the per-session temporary-table storage ceiling in bytes; 0 ⇒ unlimited
  // (temp-tables.md §7). An over-ceiling temp write aborts 54P03.
  setMaxTempStorageBytes(bytes: number): void {
    this.engine.setMaxTempStorageBytes(bytes);
  }
  setVar(name: string, value: string): void {
    this.engine.session.setVar(name, value);
  }
  resetVar(name: string): void {
    this.engine.session.resetVar(name);
  }
  resetVars(): void {
    this.engine.session.resetVars();
  }
  var(name: string): string | undefined {
    return this.engine.session.var(name);
  }
  setTimeZone(zone: string): void {
    this.engine.session.setTimeZone(zone);
  }
  // render renders v as PostgreSQL shows a result column (spec/design/timezones.md §9.5): exactly the
  // canonical render, except every timestamptz — at any depth inside an array, composite, or range —
  // renders in this session's CURRENT time zone (`2024-01-15 07:00:00-05`). Host-side and unmetered.
  // Throws 22008 only for a finite instant within an offset of jed's i64 extremes, outside
  // PostgreSQL's range. The value.ts render stays the zone-free (UTC) canonical form.
  render(v: Value): string {
    return renderIn(v, this.engine.session.timeZone);
  }
  // environment is the Environment of the database this session belongs to
  // (spec/design/environment.md §6).
  environment(): Environment {
    return this.engine.environment;
  }

  // --- Catalog / storage introspection (spec/design/api.md §6): reads this session's engine (its
  // working set inside a block, else its last-pinned committed snapshot). Not the embedding API —
  // hosts introspect through SQL (the jed_ catalog relations, introspection.md); these return the
  // doc-hidden detail white-box tests + the in-repo tools reach for. ---

  table(name: string): Table | undefined {
    return this.engine.table(name);
  }
  compositeType(name: string): CompositeType | undefined {
    return this.engine.compositeType(name);
  }
  // viewsInDependencyOrder is every view of the main database this session sees, in dependency order
  // — a view after every view it reads, ties in name byte order (spec/design/views.md §11, the CLI
  // dump order). SQL's jed_views is the introspection surface.
  viewsInDependencyOrder(): ViewDef[] {
    return this.engine.viewsInDependencyOrder();
  }
  rowsInKeyOrder(name: string): Row[] {
    return this.engine.rowsInKeyOrder(name);
  }
  // toImage serializes this session's committed state to a from-scratch on-disk image.
  toImage(pageSize: number, txid: bigint): Uint8Array {
    return toImageBytes(this.engine, pageSize, txid);
  }
  get txid(): bigint {
    return this.engine.committed.txid;
  }
  get pageSize(): number {
    return this.engine.pageSize;
  }
  get pageCount(): number {
    return this.engine.pageCount;
  }
  get path(): string | null {
    return this.engine.path;
  }
  get readOnly(): boolean {
    return this.access === "ro";
  }
  collations(): CollationInfo[] {
    return this.engine.collations();
  }
  loadedCollations(): CollationInfo[] {
    return this.engine.loadedCollations();
  }
  loadedTimeZones(): TimeZoneInfo[] {
    return this.engine.loadedTimeZones();
  }
  defaultCollation(): string {
    return this.engine.defaultCollation();
  }
  // setDefaultCollation sets the per-database default collation. It COMMITS (gate + refresh + publish):
  // default_collation lives in the committed snapshot, so a bare set would be overwritten by the next
  // autocommit statement's re-pin — the same subtlety the Rust/Go cores hit.
  setDefaultCollation(name: string): void {
    if (this.access === "ro") {
      throw engineError(
        "read_only_sql_transaction",
        "cannot set the default collation on a read-only session",
      );
    }
    if (this.engine.session.tx !== null) {
      this.engine.setDefaultCollation(name);
      return;
    }
    this.acquireGate();
    try {
      this.refreshCommitted();
      this.engine.setDefaultCollation(name);
      // No commitTx ran: this commit stages no record version (memory.md §8.3).
      this.engine.commitStagesRows = false;
      this.publish();
    } finally {
      this.releaseGate();
    }
  }
  // upgradeCollations runs the COLLATION UPGRADE migration (collation.md §12), returning the number of
  // re-pinned collations. A migration write: commit it (gate + refresh + publish) like setDefaultCollation.
  upgradeCollations(): number {
    if (this.access === "ro") {
      throw engineError(
        "read_only_sql_transaction",
        "cannot upgrade collations on a read-only session",
      );
    }
    if (this.engine.session.tx !== null) return this.engine.upgradeCollations();
    this.acquireGate();
    try {
      this.refreshCommitted();
      const n = this.engine.upgradeCollations();
      // Nothing was skewed ⇒ no state change, so there is no new version to publish (mirrors the
      // engine, which only swaps in the rebuilt snapshot when n > 0, and the Rust/Go sessions).
      if (n > 0) {
        // No commitTx ran: the rebuilt index entries are this commit's staged writes. They decide the
        // repair exemption (memory.md §8.3) and must not stay pending in the published snapshot (§7).
        const committed = this.engine.committed;
        this.engine.commitStagesRows = committed.stagedBytes() !== 0;
        committed.clearStaged();
        this.publish();
      }
      return n;
    } finally {
      this.releaseGate();
    }
  }
}
