// A serving owner's client connections (spec/design/page-service.md §14, spec/pageservice/README.md
// §6.2, §6.4, §6.5): the protocol each connection speaks, shared by the production service worker
// (owner_worker.ts, over OS sockets, with its own file handle) and the deterministic simulator's
// same-thread service (src/sim/net.ts, through the owner's pager). A ServedDb is the database being
// served: its shared state, its storage operations, and its events for the engine thread; a ConnSocket
// is one connection's byte stream. Each frame is written with one write.
//
// The writer gate. A begin_write waits for the gate asynchronously (Atomics.waitAsync), so the worker
// keeps serving the connection's other requests and every other connection meanwhile; write_begun is
// sent when the wait ends, possibly out of request order (README §2). A client commit past its point of
// no return is written, synced, and published synchronously, so a commit_status (from any connection)
// never observes it half published. When a client commit is published the service hands the gate to the
// engine thread if a maintenance commit is due, which runs it and releases the gate (README §6.4; the
// service has no engine).
//
// Handoff (README §6.5). The service answers request_ownership and keeps the shared handoff state: an
// accepted request sets it to yielding, sends notice kind 1 to every connection, and tells the engine
// thread, which drains and releases ownership (a release stops serving). While the owner does not serve
// — yielding, closing, or releasing — client begins are parked unanswered; a requester that disconnects
// during the drain cancels the yield (notice kind 2) and the parked begins are answered. The owner's
// limits: a connection idle between transactions for the idle-session timeout is closed without an
// error frame, and a hello beyond the maximum connections is refused with 53300. Node-only.

import process from "node:process";

import { COMPACT_GROWTH, COMPACT_MIN_PAGES } from "../costs.ts";
import { KEY_BLOCK_LEN, osKeyEntropy, PageCodec } from "../crypto.ts";
import { EngineError } from "../errors.ts";
import {
  encodeFrame,
  FrameAccumulator,
  PS_ERROR_FLAG_FATAL,
  PS_HELLO_FLAG_READ_ONLY,
  PS_HELLO_OK_FLAG_CONNECTION_READ_ONLY,
  PS_HELLO_OK_FLAG_OWNER_READ_ONLY,
  PS_HELLO_OK_FLAG_OWNER_YIELDS,
  PS_INVALIDATE_ALL,
  PS_INVALIDATE_EXACT,
  PS_NOTICE_YIELD_BEGUN,
  PS_OWNERSHIP_REFUSED,
  PS_OWNERSHIP_YIELDING,
  PS_PROTOCOL_VERSION,
  type PsBeginWrite,
  type PsCommitBegin,
  type PsCommitPages,
  type PsHello,
  type PsIndexedPage,
  type PsMessage,
} from "./codec.ts";
import { Delta, type DeltaBase, DeltaFailure, readFreeListChain } from "./delta.ts";
import {
  ACK_CRASH,
  ACK_HANG_UP,
  GATE_FREE,
  GATE_WORKER,
  OWNER_SERVING,
  OWNER_YIELDING,
  type ServiceState,
} from "./shared_state.ts";

let nextConnectionId = 0n;

// GATE_POLL_MS caps one asynchronous gate wait, so a waiter notices its connection closing.
const GATE_POLL_MS = 500;

// ConnSocket is one client connection's byte stream: write a frame, write a last frame and close, or
// close at once.
export interface ConnSocket {
  write(frame: Uint8Array): void;
  end(frame: Uint8Array): void;
  destroy(): void;
}

// ServedDb is one database a service serves, as its connections see it.
export interface ServedDb {
  readonly state: ServiceState;
  readonly pageSize: number;
  readonly maxReadPages: number;
  readonly formatVersion: number;
  readonly incarnation: Uint8Array;
  readonly ownerReadOnly: boolean;
  readonly preferYield: boolean;
  readonly idleTimeoutMs: number;
  readonly idleSessionTimeoutMs: number;
  readonly maxConnections: number;
  readonly connections: Set<Connection>;
  // stopped is set once the owner stops serving (closed, or poisoned by a failed client commit).
  readonly stopped: boolean;
  // gateWaits is false when a begin_write meeting a held gate must be refused at once (55P03): a
  // same-thread service cannot wait for the engine thread to release it.
  readonly gateWaits: boolean;
  // readPage reads one page of the file.
  readPage(index: number): Uint8Array;
  // writeDelta writes validated delta pages of write transaction w (the new page count is
  // pageCount); a failure stops serving and throws a fatal Failure. abandonDelta ends a delta whose
  // meta was never written.
  writeDelta(w: WriteTxn, pages: PsIndexedPage[], pageCount: number): void;
  abandonDelta(w: WriteTxn): void;
  // commitDelta writes a completed delta's meta and runs the single durable barrier; false when it
  // failed (the meta may be durable).
  commitDelta(d: Delta): boolean;
  // flushBatch makes the owner's pending relaxed commits durable (spec/design/validated-cow.md §4): the
  // file's durable barrier, after which the newest meta is the floor (ServiceState.batchFlushed).
  // false when the barrier failed. Called holding the writer gate.
  flushBatch(): boolean;
  // maintenanceDue reports, holding the state lock after a client commit was published, whether to
  // hand the gate to the engine thread (computed is the service's own estimate); maintain tells the
  // engine thread, which runs the maintenance commit and releases the gate.
  maintenanceDue(computed: boolean): boolean;
  maintain(): void;
  // localOldest is the oldest version a local reader of the engine thread pins (null: none), read
  // holding the state lock.
  localOldest(): bigint | null;
  // yieldAccepted tells the engine thread to drain for an accepted ownership request.
  yieldAccepted(): void;
  stopServing(): void;
  broadcast(kind: number, connectionId: bigint): void;
  cancelYield(requester: bigint): void;
}

// Failure is a protocol-level error answer: fatal ones close the connection.
export class Failure extends Error {
  readonly sqlstate: string;
  readonly fatal: boolean;
  constructor(sqlstate: string, message: string, fatal: boolean) {
    super(message);
    this.sqlstate = sqlstate;
    this.fatal = fatal;
  }
}

// FlushFailure is a failed flush of the owner's pending batch at a begin_write: the error answer,
// after which the owner stops serving.
class FlushFailure extends Failure {
  constructor() {
    super("58030", "the database owner could not make its pending commits durable", false);
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

// reclaimDue mirrors format.ts reclaimDue (format.md *Reclamation*), kept here so the service does not
// import the engine: the pages off the free list exceed compact_min_pages and compact_growth × the
// live count at the last reclamation.
function reclaimDue(pageCount: number, free: number, live: number): boolean {
  const unlisted = pageCount - free;
  return unlisted > COMPACT_MIN_PAGES && unlisted > COMPACT_GROWTH * live;
}

function freeListHead(meta: Uint8Array): number {
  return ((meta[28]! << 24) | (meta[29]! << 16) | (meta[30]! << 8) | meta[31]!) >>> 0;
}

// freeListTag is an encrypted meta's free-list tag (offset 80, spec/fileformat/encrypted.md §6).
function freeListTag(meta: Uint8Array): Uint8Array {
  return meta.slice(80, 96);
}

// codecFor is the page codec of an encrypted database's data key (spec/fileformat/encrypted.md §14):
// the worker checks a client commit's meta under it, reseals the meta for the slot it writes with a
// nonce of its own, and opens free-list pages. The key block is never written here.
function codecFor(dek: Uint8Array | null): PageCodec | null {
  return dek === null ? null : new PageCodec(dek, osKeyEntropy(), new Uint8Array(KEY_BLOCK_LEN));
}

// WriteTxn is a client write transaction: the connection holds the gate and a write pin on base.
export type WriteTxn = {
  base: bigint;
  check: DeltaBase;
  // delta is the CommitDelta being received (between commit_begin and commit_end); failure is a
  // rejected delta, whose remaining frames are discarded and which commit_end reports.
  delta: Delta | null;
  failure: Error | null;
  // started is set once the delta's first page was written (writeDelta).
  started: boolean;
  // finishing is set at the point of no return: the commit completes whatever the connection does.
  finishing: boolean;
};

// Connection is one client connection: its frame reassembly, its pins (counted per txid, the write pin
// included; each is also one count in the shared pin table), and its write transaction. Its owner
// feeds it received bytes (receive) and reports its socket's close (closed).
export class Connection {
  private readonly db: ServedDb;
  private readonly socket: ConnSocket;
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
  // parked holds begins left unanswered while the owner does not serve (README §6.5); a cancelled
  // yield answers them.
  private parked: (() => void)[] = [];
  // session closes the connection after the idle-session timeout (page-service.md §11.1).
  private session: ReturnType<typeof setTimeout> | null = null;

  constructor(db: ServedDb, socket: ConnSocket) {
    this.db = db;
    this.socket = socket;
    this.id = ++nextConnectionId;
    this.armSession();
  }

  // closed is the socket's close: the connection's state ends, and a yield to it is cancelled.
  closed(): void {
    this.teardown();
    this.db.connections.delete(this);
    this.db.cancelYield(this.id);
  }

  // notice sends an unsolicited notice (request id 0).
  notice(kind: number, connectionId: bigint): void {
    this.send(0, { kind: "notice", noticeKind: kind, connectionId });
  }

  // answerParked answers the begins parked during a cancelled yield.
  answerParked(): void {
    const parked = this.parked;
    this.parked = [];
    for (const run of parked) {
      if (this.dead) return;
      run();
    }
  }

  // admitting reports whether begins are answered rather than parked.
  private admitting(): boolean {
    return this.db.state.ownerState() === OWNER_SERVING;
  }

  destroy(): void {
    this.socket.destroy();
  }

  // teardown ends the connection's state: its write transaction (unless past the point of no return)
  // and every pin.
  private teardown(): void {
    this.dead = true;
    this.parked = [];
    if (this.session !== null) clearTimeout(this.session);
    this.session = null;
    if (this.write !== null && !this.write.finishing) this.abortWrite();
    this.releasePins();
  }

  // receive handles every complete frame: a non-fatal error answers its request and the next frame
  // is handled; a fatal one (or an undecodable frame) closes the connection.
  receive(chunk: Uint8Array): void {
    if (this.dead) return;
    for (;;) {
      let frame: { requestId: number; message: PsMessage } | null;
      try {
        if (chunk.length > 0) {
          this.frames.push(chunk);
          chunk = new Uint8Array(0);
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
        if (!this.admitting()) {
          // Parked while the owner drains (README §6.5); a cancelled yield answers it.
          const { incarnation, lastTxid } = m;
          this.parked.push(() => {
            try {
              this.send(requestId, this.beginRead(incarnation, lastTxid));
            } catch (error) {
              this.fail(requestId, error);
            }
          });
          return null;
        }
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
      case "request_ownership":
        return this.requestOwnership();
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
    if (this.db.preferYield) flags |= PS_HELLO_OK_FLAG_OWNER_YIELDS;
    const limit = this.db.maxConnections;
    if (limit > 0 && this.db.connections.size > limit) {
      throw new Failure(
        "53300",
        `the database owner already serves its maximum number of connections (${limit})`,
        true,
      );
    }
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

  // requestOwnership answers request_ownership (README §6.5): an owner whose preference is yield, or
  // whose handle is read-only, yields to the first requester while it serves; the requester asking
  // again is answered 0 again; anyone else, or any request while yielding or closing, is refused.
  private requestOwnership(): PsMessage {
    const state = this.db.state;
    state.lock();
    let accepted = false;
    let status = PS_OWNERSHIP_REFUSED;
    try {
      const current = state.ownerState();
      if (current === OWNER_YIELDING && state.requesterLocked() === this.id) {
        status = PS_OWNERSHIP_YIELDING;
      } else if (current === OWNER_SERVING && (this.db.preferYield || this.db.ownerReadOnly)) {
        state.setOwnerStateLocked(OWNER_YIELDING);
        state.setRequesterLocked(this.id);
        status = PS_OWNERSHIP_YIELDING;
        accepted = true;
      }
    } finally {
      state.unlock();
    }
    if (accepted) {
      // The reply goes first, then the notice to every connection, then the engine thread drains.
      queueMicrotask(() => {
        this.db.broadcast(PS_NOTICE_YIELD_BEGUN, this.id);
        this.db.yieldAccepted();
      });
    }
    return { kind: "ownership_reply", status };
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
    if (!this.admitting()) {
      // Parked while the owner drains (README §6.5); a cancelled yield starts the wait.
      this.parked.push(() => void this.waitForGate(requestId, m));
      return;
    }
    void this.waitForGate(requestId, m);
  }

  // waitForGate acquires the gate for a begin_write (at most lock_timeout_ms; 0 = no deadline), then
  // pins the committed snapshot and answers with the cache answer and the reuse grant. A free gate is
  // taken synchronously; only a held one awaits.
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
      const remaining = this.db.gateWaits ? deadline - Date.now() : 0;
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
      // A failed flush of the owner's batch poisons the owner, which stops serving (README §3.5).
      if (error instanceof FlushFailure) this.db.stopServing();
    }
  }

  // startWrite runs with the gate held: the write pin, the cache answer, and the reuse grant — every
  // pin on the database, the write pin and the engine thread's readers included, is at or after the
  // free list's generation (page-service.md §6). Later pins are on this snapshot or newer.
  private startWrite(m: PsBeginWrite): PsMessage {
    const state = this.db.state;
    // The owner's own relaxed commits are flushed before a client plans against the committed meta: a
    // client cannot name the owner's pending set in its manifest (README §3.5).
    if (state.batchPending() && !this.db.flushBatch()) {
      state.releaseGate();
      throw new FlushFailure();
    }
    let txid: bigint;
    let grant: boolean;
    let meta: Uint8Array;
    let invalidate: number;
    let indices: number[];
    let dek: Uint8Array | null;
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
      const local = this.db.localOldest();
      if (local !== null && local < oldest) oldest = local;
      grant = oldest >= state.freeGenTxidLocked();
      meta = state.metaLocked();
      dek = state.keyLocked();
    } finally {
      state.unlock();
    }
    let codec: PageCodec | null;
    try {
      codec = codecFor(dek);
    } catch (error) {
      state.releaseGate();
      throw error;
    }
    const pageCount = metaPageCount(meta);
    let granted: Set<number> | null = null;
    if (grant) {
      // The committed free list, read under the gate (no commit is writing it).
      let free: number[] | null = null;
      try {
        free = readFreeListChain(
          (p) => this.db.readPage(p),
          freeListHead(meta),
          pageCount,
          codec,
          freeListTag(meta),
        );
      } catch {
        free = null;
      }
      if (free === null) grant = false;
      else granted = new Set(free);
    }
    this.write = {
      base: txid,
      check: { committedTxid: txid, pageSize: this.db.pageSize, pageCount, granted, codec },
      delta: null,
      failure: null,
      started: false,
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

  // touch restarts the idle-in-transaction and idle-session timers on every received frame.
  private touch(): void {
    if (this.write !== null && !this.write.finishing) this.armIdle();
    this.armSession();
  }

  // armSession (re)starts the idle-session timer (page-service.md §11.1).
  private armSession(): void {
    const timeout = this.db.idleSessionTimeoutMs;
    if (timeout <= 0 || this.dead) return;
    if (this.session !== null) clearTimeout(this.session);
    this.session = setTimeout(() => this.idleSessionExpired(), timeout);
  }

  // idleSessionExpired closes a connection idle between transactions — no pin, no write transaction,
  // no begin waiting — without an error frame; a busy connection's timer starts again.
  private idleSessionExpired(): void {
    this.session = null;
    if (this.dead) return;
    if (this.pins.size > 0 || this.write !== null || this.waiting || this.parked.length > 0) {
      this.armSession();
      return;
    }
    this.socket.destroy();
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
    if (w.started) this.db.abandonDelta(w);
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
    this.db.writeDelta(w, w.delta.manifest, w.delta.parsed.pageCount);
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
    this.db.writeDelta(w, m.pages, w.delta.parsed.pageCount);
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
    const codec = w.check.codec ?? null;
    if (codec !== null) {
      // A client cannot know the slot its meta lands in, so the meta, already checked against the
      // database key, is resealed for it (spec/fileformat/encrypted.md §14). Both commitDelta forms
      // write the slot not holding the floor.
      const chain = d.manifest.map((pg) => ({ index: pg.index, bytes: pg.page }));
      codec.sealMeta(state.floorSlot() ^ 1, d.meta, chain);
    }
    if (!this.db.commitDelta(d)) {
      // The meta may be durable: no error frame, only a closed connection, so the client resolves the
      // outcome with commit_status (README §3.5).
      this.write = null;
      this.unpin(w.base);
      state.releaseGate();
      this.db.stopServing();
      return;
    }
    let due: boolean;
    let quiet = false;
    let live = 0;
    state.lock();
    try {
      state.unpinLocked(w.base, 1);
      state.publishLocked(d.txid, d.meta, d.written);
      state.bumpEpoch();
      // A maintenance commit is due when no pin — every pin is older than the next txid — holds back
      // reclamation (README §6.4) and either the pages off the free list passed the reclamation
      // threshold (format.md *Reclamation*) or the free list ends at the high-water, which the
      // maintenance commit then truncates (format.md *Trailing truncation*). The committed free list
      // is read below to decide.
      live = state.liveAtCompactionLocked();
      quiet = state.oldestPinnedLocked() === null && state.engineOldestLocked() === null;
      due = quiet && live === 0;
    } finally {
      state.unlock();
    }
    if (quiet && live !== 0) {
      let free: number[] | null;
      try {
        free = readFreeListChain(
          (p) => this.db.readPage(p),
          freeListHead(d.meta),
          d.parsed.pageCount,
          codec,
          freeListTag(d.meta),
        );
      } catch {
        free = null;
      }
      // An unreadable chain hands the gate over anyway: the engine's maintenance commit decides
      // again from the free list it adopts.
      due =
        free === null ||
        reclaimDue(d.parsed.pageCount, free.length, live) ||
        free[free.length - 1] === d.parsed.pageCount - 1;
    }
    const maintenance = this.db.maintenanceDue(due);
    const n = this.pins.get(w.base) ?? 0;
    if (n <= 1) this.pins.delete(w.base);
    else this.pins.set(w.base, n - 1);
    this.write = null;
    const hook = state.takeAckHook();
    if (hook === ACK_CRASH) process.kill(process.pid, "SIGKILL");
    if (hook === ACK_HANG_UP) this.socket.destroy();
    else this.send(requestId, { kind: "committed", txid: d.txid });
    if (maintenance) {
      state.handGate();
      this.db.maintain();
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
