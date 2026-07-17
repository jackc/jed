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

import {
  type Attachment,
  type CollationInfo,
  Engine,
  SessionState,
  Snapshot,
  stmtIsWrite,
  type Outcome,
  type InsertCacheHolder,
  type ScanCacheHolder,
  type SessionOptions,
  type TxStatus,
  type FileCoordinatorHost,
} from "./executor.ts";
import { PreparedStatement, Rows, rowsFromOutcome, Transaction } from "./api.ts";
import {
  loadEngine,
  loadEnginePaged,
  newAttachedStorage,
  toImage as toImageBytes,
} from "./format.ts";
import type { CompositeType, Table } from "./catalog.ts";
import type { ExtensionRegistry } from "./extension.ts";
import type { Row } from "./storage.ts";
import type { Cursor } from "./cursor.ts";
import { throwIfAborted } from "./cancel.ts";
import {
  drainRun,
  type JsParam,
  type Row as ErgoRow,
  type RunResult,
  Statement as ErgoStatement,
} from "./ergonomic.ts";
import { engineError } from "./errors.ts";
import { persistImpl, persistSharedBody } from "./persist.ts";
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
import type { ClockFunc, RandomFill } from "./seam.ts";
import type { Value } from "./value.ts";

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
  const db = new Engine();
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
  // (spec/design/attached-databases.md §5), keyed by lowercased name. A minted session captures this map
  // (with the committed root) so it sees a CONSISTENT cross-database snapshot; attach/detach REPLACE it
  // (never mutate in place) so a pinned reader is unaffected. A publish swaps the committing session's
  // adopted attached view in. Empty when nothing is attached — byte-for-byte the pre-attachment behavior.
  // Session-local `temp` is NOT here (it is session-private, on SessionState.tempCommitted).
  attached = new Map<string, Snapshot>();
  // attachments is the core-owned registry of host-attached databases (attached-databases.md §2/§5),
  // keyed by lowercased name — each attachment's MUTABLE storage identity (a MemoryBlockStore Engine,
  // like the temp domain) + its write mode. The immutable published root lives in `attached` under the
  // same key. Populated by Database.attach / cleared by Database.detach. Read by the executor via
  // Engine.core (the structural AttachmentCore view) during a commit persist.
  attachments = new Map<string, Attachment>();
  // live maps a pinned snapshot version to its reader refcount; its minimum key is the reclamation
  // watermark (several readers may pin the same version).
  live = new Map<bigint, number>();
  // writerActive is true while a write transaction is open (at most one — CLAUDE.md §3).
  writerActive = false;
  private heldWriterCoordinators: FileCoordinatorHost[] = [];
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
  planEpoch = 0;
  // The frozen host extension registry (spec/design/extensibility.md §7): the scalar functions the
  // host supplied in the create/open options, shared (by reference) into every minted session's
  // SessionState. null when no extensions were supplied.
  extensions: ExtensionRegistry | null = null;

  constructor(snap: Snapshot, storage: Engine, coordinator: FileCoordinatorHost | null = null) {
    this.committed = snap;
    this.storage = storage;
    this.coordinator = coordinator;
  }

  acquireWriter(timeoutMs: number): void {
    if (this.writerActive) {
      throw engineError("active_sql_transaction", "there is already a writer in progress");
    }
    this.writerActive = true;
    if (this.attachments.size === 0) {
      const coordinator = this.coordinator;
      if (coordinator === null) return;
      try {
        coordinator.checkPid();
        if (coordinator.state === "alone" || coordinator.state === "exclusive") return;
        if (coordinator.state === "poisoned") {
          throw engineError("io_error", "shared-file coordinator is poisoned");
        }
      } catch (error) {
        this.writerActive = false;
        throw error;
      }
    }
    const held: FileCoordinatorHost[] = [];
    try {
      for (const coordinator of this.writableCoordinators()) {
        coordinator.checkPid();
        if (coordinator.state === "poisoned") {
          throw engineError("io_error", "shared-file coordinator is poisoned");
        }
        if (coordinator.state === "shared") {
          coordinator.lockWriter(timeoutMs);
          held.push(coordinator);
        }
      }
      this.refreshShared();
      this.heldWriterCoordinators = held;
    } catch (error) {
      for (const coordinator of held.reverse()) coordinator.unlockWriter();
      this.writerActive = false;
      throw error;
    }
  }

  releaseWriter(): void {
    if (!this.writerActive) return;
    if (this.heldWriterCoordinators.length !== 0) {
      for (const coordinator of this.heldWriterCoordinators.reverse()) coordinator.unlockWriter();
      this.heldWriterCoordinators.length = 0;
    }
    this.writerActive = false;
  }

  private writableCoordinators(): FileCoordinatorHost[] {
    const coordinators: FileCoordinatorHost[] = [];
    if (this.coordinator !== null) coordinators.push(this.coordinator);
    for (const attachment of this.attachments.values()) {
      if (!attachment.readOnly && attachment.coordinator !== null) {
        coordinators.push(attachment.coordinator);
      }
    }
    coordinators.sort((a, b) => compareFilePaths(a.path, b.path));
    return coordinators;
  }

  hasSharedCoordinator(): boolean {
    if (this.coordinator?.state === "shared") return true;
    for (const attachment of this.attachments.values()) {
      if (attachment.coordinator?.state === "shared") return true;
    }
    return false;
  }

  checkPid(): void {
    this.coordinator?.checkPid();
    for (const attachment of this.attachments.values()) attachment.coordinator?.checkPid();
  }

  refreshShared(): void {
    if (!this.hasSharedCoordinator()) return;
    const coordinator = this.coordinator;
    if (coordinator?.state === "shared") {
      coordinator.checkPid();
      coordinator.lockCommitShared();
      try {
        this.reloadFromPager();
      } finally {
        coordinator.unlockCommit();
      }
    }
    const attachments = [...this.attachments.entries()]
      .filter((entry): entry is [string, Attachment & { coordinator: FileCoordinatorHost }] =>
        Boolean(entry[1].coordinator),
      )
      .sort((a, b) => compareFilePaths(a[1].coordinator.path, b[1].coordinator.path));
    for (const [name, attachment] of attachments) {
      const attachedCoordinator = attachment.coordinator;
      if (attachedCoordinator.state !== "shared") continue;
      attachedCoordinator.checkPid();
      attachedCoordinator.lockCommitShared();
      try {
        this.reloadAttachmentFromPager(name, attachment);
      } finally {
        attachedCoordinator.unlockCommit();
      }
    }
  }

  private reloadFromPager(): void {
    const paging = this.storage.paging;
    if (paging === null) return;
    paging.refreshAllocatedPages();
    const loaded = loadEnginePaged(paging);
    if (loaded.committed.txid <= this.committed.txid) return;
    this.storage.pageCount = loaded.pageCount;
    this.storage.freePages = loaded.freePages;
    this.storage.liveAtCompaction = loaded.liveAtCompaction;
    this.storage.freeGenTxid = loaded.freeGenTxid;
    this.committed = loaded.committed;
    this.planEpoch++;
  }

  private reloadAttachmentFromPager(name: string, attachment: Attachment): void {
    const paging = attachment.storage.paging;
    if (paging === null) return;
    paging.refreshAllocatedPages();
    const loaded = loadEnginePaged(paging);
    const current = this.attached.get(name);
    if (current !== undefined && loaded.committed.txid <= current.txid) return;
    attachment.storage.pageCount = loaded.pageCount;
    attachment.storage.freePages = loaded.freePages;
    attachment.storage.liveAtCompaction = loaded.liveAtCompaction;
    attachment.storage.freeGenTxid = loaded.freeGenTxid;
    const attached = new Map(this.attached);
    attached.set(name, loaded.committed);
    this.attached = attached;
    this.planEpoch++;
  }

  coordinationTick(): void {
    const coordinator = this.coordinator;
    if (coordinator === null || this.writerActive) return;
    try {
      if (coordinator.state === "alone") this.tryDowngrade(coordinator);
      else if (coordinator.state === "shared") this.tryUpgrade(coordinator, null);
    } catch {
      coordinator.state = "poisoned";
    }
  }

  coordinationTickAttachment(name: string, coordinator: FileCoordinatorHost): void {
    if (this.writerActive) return;
    try {
      if (coordinator.state === "alone") this.tryDowngrade(coordinator);
      else if (coordinator.state === "shared") this.tryUpgrade(coordinator, name);
    } catch {
      coordinator.state = "poisoned";
    }
  }

  private tryDowngrade(coordinator: FileCoordinatorHost): void {
    if (coordinator.tryArrivalExclusive()) {
      coordinator.unlockArrival();
      return;
    }
    coordinator.lockTransition();
    try {
      if (coordinator.tryArrivalExclusive()) {
        coordinator.unlockArrival();
        return;
      }
      coordinator.downgradePresence();
    } finally {
      coordinator.unlockTransition();
    }
  }

  private tryUpgrade(coordinator: FileCoordinatorHost, attachment: string | null): void {
    coordinator.lockTransition();
    try {
      if (!coordinator.tryArrivalExclusive()) return;
      try {
        if (!coordinator.tryUpgradePresence()) return;
        coordinator.lockCommitShared();
        try {
          if (attachment === null) this.reloadFromPager();
          else {
            const attached = this.attachments.get(attachment);
            if (attached !== undefined) this.reloadAttachmentFromPager(attachment, attached);
          }
        } finally {
          coordinator.unlockCommit();
        }
        coordinator.state = "alone";
      } finally {
        coordinator.unlockArrival();
      }
    } finally {
      coordinator.unlockTransition();
    }
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
  persist(snap: Snapshot): void {
    // The reader-liveness watermark (transactions.md §8) gates two things at the main-domain commit:
    //   - canReclaim (oldest_live == the new version, i.e. no reader live at an older version) lets the
    //     periodic COMPACT recompute the free-list (freeing this commit's fresh orphans);
    //   - canReuse (oldest_live ≥ the free-list's generation) lets this commit REUSE the free-list — a
    //     page dead at generation G is only safe to overwrite once no reader pins a version older than G.
    // Both consult the SAME registry; single-handle / all-readers-current keeps oldest_live == committed,
    // so both hold and behavior (and on-disk bytes) are identical to an ungated commit.
    const oldest = this.oldestLiveVersion(snap.txid);
    const coordinator = this.coordinator;
    if (coordinator !== null && coordinator.state === "shared" && this.storage.path !== null) {
      try {
        const pending = persistSharedBody(this.storage, snap);
        coordinator.lockCommitExclusive();
        try {
          pending.publishMeta();
        } finally {
          coordinator.unlockCommit();
        }
      } catch (error) {
        coordinator.state = "poisoned";
        throw error;
      }
      return;
    }
    persistImpl(this.storage, snap, oldest === snap.txid, oldest >= this.storage.freeGenTxid);
  }

  // hasLiveReaders reports whether any cross-session reader currently pins a committed snapshot (the
  // live registry, transactions.md §8) — the within-session compaction watermark for a host attachment
  // (attached-databases.md §5): the committing writer holds the writer flag but is not itself in `live`,
  // so an empty registry means no other session can observe a page the commit is about to reclaim.
  hasLiveReaders(): boolean {
    return this.live.size > 0;
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

  // oldestLiveVersion is the oldest still-pinned version, floored at newTxid (the version this commit
  // publishes) so "no live reader" reads as newTxid — the safe case for compaction (temp-tables.md §6).
  // Any live reader pins a version older than newTxid (it opened before this commit), so a non-empty
  // registry yields a value < newTxid and defers compaction. Distinct from oldest(), which floors at the
  // CURRENTLY-committed version; during a persist (before committed swaps) the two differ.
  oldestLiveVersion(newTxid: bigint): bigint {
    let oldest = newTxid;
    for (const v of this.live.keys()) if (v < oldest) oldest = v;
    return oldest;
  }
}

// AttachSource selects the backing for a database attached via Database.attach
// (spec/design/attached-databases.md §4). A MEMORY source is a fresh, empty in-memory database
// (Slice 1b); a FILE source opens an existing single-file jed database on disk (Slice 2). Build one with
// attachMemory() or attachFile(path).
export type AttachSource = {
  file: boolean;
  path?: string;
  locking?: "auto" | "shared" | "exclusive" | "none";
  fileLockTimeoutMs?: number;
};

// attachMemory returns a source for a fresh, empty in-memory attachment (attached-databases.md §6).
export function attachMemory(): AttachSource {
  return { file: false };
}

// attachFile returns a source for a file-backed attachment: an existing single-file jed database at path
// (attached-databases.md §4, Slice 2). The file's own page size is honored (each attachment is its own
// page space, §2). With readOnly=true it is opened read-only (as well as write-rejected, 25006);
// readOnly=false opens it read-write so DDL/DML can target it (subject to the one-durable-writer rule, §5).
export function attachFile(
  path: string,
  options: Pick<AttachSource, "locking" | "fileLockTimeoutMs"> = {},
): AttachSource {
  return { file: true, path, ...options };
}

// fileAttachOpener is the host-injected file opener for a file-backed attachment (attached-databases.md
// §4, Slice 2): the node host (file.ts) registers it at load, so Database.attach can open a file-backed
// attachment WITHOUT shared.ts importing a host module (keeping it browser-clean — the same reason
// file.ts, not shared.ts, owns open/create). Returns the opened storage Engine (its committed snapshot +
// paging become the attachment). null until a host registers one → a file attach then throws
// feature_not_supported (a pure in-memory build has no file layer to reach).
type OpenedAttachment = { engine: Engine; coordinator: FileCoordinatorHost | null };
let fileAttachOpener: ((source: AttachSource, readOnly: boolean) => OpenedAttachment) | null = null;

// registerFileAttachOpener installs the host file layer that Database.attach uses for a file source
// (called once, at file.ts module load). The OPFS host would register its own.
export function registerFileAttachOpener(
  fn: (source: AttachSource, readOnly: boolean) => OpenedAttachment,
): void {
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
  static fromEngine(
    engine: Engine,
    coordinator: FileCoordinatorHost | null = null,
    extensions: ExtensionRegistry | null = null,
  ): Database {
    // v25: the main domain (file or in-memory) reclaims within-session — the open path reads the
    // persisted free-list and no longer reconstructs it, so mid-session orphans must be returned at each
    // commit or they would leak permanently (format.md *Reclamation*).
    engine.reclaimWithinSession = true;
    const core = new SharedCore(engine.committed, engine, coordinator);
    core.readOnly = engine.readOnly;
    core.extensions = extensions; // frozen at open/create, shared into every session (§7)
    coordinator?.startProbe(() => core.coordinationTick());
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
  // read-only. The name is case-folded; it must not name a reserved database (`main` / `temp`) or one
  // already attached (42710). Opening a file surfaces the same host/file codes as opening `main`
  // (58P01/XX001/…). Publishing the new root replaces the attached map (a pinned reader keeps its old map).
  attach(name: string, source: AttachSource = attachMemory(), readOnly = false): void {
    const lname = name.toLowerCase();
    if (lname === "") {
      throw engineError("duplicate_object", "attachment name must not be empty");
    }
    const c = this.core;
    let storage: Engine;
    let root: Snapshot;
    let coordinator: FileCoordinatorHost | null = null;
    if (source.file) {
      if (fileAttachOpener === null) {
        // A pure in-memory build (no node/OPFS host imported) has no file layer to reach.
        throw engineError("feature_not_supported", "file attachment needs a host file layer");
      }
      // Open the file BEFORE the dup check (an open may throw 58P01/XX001); on a name conflict close it.
      const opened = fileAttachOpener(source, readOnly);
      const engine = opened.engine;
      coordinator = opened.coordinator;
      if (lname === "main" || lname === "temp" || c.attachments.has(lname)) {
        engine.paging?.close(); // release the just-opened file — the name is taken
        coordinator?.close();
        throw engineError("duplicate_object", `database "${name}" already exists`);
      }
      // v25: a file attachment persists + reclaims like the main file domain.
      engine.reclaimWithinSession = true;
      storage = engine; // its stores fault through engine.paging (bound at load); storePaging stays unset
      root = engine.committed;
    } else {
      if (lname === "main" || lname === "temp" || c.attachments.has(lname)) {
        throw engineError("duplicate_object", `database "${name}" already exists`);
      }
      storage = newAttachedStorage(c.pageSize);
      // The fresh attachment's committed root: an empty snapshot whose NEW stores attach to its own paging
      // (the same storePaging seam session-local temp uses — a snapshot's storePaging is "the paging new
      // stores bind to").
      const empty = new Snapshot(0n);
      empty.storePaging = storage.paging;
      root = empty;
    }
    try {
      c.acquireWriter(0);
    } catch (error) {
      storage.paging?.close();
      coordinator?.close();
      throw error;
    }
    try {
      // Publish a NEW attached map so a live reader's pinned map is unaffected.
      c.attachments.set(lname, { name: lname, readOnly, storage, coordinator });
      const na = new Map(c.attached);
      na.set(lname, root);
      c.attached = na;
    } finally {
      c.releaseWriter();
    }
    coordinator?.startProbe(() => c.coordinationTickAttachment(lname, coordinator!));
  }

  // detach removes a previously attached database (spec/design/attached-databases.md §4/§8). A host-API
  // act. It is 55006 (object_in_use) while any live reader session / cursor still pins a committed
  // snapshot (the reader-liveness watermark, §5 — a reader pins the whole roots, so an open reader pins
  // every attachment), and 42704 if no database of that name is attached (`main` / `temp` are not
  // detachable). On success the attachment's root is dropped from the published roots and its storage
  // released.
  detach(name: string): void {
    const lname = name.toLowerCase();
    const c = this.core;
    c.acquireWriter(0);
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
      const na = new Map(c.attached);
      na.delete(lname);
      c.attached = na;
    } finally {
      c.releaseWriter();
    }
    // Release a file attachment's OS handle once it is unpublished and unreferenced (a no-op for an
    // in-memory attachment). No live reader can still fault it — detach-in-use was rejected above.
    att.storage.paging?.close();
    att.coordinator?.close();
  }

  // readSession opens a READ ONLY session over a consistent snapshot (spec/design/session.md §2.4,
  // transactions.md §10). Pins the committed snapshot now and registers its version in the live set;
  // the session serves reads from that snapshot for its life — observing one stable version even as a
  // writer interleaves commits — and a write through it is 25006. The caller must close() it to
  // deregister (advancing the watermark), since JS has no destructor. (The old SharedDb.read().)
  readSession(): Session {
    this.core.checkPid();
    this.core.refreshShared();
    const snap = this.core.committed; // pin (immutable — no clone)
    this.core.register(snap.txid);
    const engine = databaseFromSnapshot(snap, this.core);
    // The attached roots are pinned together with the committed root (attached-databases.md §5), so the
    // session sees a consistent cross-database snapshot; it routes attachment persists via the core.
    engine.core = this.core;
    engine.session.extensions = this.core.extensions; // the frozen host registry (extensibility.md §7)
    engine.attachedCommitted = this.core.attached;
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
    this.core.acquireWriter(0);
    try {
      const base = this.core.committed;
      // committed = the immutable base; beginTx clones it to working.
      const engine = databaseFromSnapshot(base, this.core);
      engine.core = this.core;
      engine.session.extensions = this.core.extensions; // the frozen host registry (extensibility.md §7)
      engine.attachedCommitted = this.core.attached; // pin the attached roots together (§5)
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
    if (this.core.readOnly) this.core.refreshShared();
    const snap = this.core.committed;
    const engine = databaseFromSnapshot(snap, this.core);
    engine.session = new SessionState(opts);
    engine.core = this.core;
    engine.session.extensions = this.core.extensions; // the frozen host registry (extensibility.md §7)
    engine.attachedCommitted = this.core.attached; // pin the attached roots together (§5)
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
    const st = this.core.storage;
    if (st.paging !== null) {
      st.paging.close();
      st.paging = null;
    }
    // Release any still-attached file databases (an in-memory attachment's close is a no-op), so the host
    // need not detach before close (attached-databases.md §4). Order-independent (just closing).
    for (const att of this.core.attachments.values()) {
      att.storage.paging?.close();
      att.coordinator?.close();
    }
    this.core.attachments = new Map();
    this.core.coordinator?.close();
    this.core.coordinator = null;
  }

  // fromImage lifts a from-scratch on-disk image (spec/fileformat/format.md) into an in-memory host
  // handle — the inverse of toImage, used by the byte-level golden round-trip tests and by a host
  // rehydrating an in-memory database from bytes. Since B3 the image becomes the core's
  // MemoryBlockStore, demand-paged like a file (one read path); there is no backing file (path stays
  // null) and a commit packs pages into the memory store.
  static fromImage(image: Uint8Array): Database {
    return Database.fromEngine(loadEngine(image));
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
    return this.core.committed.txid;
  }
  // pageSize is the page payload size this database serializes at.
  get pageSize(): number {
    return this.core.pageSize;
  }
  // pageCount is the byte store's page high-water (since B3 an in-memory database has a real one —
  // its MemoryBlockStore's committed page count).
  get pageCount(): number {
    return this.core.storage.pageCount;
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
    return this.committedEngine().loadedCollations();
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
export function buildInMemory(
  pageSize: number,
  extensions: ExtensionRegistry | null = null,
): Database {
  const image = toImageBytes(new Snapshot(0n), pageSize, 0n);
  return Database.fromEngine(loadEngine(image), null, extensions);
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
    this.planEpoch = core.planEpoch;
  }

  // execute runs a (possibly mutating) statement on this session, binding $N params (spec/design/
  // api.md §5). Routes by the session's state (read-only / open block / autocommit) with the
  // lazy-gate lifecycle (§2.4).
  execute(sql: string, params: Value[] = []): RunResult {
    return drainRun(this.query(sql, params));
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
    return this.queryStmt(this.engine.parse(sql), params, null, null); // one-shot: no cross-call plan cache (still plans once)
  }

  // queryStmt routes an already-parsed query AST through the session's lazy lanes — the autocommit
  // re-pin, the plan-once scan (streaming/buffered) then deferred cursors, and the reader-liveness
  // watermark pin — falling back to the materialized dispatch for a shape no lazy lane covers (a
  // write, a data-modifying WITH). Shared by query (parse-then-route, holder null) and a prepared
  // query (queryPrepared passes the statement's ScanCacheHolder), so a prepared query streams and
  // pins its snapshot exactly like an ad-hoc one but reuses its cached plan across executes.
  private queryStmt(
    stmt: Statement,
    params: Value[],
    holder: ScanCacheHolder | null,
    insertHolder: InsertCacheHolder | null,
  ): Rows {
    // Route the read before building the lazy cursor: an autocommit (non-block, writable access) read
    // re-pins the latest committed so the snapshot is current; a read-only session uses its existing
    // pin, and an open block uses its working set.
    if (this.access !== "ro" && this.engine.session.tx === null && !stmtIsWrite(stmt)) {
      this.core.refreshShared();
      this.refreshCommitted();
    }
    if (this.planEpoch !== this.core.planEpoch) {
      if (holder !== null) holder.cache = null;
      if (insertHolder !== null) insertHolder.cache = null;
      this.planEpoch = this.core.planEpoch;
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
      !stmtIsWrite(stmt);
    // pin registers the cursor's snapshot version in the watermark (streaming.md §5); the deregister
    // runs on cursor close (JS has no destructor), advancing oldestLiveTxid.
    const pin = (built: { columnNames: string[]; columnTypes: string[]; cursor: Cursor }): Rows => {
      const version = this.baseVersion;
      this.core.register(version);
      // A live streaming cursor also blocks within-session temp compaction: it faults its pinned temp
      // tree lazily, so a temp commit must not reclaim a page it may still read (temp-tables.md §6). The
      // counter is on the session's engine (single-threaded), like the write path it gates.
      this.engine.openStreams++;
      const rows = new Rows(built.columnNames, built.columnTypes, built.cursor, null);
      rows.attachPin(() => {
        this.engine.openStreams--;
        this.core.deregister(version);
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
      const scanned = this.engine.tryScanQuery(stmt, params, holder);
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
    return rowsFromOutcome(this.dispatch(stmt, params, insertHolder));
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

  // executePrepared runs a prepared statement on this session, binding $N params, and returns its
  // command tag — the prepared analogue of execute (spec/design/api.md §2.4).
  executePrepared(stmt: PreparedStatement, params: Value[] = []): RunResult {
    return drainRun(this.queryPrepared(stmt, params));
  }

  // queryPrepared runs a prepared query on this session (its pinned snapshot, privileges, temp
  // domain), returning a row cursor. The prepared AST routes through the same lazy lanes as the
  // ad-hoc query — so a prepared query streams and pins its snapshot identically — but reuses its
  // cached plan across executes (and across sessions of one database, spec/design/api.md §2.4).
  queryPrepared(stmt: PreparedStatement, params: Value[] = []): Rows {
    return this.queryStmt(stmt.ast, params, stmt.scHolder, stmt.icHolder);
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

  private dispatch(
    stmt: Statement,
    params: Value[],
    insertHolder: InsertCacheHolder | null,
  ): Outcome {
    if (this.access === "ro") {
      if (stmtIsWrite(stmt)) {
        throw engineError(
          "read_only_sql_transaction",
          "cannot execute a write statement against a read-only snapshot",
        );
      }
      return this.engine.executeStmtParams(stmt, params, insertHolder);
    }
    if (stmt.kind === "begin") return this.beginBlock(stmt.writable);
    if (stmt.kind === "commit") return this.endBlock(true);
    if (stmt.kind === "rollback") return this.endBlock(false);
    if (this.engine.session.tx !== null) {
      // Inside an open block (an eager write session, or this session after BEGIN): run on the
      // working set. The gate is already held for a writable block.
      return this.engine.executeStmtParams(stmt, params, insertHolder);
    }
    if (!stmtIsWrite(stmt)) {
      // Autocommit read: pin the latest committed for this one statement (PG-faithful); no gate.
      this.refreshCommitted();
      return this.engine.executeStmtParams(stmt, params, insertHolder);
    }
    // Autocommit write — the lazy gate (§2.4): take it (throwing 25001 if another writer is open),
    // capture the latest committed as the working base, run, publish on success, release.
    this.acquireGate();
    try {
      this.refreshCommitted();
      const out = this.engine.executeStmtParams(stmt, params, insertHolder);
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
      this.core.refreshShared();
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
      this.core.deregister(this.pinnedVersion);
      this.pinnedVersion = null;
    }
  }

  // refreshCommitted re-pins the latest committed root as this session's base (spec/design/session.md
  // §2.4): the autocommit read/write path always works against the newest committed state.
  private refreshCommitted(): void {
    this.baseVersion = this.core.committed.txid;
    this.engine.committed = this.core.committed;
    this.engine.attachedCommitted = this.core.attached; // pin the latest attached roots together (§5)
  }

  // publish stores the engine's committed root into the shared cell at the next version (the §3 commit
  // window — transactions.md §2). Called after a clean autocommit write or an explicit COMMIT of a
  // writable block.
  //
  // File-backed: the new file snapshot is persisted durably first (core.persist) and the cell is
  // updated only on success, so a persist I/O failure throws and leaves the shared committed state (and
  // this session's version) unchanged. In-memory persist is a no-op.
  private publish(): void {
    this.core.checkPid();
    const snap = this.engine.committed;
    snap.txid = this.baseVersion + 1n; // advance the shared version on every commit
    this.core.persist(snap); // durable before publish (packs into the byte store, any host)
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
    snap.demoteCleanLeaves();
    this.engine.committed = snap;
    this.core.committed = snap;
    // The N-root commit (attached-databases.md §5): publish the new attached roots the commit adopted
    // (commitTx already packed each dirtied attachment's working root into its in-RAM store and adopted
    // it into engine.attachedCommitted) together with the new main root, so a reader pins a consistent
    // cross-database snapshot. An unchanged attachment carries its prior root through; an empty map
    // (nothing attached) is the pre-attachment single-root publish.
    this.core.attached = this.engine.attachedCommitted;
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
  setTempBuffers(bytes: number): void {
    this.engine.setTempBuffers(bytes);
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
      this.publish();
      return n;
    } finally {
      this.releaseGate();
    }
  }
}
