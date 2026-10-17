// The page client (spec/design/page-service.md §4.2, §5.3–§5.6, §14; spec/pageservice/README.md §6.3,
// §6.4): a process that is not the owner runs its own engine over a pager whose byte store reads pages
// from, and commits whole CommitDeltas through, the owner's page service instead of the file. A worker
// thread (client_worker.ts) owns the socket; the engine thread posts each request and blocks in
// Atomics.wait for the reply, matching it by request id.
//
// Pins. The client holds exactly one owner pin on each version a local reader pins, and no other
// (README §6.3): begin_read's pin is recorded at once (a duplicate is released at once) and kept for
// the local reader that follows; when a version's last local reader ends, the core releases it. A write
// transaction holds the owner's write pin on its base instead. read_pages names the newest version the
// client pins, the write pin included — page counts only grow between compactions and a pin blocks
// compaction, so it bounds every page of every older pinned version. The engine thread is the only
// sender, so no end_read, abort, or commit_end can overtake a read_pages naming the version it
// releases. Node-only.

import { tmpdir } from "node:os";

import type { BlockStore } from "../blockstore.ts";
import { crc64Update, parseCommitMeta } from "../commit_manifest.ts";
import { EngineError, engineError, type SqlState } from "../errors.ts";
import type { Engine } from "../executor.ts";
import { FORMAT_VERSION, loadEnginePaged } from "../format.ts";
import type { Environment } from "../environment.ts";
import { Pager } from "../pager.ts";
import { cacheLeaves, DEFAULT_CACHE_BYTES, SharedPaging } from "../paging.ts";
import { persistImpl } from "../persist.ts";
import type { ClientReadBegun, PageClientHandle } from "../shared.ts";
import { FileSpillSink } from "../spillfile.ts";
import { ERRORS } from "../sqlstate.ts";
import { type BlockingChannel, ProcessWorker } from "./blocking.ts";
import type { ClientAnswer, ClientRequest } from "./client_worker.ts";
import {
  decodeFrame,
  encodeFrame,
  PS_COMMIT_STATUS_COMMITTED,
  PS_COMMIT_STATUS_NOT_COMMITTED,
  PS_ERROR_FLAG_FATAL,
  PS_HELLO_FLAG_READ_ONLY,
  PS_HELLO_OK_FLAG_CONNECTION_READ_ONLY,
  PS_INVALIDATE_ALL,
  PS_MAX_FRAME_LENGTH,
  PS_PROTOCOL_VERSION,
  type PsIndexedPage,
  type PsMessage,
} from "./codec.ts";
import { currentUid, readOwnerRecord } from "./endpoint.ts";

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

// sleepMs blocks the engine thread for ms milliseconds (commit resolution's retry pause).
function sleepMs(ms: number): void {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
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

// The process's one socket worker, shared by every page-service connection it opens; conn numbers
// tag each connection there.
const socketWorker = new ProcessWorker(new URL("./client_worker.ts", import.meta.url));
let nextConn = 0;

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
  // timeoutMs bounds commit resolution's search for an owner: exactly the open's file_lock_timeout_ms
  // (0: one attempt).
  timeoutMs = 5000;
  // store is the pager's byte store, whose buffered commit a failure discards.
  store: ServiceBlockStore | null = null;
  private meta: Uint8Array = new Uint8Array(0);
  // gateHeld and writeTxid describe the write transaction: the owner's gate and the write pin's txid
  // (0 when none).
  private gateHeld = false;
  private writeTxid = 0n;
  private readonly channel: BlockingChannel;
  private readonly conn = ++nextConn;
  private nextId = 0;
  // broken is the sticky failure after the connection is lost (08006) or a fatal error.
  private broken: EngineError | null = null;
  // unsent is set when the connection was found lost before the last request could be written.
  private unsent = false;
  private closed = false;
  // release frees the process-local open-path claim when the client closes.
  release: (() => void) | null = null;

  constructor(dbPath: string) {
    this.dbPath = dbPath;
    this.channel = socketWorker.acquire();
  }

  // connect dials the endpoint and runs hello, asking for a read-only connection for a read-only
  // handle.
  connect(endpoint: string, readOnly = false): ConnectResult {
    this.broken = null;
    this.nextId = 0;
    this.pageSize = 0;
    const answer = this.channel.request({
      op: "connect",
      conn: this.conn,
      endpoint,
    } as ClientRequest) as ClientAnswer;
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
      this.channel.post({ op: "disconnect", conn: this.conn } as ClientRequest);
      const e = error as EngineError;
      return { ok: false, retry: e.code() === "08006", error: e };
    }
    if (reply.kind !== "hello_ok" || !pageSizeValid(reply.pageSize)) {
      this.channel.post({ op: "disconnect", conn: this.conn } as ClientRequest);
      return {
        ok: false,
        retry: false,
        error: engineError("protocol_violation", "unexpected reply to hello"),
      };
    }
    this.pageSize = reply.pageSize;
    this.readOnly = (reply.flags & PS_HELLO_OK_FLAG_CONNECTION_READ_ONLY) !== 0;
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
    let answer = this.channel.wait() as ClientAnswer;
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
      if (decoded.requestId === 0 && reply.kind === "notice") {
        answer = this.channel.request({
          op: "next",
          conn: this.conn,
        } as ClientRequest) as ClientAnswer;
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
    this.channel.post({ op: "send", conn: this.conn, frame, reply } as ClientRequest);
    if (!reply) {
      const answer = this.channel.wait() as ClientAnswer;
      if ("lost" in answer) this.broken = connectionLost(answer.lost);
    }
    return this.nextId;
  }

  private breakWith(error: EngineError): void {
    this.broken = error;
    this.channel.post({ op: "disconnect", conn: this.conn } as ClientRequest);
  }

  // request runs one request/reply round trip for a caller outside the engine paths (commit
  // resolution's commit_status).
  request(m: PsMessage): PsMessage {
    return this.roundTrip(m);
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
        this.resolveCommit(m.txid, digest, error);
        return;
      }
      throw new NotCommitted(error);
    }
    if (reply.kind !== "committed" || reply.txid !== m.txid) {
      this.breakWith(engineError("protocol_violation", "unexpected reply to commit_end"));
      throw this.broken!;
    }
  }

  // resolveCommit settles a commit whose acknowledgement was lost (page-service.md §8.4): it reconnects
  // to whichever process owns the database until the open timeout passes and asks commit_status. The
  // handle stays broken (08006) either way; only this commit's outcome is resolved.
  private resolveCommit(txid: bigint, digest: bigint, lost: EngineError): void {
    const deadline = Date.now() + this.timeoutMs;
    for (;;) {
      const record = readOwnerRecord(this.dbPath);
      if (record?.serving && record.uid === currentUid()) {
        const other = new PageClient(this.dbPath);
        try {
          if (other.connect(record.endpoint, true).ok) {
            const reply = other.request({ kind: "commit_status", txid, digest });
            if (reply.kind === "commit_status_reply") {
              if (reply.status === PS_COMMIT_STATUS_COMMITTED) return;
              if (reply.status === PS_COMMIT_STATUS_NOT_COMMITTED) throw new NotCommitted(lost);
              throw engineError(
                "transaction_resolution_unknown",
                "lost the connection to the database owner during commit; whether the transaction committed is unknown",
              );
            }
          }
        } catch (error) {
          if (error instanceof NotCommitted) throw error;
          if (error instanceof EngineError && error.code() === "08007") throw error;
        } finally {
          other.close();
        }
      }
      if (Date.now() >= deadline) {
        throw engineError(
          "transaction_resolution_unknown",
          "lost the connection to the database owner during commit, and no owner answered whether it committed",
        );
      }
      sleepMs(10);
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

  // close drops the connection (the owner releases every pin and the gate at EOF) and stops the socket
  // thread.
  close(): void {
    if (this.closed) return;
    this.closed = true;
    if (this.broken === null) {
      this.broken = engineError("connection_failure", "the page-service connection is closed");
    }
    this.channel.post({ op: "forget", conn: this.conn } as ClientRequest);
    socketWorker.release();
    this.release?.();
    this.release = null;
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
  const paging = new SharedPaging(
    pager,
    cacheLeaves(opts.cacheBytes ?? DEFAULT_CACHE_BYTES, pager.pageSize),
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
