// The serving owner's worker thread (spec/design/page-service.md §14, spec/pageservice/README.md §6.2,
// §6.4). One worker per process serves every database the process owns (owner.ts starts it once and
// reuses it across opens): for each it owns the endpoint and every client connection, reads pages
// through its own file handle, and commits client CommitDeltas through that handle — the owner process
// is still the only one touching the file — so a client request never waits for the engine thread's
// synchronous work. It shares only each database's ServiceState with the engine thread: pins, the
// published meta and windows, and the writer gate.
//
// The writer gate. A begin_write waits for the gate asynchronously (Atomics.waitAsync), so the worker
// keeps serving the connection's other requests and every other connection meanwhile; write_begun is
// sent when the wait ends, possibly out of request order (README §2). A client commit past its point of
// no return is written, synced, and published synchronously within one event-loop turn, so a
// commit_status (from any connection) never observes it half published. When a client commit leaves
// the high-water past the reclamation threshold, the worker hands the gate to the engine thread, which
// runs the maintenance commit and releases it (README §6.4; the worker has no engine).

import {
  closeSync,
  fdatasyncSync,
  fstatSync,
  ftruncateSync,
  openSync,
  readSync,
  writeSync,
} from "node:fs";
import { createServer, type Server, type Socket } from "node:net";
import { type MessagePort, workerData } from "node:worker_threads";

import "../crc32_node.ts";
import { COMPACT_GROWTH, COMPACT_MIN_PAGES } from "../costs.ts";
import { EngineError } from "../errors.ts";
import { signalAnswer } from "./blocking.ts";
import {
  encodeFrame,
  FrameAccumulator,
  PS_ERROR_FLAG_FATAL,
  PS_HELLO_FLAG_READ_ONLY,
  PS_HELLO_OK_FLAG_CONNECTION_READ_ONLY,
  PS_HELLO_OK_FLAG_OWNER_READ_ONLY,
  PS_INVALIDATE_ALL,
  PS_INVALIDATE_EXACT,
  PS_PROTOCOL_VERSION,
  psMaxReadPages,
  type PsBeginWrite,
  type PsCommitBegin,
  type PsCommitPages,
  type PsHello,
  type PsMessage,
} from "./codec.ts";
import { Delta, type DeltaBase, DeltaFailure, readFreeListChain } from "./delta.ts";
import { GATE_FREE, GATE_WORKER, ServiceState } from "./shared_state.ts";

// OwnerServe asks the worker to serve one owned database (owner.ts); OwnerStop stops serving it.
// notify is the engine thread's port for gate handoffs (a maintenance commit, README §6.4).
export type OwnerServe = {
  op: "serve";
  id: number;
  state: SharedArrayBuffer;
  dbPath: string;
  endpoint: string;
  pageSize: number;
  formatVersion: number;
  incarnation: Uint8Array;
  ownerReadOnly: boolean;
  noSync: boolean;
  idleTimeoutMs: number;
  notify: MessagePort;
};
export type OwnerStop = { op: "stop"; id: number };
// OwnerPoison makes the worker stop serving one database whose owner handle failed a commit.
export type OwnerPoison = { op: "poison"; id: number };

const { flag, port } = workerData as { flag: Int32Array; port: MessagePort };
let nextConnectionId = 0n;

// GATE_POLL_MS caps one asynchronous gate wait, so a waiter notices its connection closing.
const GATE_POLL_MS = 500;

// Failure is a protocol-level error answer: fatal ones close the connection.
class Failure extends Error {
  readonly sqlstate: string;
  readonly fatal: boolean;
  constructor(sqlstate: string, message: string, fatal: boolean) {
    super(message);
    this.sqlstate = sqlstate;
    this.fatal = fatal;
  }
}

function violation(message: string): Failure {
  return new Failure("08P01", message, true);
}

function idleRevoked(): Failure {
  return new Failure(
    "25P03",
    "terminating the write transaction due to the idle-in-transaction timeout",
    false,
  );
}

function sameId(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

function metaPageCount(meta: Uint8Array): number {
  return ((meta[24]! << 24) | (meta[25]! << 16) | (meta[26]! << 8) | meta[27]!) >>> 0;
}

function freeListHead(meta: Uint8Array): number {
  return ((meta[28]! << 24) | (meta[29]! << 16) | (meta[30]! << 8) | meta[31]!) >>> 0;
}

// Served is one database this worker serves: its shared state, file handle, endpoint, and
// connections.
class Served {
  readonly state: ServiceState;
  readonly dbPath: string;
  readonly pageSize: number;
  readonly maxReadPages: number;
  readonly formatVersion: number;
  readonly incarnation: Uint8Array;
  readonly ownerReadOnly: boolean;
  readonly noSync: boolean;
  readonly idleTimeoutMs: number;
  readonly notify: MessagePort;
  readonly connections = new Set<Connection>();
  readonly server: Server;
  // stopped is set once the owner stops serving (closed, or poisoned by a failed client commit).
  stopped = false;
  // The file handle and the file generation it was opened at; compaction bumps the generation and the
  // next access reopens the (renamed-into-place) file. Read-write unless the owner is read-only.
  private fd: number;
  private fdGeneration: number;

  constructor(m: OwnerServe) {
    this.state = new ServiceState(m.state);
    this.dbPath = m.dbPath;
    this.pageSize = m.pageSize;
    this.maxReadPages = psMaxReadPages(m.pageSize);
    this.formatVersion = m.formatVersion;
    this.incarnation = m.incarnation;
    this.ownerReadOnly = m.ownerReadOnly;
    this.noSync = m.noSync;
    this.idleTimeoutMs = m.idleTimeoutMs;
    this.notify = m.notify;
    this.fd = openSync(m.dbPath, this.ownerReadOnly ? "r" : "r+");
    this.fdGeneration = this.state.fileGeneration();
    this.server = createServer((socket) => {
      this.connections.add(new Connection(this, socket));
    });
  }

  private file(): number {
    const generation = this.state.fileGeneration();
    if (generation !== this.fdGeneration) {
      closeSync(this.fd);
      this.fd = openSync(this.dbPath, this.ownerReadOnly ? "r" : "r+");
      this.fdGeneration = generation;
    }
    return this.fd;
  }

  // readPage reads one page from the file at its index.
  readPage(index: number): Uint8Array {
    const fd = this.file();
    const ps = this.pageSize;
    const page = new Uint8Array(ps);
    let done = 0;
    while (done < ps) {
      const n = readSync(fd, page, done, ps - done, index * ps + done);
      if (n <= 0)
        throw new Failure("58030", `page service read: short read at page ${index}`, true);
      done += n;
    }
    return page;
  }

  // writePage writes one page of a client commit at its index.
  writePage(index: number, page: Uint8Array): void {
    const fd = this.file();
    let done = 0;
    while (done < page.length) {
      const n = writeSync(fd, page, done, page.length - done, index * this.pageSize + done);
      if (n <= 0) throw new Error("file write made no progress");
      done += n;
    }
  }

  // growTo extends the file to pageCount pages, so every page the new meta counts exists; the commit's
  // one durable barrier covers the new size.
  growTo(pageCount: number): void {
    const fd = this.file();
    const bytes = pageCount * this.pageSize;
    if (fstatSync(fd).size < bytes) ftruncateSync(fd, bytes);
  }

  // sync is the single durable barrier of a client commit (skipped under fsync=off).
  sync(): void {
    if (!this.noSync) fdatasyncSync(this.file());
  }

  // stopServing is the response to a write or sync failure in a client commit (README §3.5): the owner
  // is poisoned, every connection closes, and its own handle refuses commits until reopened.
  stopServing(): void {
    this.state.poison();
    this.stopped = true;
    this.server.close();
    for (const connection of this.connections) connection.destroy();
  }

  closeFile(): void {
    try {
      closeSync(this.fd);
    } catch {
      // Already closed.
    }
  }
}

// WriteTxn is a client write transaction: the connection holds the gate and a write pin on base.
type WriteTxn = {
  base: bigint;
  check: DeltaBase;
  // delta is the CommitDelta being received (between commit_begin and commit_end); failure is a
  // rejected delta, whose remaining frames are discarded and which commit_end reports.
  delta: Delta | null;
  failure: Error | null;
  // finishing is set at the point of no return: the commit completes whatever the connection does.
  finishing: boolean;
};

// Connection is one client connection: its frame reassembly, its pins (counted per txid, the write pin
// included; each is also one count in the shared pin table), and its write transaction.
class Connection {
  private readonly db: Served;
  private readonly socket: Socket;
  private readonly id: bigint;
  private readonly frames = new FrameAccumulator();
  private readonly pins = new Map<bigint, number>();
  private helloDone = false;
  private readOnly = false;
  private dead = false;
  private write: WriteTxn | null = null;
  // waiting is true while a begin_write waits for the gate.
  private waiting = false;
  // revoked is set when the idle-in-transaction timeout revoked the write transaction on revokedBase;
  // the next write-path request reports 25P03.
  private revoked = false;
  private revokedBase = 0n;
  private idle: ReturnType<typeof setTimeout> | null = null;

  constructor(db: Served, socket: Socket) {
    this.db = db;
    this.socket = socket;
    this.id = ++nextConnectionId;
    socket.on("data", (chunk: Buffer) => this.receive(chunk));
    socket.on("error", () => {});
    socket.on("close", () => {
      this.teardown();
      this.db.connections.delete(this);
    });
  }

  destroy(): void {
    this.socket.destroy();
  }

  // teardown ends the connection's state: its write transaction (unless past the point of no return)
  // and every pin.
  private teardown(): void {
    this.dead = true;
    if (this.write !== null && !this.write.finishing) this.abortWrite();
    this.releasePins();
  }

  // receive handles every complete frame: a non-fatal error answers its request and the next frame
  // is handled; a fatal one (or an undecodable frame) closes the connection.
  private receive(chunk: Buffer): void {
    if (this.dead) return;
    for (;;) {
      let frame: { requestId: number; message: PsMessage } | null;
      try {
        if (chunk.length > 0) {
          this.frames.push(new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength));
          chunk = Buffer.alloc(0);
        }
        frame = this.frames.next(this.db.pageSize);
      } catch (error) {
        this.fail(0, error);
        return;
      }
      if (frame === null) return;
      this.touch();
      try {
        const reply = this.handle(frame.requestId, frame.message);
        if (reply !== null) this.send(frame.requestId, reply);
      } catch (error) {
        this.fail(frame.requestId, error);
      }
      if (this.dead) return;
    }
  }

  private handle(requestId: number, m: PsMessage): PsMessage | null {
    if (!this.helloDone) {
      if (m.kind !== "hello") throw violation("the first frame must be hello");
      return this.hello(m);
    }
    switch (m.kind) {
      case "begin_read":
        return this.beginRead(m.incarnation, m.lastTxid);
      case "read_pages":
        return this.readPages(m.txid, m.indices);
      case "end_read":
        this.endRead(m.txid);
        return null;
      case "begin_write":
        this.beginWrite(requestId, m);
        return null;
      case "commit_begin":
        this.commitBegin(m);
        return null;
      case "commit_pages":
        this.commitPages(m);
        return null;
      case "commit_end":
        return this.commitEnd(requestId);
      case "abort":
        this.abort();
        return null;
      case "commit_status":
        return this.commitStatus(m.txid, m.digest);
      default:
        throw violation(`unexpected message ${m.kind}`);
    }
  }

  // hello negotiates the protocol and the format (README §3.1).
  private hello(m: PsHello): PsMessage {
    if (m.protocolMin > PS_PROTOCOL_VERSION || m.protocolMax < PS_PROTOCOL_VERSION) {
      throw new Failure(
        "0A000",
        "unsupported page-service protocol; the owner speaks version 1",
        true,
      );
    }
    if (m.formatVersion !== this.db.formatVersion) {
      throw new Failure(
        "0A000",
        `the client does not support this database's format_version; it requires format_version ${this.db.formatVersion}`,
        true,
      );
    }
    this.helloDone = true;
    let flags = 0;
    if ((m.flags & PS_HELLO_FLAG_READ_ONLY) !== 0) {
      this.readOnly = true;
      flags |= PS_HELLO_OK_FLAG_CONNECTION_READ_ONLY;
    }
    if (this.db.ownerReadOnly) flags |= PS_HELLO_OK_FLAG_OWNER_READ_ONLY;
    return {
      kind: "hello_ok",
      protocol: PS_PROTOCOL_VERSION,
      formatVersion: this.db.formatVersion,
      pageSize: this.db.pageSize,
      connectionId: this.id,
      flags,
      maxReadPages: this.db.maxReadPages,
    };
  }

  // pinCommittedLocked pins the committed snapshot for this connection. The caller holds the lock.
  private pinCommittedLocked(): bigint {
    const txid = this.db.state.committedTxidLocked();
    if (!this.db.state.pinLocked(txid)) {
      throw new Failure("54000", "too many pinned snapshots on this owner", true);
    }
    this.pins.set(txid, (this.pins.get(txid) ?? 0) + 1);
    return txid;
  }

  // beginRead pins the committed snapshot for this connection (README §3.2).
  private beginRead(incarnation: Uint8Array, lastTxid: bigint): PsMessage {
    this.db.state.lock();
    try {
      const txid = this.pinCommittedLocked();
      const inv = this.db.state.invalidationLocked(
        sameId(incarnation, this.db.incarnation),
        lastTxid,
        txid,
      );
      return {
        kind: "read_begun",
        incarnation: this.db.incarnation,
        txid,
        invalidate: inv.all ? PS_INVALIDATE_ALL : PS_INVALIDATE_EXACT,
        indices: inv.indices,
        meta: this.db.state.metaLocked(),
      };
    } finally {
      this.db.state.unlock();
    }
  }

  // readPages returns the requested pages (README §3.3).
  private readPages(txid: bigint, indices: number[]): PsMessage {
    if (!this.pins.has(txid)) {
      if (this.revoked && txid === this.revokedBase) {
        // A read under a revoked write transaction's snapshot: report the revocation.
        this.revoked = false;
        throw idleRevoked();
      }
      throw violation("read_pages names a txid this connection has not pinned");
    }
    if (indices.length === 0 || indices.length > this.db.maxReadPages) {
      throw violation("read_pages names too few or too many pages");
    }
    // The page count only grows between compactions, and a pin blocks compaction, so the current
    // count bounds every page of the pinned snapshot.
    this.db.state.lock();
    const pageCount = this.db.state.pageCountLocked();
    this.db.state.unlock();
    const pages: Uint8Array[] = [];
    for (const index of indices) {
      if (index < 2 || index >= pageCount) {
        throw violation("read_pages names a page outside the pinned snapshot");
      }
      pages.push(this.db.readPage(index));
    }
    return { kind: "pages", pages };
  }

  // endRead releases one pin (README §3.4).
  private endRead(txid: bigint): void {
    if (!this.pins.has(txid)) {
      throw violation("end_read names a txid this connection has not pinned");
    }
    this.unpin(txid);
  }

  // unpin drops one of this connection's pins on txid.
  private unpin(txid: bigint): void {
    const n = this.pins.get(txid) ?? 0;
    if (n <= 1) this.pins.delete(txid);
    else this.pins.set(txid, n - 1);
    this.db.state.lock();
    this.db.state.unpinLocked(txid, 1);
    this.db.state.unlock();
  }

  // releasePins drops every pin of a closed connection.
  private releasePins(): void {
    if (this.pins.size === 0) return;
    this.db.state.lock();
    for (const [txid, n] of this.pins) this.db.state.unpinLocked(txid, n);
    this.db.state.unlock();
    this.pins.clear();
  }

  // beginWrite starts a client write transaction (README §3.5). The gate wait runs asynchronously, so
  // the connection keeps serving meanwhile; write_begun (or the wait's error) is sent when it ends.
  private beginWrite(requestId: number, m: PsBeginWrite): void {
    if (this.readOnly) {
      throw new Failure("25006", "cannot write through a read-only page-service connection", false);
    }
    if (this.db.ownerReadOnly) {
      throw new Failure("25006", "the database owner opened the file read-only", false);
    }
    if (this.write !== null || this.waiting) {
      throw violation("begin_write while a write transaction is open");
    }
    if (this.revoked) {
      this.revoked = false;
      throw idleRevoked();
    }
    this.waiting = true;
    void this.waitForGate(requestId, m);
  }

  // waitForGate acquires the gate for a begin_write (at most lock_timeout_ms; 0 = no deadline), then
  // pins the committed snapshot and answers with the cache answer and the reuse grant.
  private async waitForGate(requestId: number, m: PsBeginWrite): Promise<void> {
    const state = this.db.state;
    const deadline =
      m.lockTimeoutMs > 0n ? Date.now() + Number(m.lockTimeoutMs) : Number.POSITIVE_INFINITY;
    for (;;) {
      if (this.dead || this.db.stopped) {
        this.waiting = false;
        return;
      }
      if (state.tryGate(GATE_WORKER)) break;
      const seen = state.gate();
      if (seen === GATE_FREE) continue;
      const remaining = deadline - Date.now();
      if (remaining <= 0) {
        this.waiting = false;
        this.answer(
          requestId,
          new Failure("55P03", "could not obtain the database writer lock", false),
        );
        return;
      }
      await state.waitGateAsync(seen, Math.min(remaining, GATE_POLL_MS));
    }
    this.waiting = false;
    if (this.dead || this.db.stopped) {
      state.releaseGate();
      return;
    }
    try {
      this.send(requestId, this.startWrite(m));
    } catch (error) {
      this.fail(requestId, error);
    }
  }

  // startWrite runs with the gate held: the write pin, the cache answer, and the reuse grant — every
  // pin on the database, the write pin and the engine thread's readers included, is at or after the
  // free list's generation (page-service.md §6). Later pins are on this snapshot or newer.
  private startWrite(m: PsBeginWrite): PsMessage {
    const state = this.db.state;
    let txid: bigint;
    let grant: boolean;
    let meta: Uint8Array;
    let invalidate: number;
    let indices: number[];
    state.lock();
    try {
      try {
        txid = this.pinCommittedLocked();
      } catch (error) {
        state.releaseGate();
        throw error;
      }
      const inv = state.invalidationLocked(
        sameId(m.incarnation, this.db.incarnation),
        m.lastTxid,
        txid,
      );
      invalidate = inv.all ? PS_INVALIDATE_ALL : PS_INVALIDATE_EXACT;
      indices = inv.indices;
      let oldest = txid;
      const pinned = state.oldestPinnedLocked();
      if (pinned !== null && pinned < oldest) oldest = pinned;
      const local = state.engineOldestLocked();
      if (local !== null && local < oldest) oldest = local;
      grant = oldest >= state.freeGenTxidLocked();
      meta = state.metaLocked();
    } finally {
      state.unlock();
    }
    const pageCount = metaPageCount(meta);
    let granted: Set<number> | null = null;
    if (grant) {
      // The committed free list, read under the gate (no commit is writing it).
      let free: number[] | null = null;
      try {
        free = readFreeListChain((p) => this.db.readPage(p), freeListHead(meta), pageCount);
      } catch {
        free = null;
      }
      if (free === null) grant = false;
      else granted = new Set(free);
    }
    this.write = {
      base: txid,
      check: { committedTxid: txid, pageSize: this.db.pageSize, pageCount, granted },
      delta: null,
      failure: null,
      finishing: false,
    };
    this.armIdle();
    return {
      kind: "write_begun",
      incarnation: this.db.incarnation,
      txid,
      invalidate,
      indices,
      reuseGrant: grant,
      meta,
    };
  }

  // touch restarts the idle-in-transaction timer on every received frame.
  private touch(): void {
    if (this.write !== null && !this.write.finishing) this.armIdle();
  }

  private armIdle(): void {
    if (this.db.idleTimeoutMs <= 0) return;
    if (this.idle !== null) clearTimeout(this.idle);
    this.idle = setTimeout(() => this.revoke(), this.db.idleTimeoutMs);
  }

  private stopIdle(): void {
    if (this.idle !== null) clearTimeout(this.idle);
    this.idle = null;
  }

  // revoke ends a write transaction that received nothing for the idle-in-transaction timeout
  // (page-service.md §11.1): the gate is released, and the next write-path request reports 25P03.
  private revoke(): void {
    this.idle = null;
    if (this.write === null || this.write.finishing || this.dead) return;
    this.revokedBase = this.write.base;
    this.abortWrite();
    this.revoked = true;
  }

  // abortWrite ends the write transaction without committing: delta pages already written are
  // unreachable orphans; the write pin and the gate are released.
  private abortWrite(): void {
    const w = this.write!;
    this.write = null;
    this.stopIdle();
    this.unpin(w.base);
    this.db.state.releaseGate();
  }

  // abort ends the write transaction at the client's request (README §3.5).
  private abort(): void {
    if (this.write !== null) {
      this.abortWrite();
      return;
    }
    if (this.revoked) {
      this.revoked = false;
      return;
    }
    throw violation("abort without a write transaction");
  }

  // writePages writes validated delta pages; a write failure poisons the owner.
  private writePages(pages: { index: number; page: Uint8Array }[], pageCount: number): void {
    try {
      this.db.growTo(pageCount);
      for (const pg of pages) this.db.writePage(pg.index, pg.page);
    } catch (error) {
      this.db.stopServing();
      throw new Failure(
        "58030",
        `page service write: ${error instanceof Error ? error.message : String(error)}`,
        true,
      );
    }
  }

  // commitBegin runs check A and writes the manifest pages (README §3.5).
  private commitBegin(m: PsCommitBegin): void {
    const w = this.write;
    if (w === null) {
      if (this.revoked) return; // the revoked transaction's delta is discarded; commit_end reports
      throw violation("commit_begin without a write transaction");
    }
    if (w.delta !== null || w.failure !== null) {
      throw violation("commit_begin twice in one write transaction");
    }
    try {
      w.delta = Delta.begin(w.check, m);
    } catch (error) {
      if (!(error instanceof DeltaFailure)) throw error;
      w.failure = error;
      return;
    }
    this.writePages(w.delta.manifest, w.delta.parsed.pageCount);
  }

  // commitPages runs check B on each page and writes it (README §3.5).
  private commitPages(m: PsCommitPages): void {
    const w = this.write;
    if (w === null) {
      if (this.revoked) return;
      throw violation("commit_pages without a write transaction");
    }
    if (w.failure !== null) return;
    if (w.delta === null) throw violation("commit_pages before commit_begin");
    for (const pg of m.pages) {
      try {
        w.delta.checkPage(pg);
      } catch (error) {
        if (!(error instanceof DeltaFailure)) throw error;
        w.failure = error;
        return;
      }
    }
    this.writePages(m.pages, w.delta.parsed.pageCount);
  }

  // commitEnd runs check C; past it (the point of no return) the commit completes whatever the
  // connection does (README §3.5).
  private commitEnd(requestId: number): PsMessage | null {
    const w = this.write;
    if (w === null) {
      if (this.revoked) {
        this.revoked = false;
        throw idleRevoked();
      }
      throw violation("commit_end without a write transaction");
    }
    if (w.failure === null && w.delta === null) throw violation("commit_end before commit_begin");
    if (w.failure === null) {
      try {
        w.delta!.complete();
      } catch (error) {
        if (!(error instanceof DeltaFailure)) throw error;
        w.failure = error;
      }
    }
    if (w.failure !== null) {
      const message = w.failure.message;
      this.abortWrite();
      throw violation(message);
    }
    w.finishing = true;
    this.stopIdle();
    this.finishClientCommit(requestId, w, w.delta!);
    return null;
  }

  // finishClientCommit publishes a validated delta (page-service.md §5.6): the meta, the single durable
  // barrier, the structural publication (meta, invalidation and resolution windows; the engine thread
  // adopts the catalog at its next transaction begin), the acknowledgement, then either a gate handoff
  // for a maintenance commit or the gate's release. A write or sync failure poisons the owner.
  private finishClientCommit(requestId: number, w: WriteTxn, d: Delta): void {
    const state = this.db.state;
    try {
      this.db.writePage(Number(d.txid & 1n), d.meta);
      this.db.sync();
    } catch {
      // The meta may be durable: no error frame, only a closed connection, so the client resolves the
      // outcome with commit_status (README §3.5).
      this.write = null;
      this.unpin(w.base);
      state.releaseGate();
      this.db.stopServing();
      return;
    }
    let maintenance: boolean;
    state.lock();
    try {
      state.unpinLocked(w.base, 1);
      state.publishLocked(d.txid, d.meta, d.written);
      state.bumpEpoch();
      // A maintenance commit is due when the high-water passed the reclamation threshold and no pin —
      // every pin is older than the next txid — holds back reclamation (README §6.4).
      const pageCount = d.parsed.pageCount;
      const live = state.liveAtCompactionLocked();
      maintenance =
        state.oldestPinnedLocked() === null &&
        state.engineOldestLocked() === null &&
        (live === 0 || (pageCount > COMPACT_MIN_PAGES && pageCount > COMPACT_GROWTH * live));
    } finally {
      state.unlock();
    }
    const n = this.pins.get(w.base) ?? 0;
    if (n <= 1) this.pins.delete(w.base);
    else this.pins.set(w.base, n - 1);
    this.write = null;
    if (state.takeHangUp()) this.socket.destroy();
    else this.send(requestId, { kind: "committed", txid: d.txid });
    if (maintenance) {
      state.handGate();
      this.db.notify.postMessage("maintain");
    } else {
      state.releaseGate();
    }
  }

  // commitStatus resolves an unacknowledged commit (page-service.md §8.4). A client commit is
  // published within the event-loop turn that passed its point of no return, so none is in flight.
  private commitStatus(txid: bigint, digest: bigint): PsMessage {
    this.db.state.lock();
    try {
      return { kind: "commit_status_reply", status: this.db.state.resolveLocked(txid, digest) };
    } finally {
      this.db.state.unlock();
    }
  }

  private send(requestId: number, m: PsMessage): void {
    if (this.dead) return;
    this.socket.write(encodeFrame(requestId, m, this.db.pageSize));
  }

  // answer sends an error reply that does not end the connection.
  private answer(requestId: number, error: Failure): void {
    this.send(requestId, {
      kind: "error",
      sqlstate: error.sqlstate,
      message: error.message,
      detail: "",
      flags: error.fatal ? PS_ERROR_FLAG_FATAL : 0,
    });
  }

  // fail answers a request with an error; a fatal one closes the connection.
  private fail(requestId: number, error: unknown): void {
    let sqlstate = "08P01";
    let fatal = true;
    let message = error instanceof Error ? error.message : String(error);
    if (error instanceof Failure) {
      sqlstate = error.sqlstate;
      fatal = error.fatal;
    } else if (error instanceof EngineError) {
      sqlstate = error.code();
    }
    if (error instanceof EngineError) message = message.replace(/^[0-9A-Z]{5}: /, "");
    if (this.dead) return;
    const frame = encodeFrame(
      requestId,
      { kind: "error", sqlstate, message, detail: "", flags: fatal ? PS_ERROR_FLAG_FATAL : 0 },
      this.db.pageSize,
    );
    if (!fatal) {
      this.socket.write(frame);
      return;
    }
    this.socket.end(frame);
    this.teardown();
  }
}

// serve starts serving one database: bind its endpoint and accept connections. A bind failure is
// reported to the waiting engine thread, which then gives up serving.
const served = new Map<number, Served>();

function serve(m: OwnerServe): void {
  let db: Served;
  try {
    db = new Served(m);
  } catch (error) {
    signalAnswer(port, flag, { error: error instanceof Error ? error.message : String(error) });
    return;
  }
  let listening = false;
  db.server.on("error", (error) => {
    if (listening) return;
    db.closeFile();
    signalAnswer(port, flag, { error: error.message });
  });
  db.server.listen(m.endpoint, () => {
    listening = true;
    served.set(m.id, db);
    signalAnswer(port, flag, { ok: true });
  });
}

// stop stops serving one database: no new connections, every client sees EOF (its pins and gate
// released), then the file handle closes and the engine thread is told.
function stop(id: number): void {
  const db = served.get(id);
  if (db === undefined) {
    signalAnswer(port, flag, { closed: true });
    return;
  }
  served.delete(id);
  db.stopped = true;
  db.server.close(() => {
    db.closeFile();
    db.notify.close();
    signalAnswer(port, flag, { closed: true });
  });
  for (const connection of db.connections) connection.destroy();
}

port.on("message", (message: OwnerServe | OwnerStop | OwnerPoison) => {
  if (message.op === "serve") serve(message);
  else if (message.op === "stop") stop(message.id);
  else {
    served.get(message.id)?.stopServing();
    signalAnswer(port, flag, { poisoned: true });
  }
});
