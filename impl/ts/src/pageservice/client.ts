// The page client (spec/design/page-service.md §4.2, §5.3–§5.6, §14; spec/pageservice/README.md §6.3,
// §6.4): a process that is not the owner runs its own engine over a pager whose byte store reads pages
// from, and commits whole CommitDeltas through, the owner's page service instead of the file. A worker
// thread (client_worker.ts) owns the socket; the engine thread posts each request and blocks in
// Atomics.wait for the reply, matching it by request id. The transport is a link (net.ts): the socket
// worker's (WorkerLink), or the deterministic simulator's in-memory network.
//
// Pins. The client holds exactly one owner pin on each version a local reader pins, and no other
// (README §6.3): begin_read's pin is recorded at once (a duplicate is released at once) and kept for
// the local reader that follows; when a version's last local reader ends, the core releases it. A write
// transaction holds the owner's write pin on its base instead. read_pages names the newest version the
// client pins, the write pin included — page counts only grow between compactions and a pin blocks
// compaction, so it bounds every page of every older pinned version. The engine thread is the only
// sender, so no end_read, abort, or commit_end can overtake a read_pages naming the version it
// releases.
//
// A PageClient is one connection. When it ends, the handle re-elects (spec/pageservice/README.md
// §6.5; shared.ts and pageservice/handoff.ts) and installs a new role with a new PageClient; this one
// stays broken (08006) for the transactions begun on it. The socket worker records the connection's
// end and the owner's notices in a status buffer this thread reads with atomic loads, and announces the
// end on an event port, so the handle can re-elect while its engine thread is idle. Node-only.

import { tmpdir } from "node:os";
import { MessageChannel, type MessagePort } from "node:worker_threads";

import type { BlockStore } from "../blockstore.ts";
import { crc64Update, parseCommitMeta } from "../commit_manifest.ts";
import { EngineError, engineError, type SqlState } from "../errors.ts";
import type { Engine } from "../executor.ts";
import { FORMAT_VERSION, loadEnginePaged, unlockPagerSteps } from "../format.ts";
import { runSync } from "../execution.ts";
import type { Environment } from "../environment.ts";
import { Pager } from "../pager.ts";
import { cacheLeaves, DEFAULT_CACHE_BYTES, SharedPaging } from "../paging.ts";
import { persistImpl } from "../persist.ts";
import type { ClientReadBegun, PageClientHandle } from "../shared.ts";
import { FileSpillSink } from "../spillfile.ts";
import { ERRORS } from "../sqlstate.ts";
import { type BlockingChannel, ProcessWorker } from "./blocking.ts";
import {
  LOST_AT_OFFSET,
  NOTICE_CONN_OFFSET,
  STATUS_BYTES,
  STATUS_LOST,
  STATUS_PAUSED,
} from "./client_status.ts";
import type { ClientAnswer, ClientRequest } from "./client_worker.ts";
import type { ClientLink } from "./net.ts";
import {
  decodeFrame,
  encodeFrame,
  PS_ERROR_FLAG_FATAL,
  PS_HELLO_FLAG_READ_ONLY,
  PS_HELLO_OK_FLAG_CONNECTION_READ_ONLY,
  PS_HELLO_OK_FLAG_OWNER_READ_ONLY,
  PS_HELLO_OK_FLAG_OWNER_YIELDS,
  PS_INVALIDATE_ALL,
  PS_MAX_FRAME_LENGTH,
  PS_PROTOCOL_VERSION,
  psMaxReadPages,
  type PsIndexedPage,
  type PsMessage,
} from "./codec.ts";

// HANDOFF_PAUSE_MS is how long a client that saw another connection's yield skips presence tries after
// its connection ends, so the requester takes ownership (README §6.5).
export const HANDOFF_PAUSE_MS = 2000;

function pageSizeValid(size: number): boolean {
  return size >= 256 && size <= 65536 && (size & (size - 1)) === 0;
}

function sameId(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

// errorFromCode rebuilds an engine error from an error frame's SQLSTATE. An unknown code (a newer
// owner) is reported as a protocol violation naming it.
function errorFromCode(code: string, message: string): EngineError {
  const desc = ERRORS.find((e) => e.code === code);
  if (desc === undefined) {
    return engineError("protocol_violation", `the owner sent unknown SQLSTATE ${code}: ${message}`);
  }
  return engineError(desc.name as SqlState, message);
}

function connectionLost(reason: string): EngineError {
  return engineError("connection_failure", `lost the connection to the database owner: ${reason}`);
}

function errClientReadOnly(): EngineError {
  return engineError(
    "read_only_sql_transaction",
    "cannot write through a read-only page-service connection",
  );
}

// NotCommitted marks a commit failure after which the owner certainly did not commit (the delta never
// completed, or the owner refused it): the client's pager may continue.
class NotCommitted extends Error {
  readonly failure: unknown;
  constructor(failure: unknown) {
    super(failure instanceof Error ? failure.message : String(failure));
    this.failure = failure;
  }
}

// CommitLost is a commit whose connection was lost after commit_end was written: the outcome is
// unknown until the handle re-elects and asks (README §6.5). failure is what a not-committed answer
// reports (08006).
export class CommitLost extends Error {
  readonly txid: bigint;
  readonly digest: bigint;
  readonly failure: EngineError;
  constructor(txid: bigint, digest: bigint, failure: EngineError) {
    super(failure.message);
    this.txid = txid;
    this.digest = digest;
    this.failure = failure;
  }
}

// The process's one socket worker, shared by every page-service connection it opens; conn numbers
// tag each connection there.
const socketWorker = new ProcessWorker(new URL("./client_worker.ts", import.meta.url));
let nextConn = 0;

// WorkerLink is a connection on the process's socket worker (client_worker.ts): the engine thread posts
// each request and blocks in Atomics.wait for the answer. The worker records the connection's end and
// the owner's notices in a status buffer read here with atomic loads, and announces the end on an
// event port.
export class WorkerLink implements ClientLink {
  private readonly channel: BlockingChannel;
  private readonly conn = ++nextConn;
  private readonly status = new SharedArrayBuffer(STATUS_BYTES);
  private readonly statusFlags = new Int32Array(this.status, 0, 2);
  private readonly events: MessagePort;
  private readonly remoteEvents: MessagePort;
  // attached is set once the status buffer and the event port went to the worker.
  private attached = false;

  constructor() {
    this.channel = socketWorker.acquire();
    const { port1, port2 } = new MessageChannel();
    this.events = port1;
    this.remoteEvents = port2;
    this.events.unref();
  }

  connect(endpoint: string): ClientAnswer {
    const request: ClientRequest = { op: "connect", conn: this.conn, endpoint };
    if (this.attached) return this.channel.request(request) as ClientAnswer;
    this.attached = true;
    request.status = this.status;
    request.events = this.remoteEvents;
    return this.channel.request(request, [this.remoteEvents]) as ClientAnswer;
  }

  send(frame: Uint8Array, reply: boolean): void {
    this.channel.post({ op: "send", conn: this.conn, frame, reply } as ClientRequest);
  }

  wait(): ClientAnswer {
    return this.channel.wait() as ClientAnswer;
  }

  next(): ClientAnswer {
    return this.channel.request({ op: "next", conn: this.conn } as ClientRequest) as ClientAnswer;
  }

  disconnect(): void {
    this.channel.post({ op: "disconnect", conn: this.conn } as ClientRequest);
  }

  close(): void {
    this.channel.post({ op: "forget", conn: this.conn } as ClientRequest);
    socketWorker.release();
    // As WorkerOwnerLink.stop: detach the listener so the closed port, a GC root until the event loop
    // turns, does not keep the engine its handler reaches.
    this.events.removeAllListeners();
    this.events.close();
  }

  lost(): boolean {
    return Atomics.load(this.statusFlags, STATUS_LOST) === 1;
  }

  waitForLoss(deadlineMs: number): boolean {
    for (;;) {
      if (this.lost()) return true;
      const remaining = deadlineMs - Date.now();
      if (remaining <= 0) return false;
      Atomics.wait(
        this.statusFlags,
        STATUS_LOST,
        0,
        Number.isFinite(remaining) ? remaining : undefined,
      );
    }
  }

  pausedBy(): bigint | null {
    if (Atomics.load(this.statusFlags, STATUS_PAUSED) !== 1) return null;
    return new BigInt64Array(this.status, NOTICE_CONN_OFFSET, 1)[0]!;
  }

  lostAtMs(): number {
    return new Float64Array(this.status, LOST_AT_OFFSET, 1)[0]!;
  }

  onLost(handler: () => void): void {
    this.events.on("message", handler);
    this.events.unref();
  }
}

// ConnectResult is one connection attempt's outcome: a connected client, or an error the election may
// retry (a failed connect or a dropped connection) or must report (the owner refused hello).
export type ConnectResult = { ok: true } | { ok: false; retry: boolean; error: EngineError };

export class PageClient implements PageClientHandle {
  readonly dbPath: string;
  readonly held = new Set<bigint>();
  incarnation: Uint8Array = new Uint8Array(16);
  adopted = 0n;
  pageSize = 0;
  // readOnly is hello_ok's answer: the connection is read-only.
  readOnly = false;
  // reuseGrant is the write transaction's reuse grant (write_begun).
  reuseGrant = false;
  // connectionId, ownerReadOnly, and ownerYields are hello_ok's (README §3.1, §6.5).
  connectionId = 0n;
  ownerReadOnly = false;
  ownerYields = false;
  // store is the pager's byte store, whose buffered commit a failure discards.
  store: ServiceBlockStore | null = null;
  private meta: Uint8Array = new Uint8Array(0);
  // gateHeld and writeTxid describe the write transaction: the owner's gate and the write pin's txid
  // (0 when none).
  private gateHeld = false;
  private writeTxid = 0n;
  // link is the connection's transport: the socket worker, or the simulator's in-memory network.
  private readonly link: ClientLink;
  private nextId = 0;
  // broken is the sticky failure after the connection is lost (08006) or a fatal error.
  private broken: EngineError | null = null;
  // unsent is set when the connection was found lost before the last request could be written.
  private unsent = false;
  private closed = false;
  // discard holds the request ids whose replies nobody waits for (a fire-and-forget
  // request_ownership); they are dropped when they arrive.
  private readonly discard = new Set<number>();

  constructor(dbPath: string, link: ClientLink = new WorkerLink()) {
    this.dbPath = dbPath;
    this.link = link;
  }

  // onLost installs the handler run (on the event loop) when the connection ends.
  onLost(handler: () => void): void {
    this.link.onLost(handler);
  }

  // lost reports whether the connection has ended (noticed by the link or by this thread).
  lost(): boolean {
    return this.broken !== null || this.link.lost();
  }

  // waitForLoss blocks until the connection ends or deadlineMs (Date.now() scale; Infinity: no
  // deadline) passes; it reports whether the connection ended.
  waitForLoss(deadlineMs: number): boolean {
    if (this.broken !== null) return true;
    return this.link.waitForLoss(deadlineMs);
  }

  // handoffPauseUntil is when this client's handoff pause ends (README §6.5; 0: not paused): a
  // yield-begun notice naming another connection pauses presence tries for HANDOFF_PAUSE_MS after the
  // connection ends.
  handoffPauseUntil(): number {
    const requester = this.link.pausedBy();
    if (requester === null || requester === this.connectionId) return 0;
    const lostAt = this.link.lostAtMs();
    return (lostAt > 0 ? lostAt : Date.now()) + HANDOFF_PAUSE_MS;
  }

  // connect dials the endpoint and runs hello, asking for a read-only connection for a read-only
  // handle.
  connect(endpoint: string, readOnly = false): ConnectResult {
    this.broken = null;
    this.nextId = 0;
    this.pageSize = 0;
    const answer = this.link.connect(endpoint);
    if (!("ok" in answer)) {
      return { ok: false, retry: true, error: connectionLost("lost" in answer ? answer.lost : "") };
    }
    let reply: PsMessage;
    try {
      reply = this.roundTrip({
        kind: "hello",
        protocolMin: PS_PROTOCOL_VERSION,
        protocolMax: PS_PROTOCOL_VERSION,
        formatVersion: FORMAT_VERSION,
        flags: readOnly ? PS_HELLO_FLAG_READ_ONLY : 0,
        clientName: "jed-ts",
      });
    } catch (error) {
      this.link.disconnect();
      const e = error as EngineError;
      // A full owner (53300) is retried by a re-election; the election decides.
      return { ok: false, retry: e.code() === "08006" || e.code() === "53300", error: e };
    }
    if (reply.kind !== "hello_ok" || !pageSizeValid(reply.pageSize)) {
      this.link.disconnect();
      return {
        ok: false,
        retry: false,
        error: engineError("protocol_violation", "unexpected reply to hello"),
      };
    }
    this.pageSize = reply.pageSize;
    this.connectionId = reply.connectionId;
    this.readOnly = (reply.flags & PS_HELLO_OK_FLAG_CONNECTION_READ_ONLY) !== 0;
    this.ownerReadOnly = (reply.flags & PS_HELLO_OK_FLAG_OWNER_READ_ONLY) !== 0;
    this.ownerYields = (reply.flags & PS_HELLO_OK_FLAG_OWNER_YIELDS) !== 0;
    return { ok: true };
  }

  // roundTrip sends one request and returns its reply. An error reply comes back as the engine error
  // it names; a lost connection is 08006 and breaks the client for good.
  private roundTrip(m: PsMessage): PsMessage {
    if (this.broken !== null) throw this.broken;
    return this.awaitReply(this.send(m, true));
  }

  // awaitReply waits for the reply carrying request id. Unsolicited notices (request id 0) are skipped;
  // a frame answering no waiting request breaks the client (a fatal error for a reply-less request
  // breaks it with that error).
  private awaitReply(id: number): PsMessage {
    let answer = this.link.wait();
    for (;;) {
      if ("lost" in answer) {
        this.unsent = answer.unsent === true;
        this.broken = connectionLost(answer.lost);
        throw this.broken;
      }
      if (!("frame" in answer)) {
        this.breakWith(
          engineError("protocol_violation", "unexpected answer from the socket thread"),
        );
        throw this.broken!;
      }
      let decoded: { requestId: number; message: PsMessage };
      try {
        decoded = decodeFrame(answer.frame, this.pageSize);
      } catch (error) {
        this.breakWith(error as EngineError);
        throw error;
      }
      const reply = decoded.message;
      if (
        (decoded.requestId === 0 && reply.kind === "notice") ||
        (decoded.requestId !== id && this.discard.delete(decoded.requestId))
      ) {
        answer = this.link.next();
        continue;
      }
      if (decoded.requestId !== id) {
        this.breakWith(
          reply.kind === "error"
            ? errorFromCode(reply.sqlstate, reply.message)
            : engineError("protocol_violation", "the owner sent a reply no request is waiting for"),
        );
        throw this.broken!;
      }
      if (reply.kind === "error") {
        const failure = errorFromCode(reply.sqlstate, reply.message);
        if ((reply.flags & PS_ERROR_FLAG_FATAL) !== 0) this.breakWith(failure);
        throw failure;
      }
      return reply;
    }
  }

  // send writes one request frame; reply says whether its answer is awaited. Returns the request id.
  // A reply-less request returns once its frame is written; a lost connection then breaks the client.
  private send(m: PsMessage, reply: boolean): number {
    this.nextId = this.nextId === 0xffff_ffff ? 1 : this.nextId + 1;
    const frame = encodeFrame(this.nextId, m, this.pageSize);
    this.link.send(frame, reply);
    if (!reply) {
      const answer = this.link.wait();
      if ("lost" in answer) this.broken = connectionLost(answer.lost);
    }
    return this.nextId;
  }

  private breakWith(error: EngineError): void {
    this.broken = error;
    this.link.disconnect();
  }

  // request runs one request/reply round trip for a caller outside the engine paths (commit
  // resolution's commit_status, a writer's request_ownership).
  request(m: PsMessage): PsMessage {
    return this.roundTrip(m);
  }

  // askOwnership sends request_ownership and waits for the reply (README §6.5 case 2); the handoff
  // itself arrives as the connection's end.
  askOwnership(): void {
    this.roundTrip({ kind: "request_ownership" });
  }

  // commitStatus asks whether a commit happened (README §3.5): 0 committed, 1 not, 2 unknown.
  commitStatus(txid: bigint, digest: bigint): number {
    const reply = this.roundTrip({ kind: "commit_status", txid, digest });
    if (reply.kind !== "commit_status_reply") {
      this.breakWith(engineError("protocol_violation", "unexpected reply to commit_status"));
      throw this.broken!;
    }
    return reply.status;
  }

  // requestOwnershipNoWait sends request_ownership without waiting for the reply (README §6.5 case 1):
  // the outcome arrives as the connection's end. A failure to send is ignored.
  requestOwnershipNoWait(): void {
    if (this.broken !== null || this.closed) return;
    const id = this.send({ kind: "request_ownership" }, false);
    this.discard.add(id);
  }

  // beginRead runs begin_read for the client's cache and records the pin it carries: held from now on
  // for the local reader that follows, or released at once when it duplicates a held one.
  beginRead(): ClientReadBegun {
    const reply = this.roundTrip({
      kind: "begin_read",
      incarnation: this.incarnation,
      lastTxid: this.adopted,
    });
    if (reply.kind !== "read_begun") {
      this.breakWith(engineError("protocol_violation", "unexpected reply to begin_read"));
      throw this.broken!;
    }
    if (this.held.has(reply.txid)) this.endRead(reply.txid);
    else this.held.add(reply.txid);
    return {
      txid: reply.txid,
      incarnation: reply.incarnation,
      invalidateAll:
        reply.invalidate === PS_INVALIDATE_ALL || !sameId(reply.incarnation, this.incarnation),
      indices: reply.indices,
      meta: reply.meta,
    };
  }

  // beginWrite takes the owner's writer gate (README §3.5), waiting at most timeoutMs, and holds the
  // write pin it carries until endWrite or the commit.
  beginWrite(timeoutMs: number): ClientReadBegun {
    if (this.readOnly) throw errClientReadOnly();
    const reply = this.roundTrip({
      kind: "begin_write",
      incarnation: this.incarnation,
      lastTxid: this.adopted,
      lockTimeoutMs: BigInt(Math.max(0, Math.floor(timeoutMs))),
    });
    if (reply.kind !== "write_begun") {
      this.breakWith(engineError("protocol_violation", "unexpected reply to begin_write"));
      throw this.broken!;
    }
    this.gateHeld = true;
    this.reuseGrant = reply.reuseGrant;
    this.writeTxid = reply.txid;
    return {
      txid: reply.txid,
      incarnation: reply.incarnation,
      invalidateAll:
        reply.invalidate === PS_INVALIDATE_ALL || !sameId(reply.incarnation, this.incarnation),
      indices: reply.indices,
      meta: reply.meta,
    };
  }

  // endWrite releases the gate of a write transaction whose commit never reached the owner (abort).
  endWrite(): void {
    if (!this.gateHeld) return;
    this.gateHeld = false;
    this.writeTxid = 0n;
    if (this.broken === null && !this.closed) this.send({ kind: "abort" }, false);
  }

  setMeta(meta: Uint8Array): void {
    this.meta = meta;
  }

  markAdopted(incarnation: Uint8Array, txid: bigint): void {
    this.incarnation = incarnation;
    this.adopted = txid;
  }

  // adoptOwnCommit records this process's own published commit as its adopted version: the owner
  // published exactly it, with no commit between the write base and it, so the cache is current.
  adoptOwnCommit(txid: bigint, meta: Uint8Array): void {
    if (txid <= this.adopted) return;
    this.meta = meta.slice();
    this.adopted = txid;
  }

  discardPending(): void {
    this.store?.discard();
  }

  failedCommit(error: unknown): { error: unknown; notCommitted: boolean } {
    if (error instanceof NotCommitted) return { error: error.failure, notCommitted: true };
    return { error, notCommitted: false };
  }

  // lostCommit recognizes a commit whose acknowledgement was lost (CommitLost), or returns null.
  lostCommit(error: unknown): { txid: bigint; digest: bigint; failure: EngineError } | null {
    return error instanceof CommitLost ? error : null;
  }

  currentMeta(): Uint8Array {
    return this.meta;
  }

  // readPage fetches one page under the newest pinned version, the write pin included.
  readPage(index: number): Uint8Array {
    let newest = this.writeTxid;
    for (const v of this.held) if (v > newest) newest = v;
    if (newest === 0n) {
      throw engineError("io_error", "a page client read a page with no pinned snapshot");
    }
    const reply = this.roundTrip({ kind: "read_pages", txid: newest, indices: [index] });
    if (reply.kind !== "pages" || reply.pages.length !== 1) {
      this.breakWith(engineError("protocol_violation", "unexpected reply to read_pages"));
      throw this.broken!;
    }
    return reply.pages[0]!;
  }

  // readPages fetches a batch of pages under the newest pinned version (a backup's copy, README.md
  // §6.6). The batch holds at most maxReadPages() indices.
  readPages(indices: number[]): Uint8Array[] {
    let newest = this.writeTxid;
    for (const v of this.held) if (v > newest) newest = v;
    if (newest === 0n) {
      throw engineError("io_error", "a page client read a page with no pinned snapshot");
    }
    const reply = this.roundTrip({ kind: "read_pages", txid: newest, indices: [...indices] });
    if (reply.kind !== "pages" || reply.pages.length !== indices.length) {
      this.breakWith(engineError("protocol_violation", "unexpected reply to read_pages"));
      throw this.broken!;
    }
    return reply.pages;
  }

  // maxReadPages is the most indices one read_pages may name on this connection (README.md §3.1).
  maxReadPages(): number {
    return psMaxReadPages(this.pageSize);
  }

  // commit sends a CommitDelta (README §3.5): commit_begin with the meta and the manifest chain, the
  // body pages in ascending index order in frame-sized chunks, then commit_end, and waits for the owner.
  // A connection lost after commit_end was written leaves the outcome unknown; resolveCommit asks.
  commit(meta: Uint8Array, pages: Map<number, Uint8Array>): void {
    const ps = this.pageSize;
    const m = parseCommitMeta(meta, ps, 0xffff_ffff);
    if (m === null) {
      throw new NotCommitted(
        engineError("data_corrupted", "a page-client commit built an invalid meta page"),
      );
    }
    const manifest: PsIndexedPage[] = [];
    const isManifest = new Set<number>();
    for (let id = m.overflowHead, n = 0; n < m.overflowCount; n++) {
      const page = pages.get(id);
      if (page === undefined) {
        throw new NotCommitted(
          engineError("data_corrupted", "a page-client commit lost a manifest page"),
        );
      }
      manifest.push({ index: id, page });
      isManifest.add(id);
      id = ((page[8]! << 24) | (page[9]! << 16) | (page[10]! << 8) | page[11]!) >>> 0;
    }
    const body = [...pages.keys()].filter((index) => !isManifest.has(index)).sort((a, b) => a - b);
    // A frame holds length's 5 bytes of code + request id, the list count, and (index, page) items.
    const perFrame = Math.floor((PS_MAX_FRAME_LENGTH - 5 - 4) / (4 + ps));
    if (5 + 8 + ps + 4 + manifest.length * (4 + ps) > PS_MAX_FRAME_LENGTH) {
      throw new NotCommitted(
        engineError(
          "program_limit_exceeded",
          "the transaction is too large to commit through the page service",
        ),
      );
    }
    const digest = crc64Update(0n, meta);
    const sendPart = (part: PsMessage): void => {
      if (this.broken !== null) throw new NotCommitted(this.broken);
      this.send(part, false);
      if (this.broken !== null) throw new NotCommitted(this.broken);
    };
    sendPart({ kind: "commit_begin", txid: m.txid, meta, manifestPages: manifest });
    for (let start = 0; start < body.length; start += perFrame) {
      const chunk = body.slice(start, start + perFrame);
      sendPart({
        kind: "commit_pages",
        pages: chunk.map((index) => ({ index, page: pages.get(index)! })),
      });
    }
    if (this.broken !== null) throw new NotCommitted(this.broken);
    // The owner releases the write pin and the gate when it processes commit_end, whatever its outcome,
    // so no later request may name the write base.
    this.writeTxid = 0n;
    this.gateHeld = false;
    this.unsent = false;
    const id = this.send({ kind: "commit_end" }, true);
    let reply: PsMessage;
    try {
      reply = this.awaitReply(id);
    } catch (error) {
      if (error instanceof EngineError && error.code() === "08006" && !this.unsent) {
        throw new CommitLost(m.txid, digest, error);
      }
      throw new NotCommitted(error);
    }
    if (reply.kind !== "committed" || reply.txid !== m.txid) {
      this.breakWith(engineError("protocol_violation", "unexpected reply to commit_end"));
      throw this.broken!;
    }
  }

  // releasePin sends end_read for a held version v.
  releasePin(v: bigint): void {
    if (!this.held.delete(v)) return;
    this.endRead(v);
  }

  private endRead(v: bigint): void {
    if (this.broken === null && !this.closed) this.send({ kind: "end_read", txid: v }, false);
  }

  // disconnect drops the connection without breaking the client, which notices at its next request
  // (the simulator's `disconnect` step).
  disconnect(): void {
    this.link.disconnect();
  }

  // close drops the connection (the owner releases every pin and the gate at EOF) and stops the socket
  // thread.
  close(): void {
    if (this.closed) return;
    this.closed = true;
    if (this.broken === null) {
      this.broken = engineError("connection_failure", "the page-service connection is closed");
    }
    this.link.close();
  }
}

// ServiceBlockStore is the client pager's byte store: page indices 0 and 1 are the adopted meta page,
// every other page comes from the owner. A commit's writes collect here (the pager reads a pending page
// back from here), and its sync sends them as one CommitDelta (page-service.md §5.6).
class ServiceBlockStore implements BlockStore {
  private readonly client: PageClient;
  private pending = new Map<number, Uint8Array>();
  private pendingMeta: Uint8Array | null = null;
  private reserved = 0;

  constructor(client: PageClient) {
    this.client = client;
  }

  readAt(offset: number, len: number): Uint8Array {
    const ps = this.client.pageSize;
    const index = Math.floor(offset / ps);
    const within = offset - index * ps;
    if (len < 0 || within + len > ps) {
      throw engineError("io_error", "page-service read must stay within one page");
    }
    const page =
      index < 2
        ? this.client.currentMeta()
        : (this.pending.get(index) ?? this.client.readPage(index));
    return page.slice(within, within + len);
  }

  writeAt(offset: number, bytes: Uint8Array): void {
    const ps = this.client.pageSize;
    if (this.client.readOnly) throw errClientReadOnly();
    if (offset % ps !== 0 || bytes.length !== ps) {
      throw engineError("io_error", "page-service write must be one whole page");
    }
    const index = offset / ps;
    if (index < 2) this.pendingMeta = bytes.slice();
    else this.pending.set(index, bytes.slice());
  }

  // sync sends the pending commit (its meta written last) and waits for the owner's answer.
  sync(): void {
    const meta = this.pendingMeta;
    if (meta === null) return;
    const pages = this.pending;
    this.discard();
    this.client.commit(meta, pages);
  }

  // discard drops a commit that never reached the owner.
  discard(): void {
    this.pendingMeta = null;
    this.pending = new Map();
  }

  // size is the adopted snapshot's page count (every page a reader of it may name lies below it), or
  // the space a commit in progress reserved.
  size(): number {
    const m = this.client.currentMeta();
    const pageCount = ((m[24]! << 24) | (m[25]! << 16) | (m[26]! << 8) | m[27]!) >>> 0;
    return Math.max(pageCount * this.client.pageSize, this.reserved);
  }

  setSize(bytes: number): void {
    if (this.client.readOnly) throw errClientReadOnly();
    this.reserved = Math.max(this.reserved, bytes);
  }

  close(): void {
    this.client.close();
  }
}

export type ClientOpenOptions = { cacheBytes?: number; workMem?: number };

// openPageClient builds an engine over a connected client (README §3.2): one begin_read pins and
// adopts the owner's committed snapshot, then the ordinary loader reads the catalog skeleton through the
// service. The open-time pin stays held; the caller's core releases it once open finishes (no local
// reader follows an open). The handle is read-only exactly when the connection is.
export function openPageClient(
  client: PageClient,
  opts: ClientOpenOptions,
  environment: Environment,
): Engine {
  const reply = client.beginRead();
  client.setMeta(reply.meta);
  client.markAdopted(reply.incarnation, reply.txid);
  const store = new ServiceBlockStore(client);
  client.store = store;
  const pager = Pager.fromStore(store);
  pager.trustMeta = true;
  // A page client of an encrypted file would need its key; the page service serves plaintext files
  // only, so a client opens without one (spec/fileformat/encrypted.md §10).
  const codec = runSync(unlockPagerSteps(pager, undefined, environment));
  const paging = new SharedPaging(
    pager,
    cacheLeaves(opts.cacheBytes ?? DEFAULT_CACHE_BYTES, pager.pageSize),
    codec,
  );
  const db = loadEnginePaged(paging, environment);
  db.path = client.dbPath;
  db.persistHook = persistImpl;
  db.readOnly = client.readOnly;
  // The file and its directory are the owner's; its commit makes them durable.
  db.dirSynced = true;
  db.spillSink = new FileSpillSink(tmpdir());
  if (opts.workMem) db.session.workMem = opts.workMem;
  return db;
}
