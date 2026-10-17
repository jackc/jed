// The page client (spec/design/page-service.md §4.2, §5.3–§5.4, §14; spec/pageservice/README.md §6.3):
// a process that is not the owner runs its own engine over a pager whose byte store reads pages from
// the owner's page service instead of the file. A worker thread (client_worker.ts) owns the socket; the
// engine thread posts each request and blocks in Atomics.wait for the reply. Phase 2: reads only — a
// client handle is read-only.
//
// Pins. The client holds exactly one owner pin on each version a local reader pins, and no other
// (README §6.3): begin_read's pin is recorded at once (a duplicate is released at once) and kept for
// the local reader that follows; when a version's last local reader ends, the core releases it.
// read_pages names the newest held version — page counts only grow between compactions and a pin
// blocks compaction, so it bounds every page of every older pinned version. The engine thread is the
// only sender, so no end_read can overtake a read_pages naming the same version. Node-only.

import { tmpdir } from "node:os";

import type { BlockStore } from "../blockstore.ts";
import { type EngineError, engineError, type SqlState } from "../errors.ts";
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
  PS_ERROR_FLAG_FATAL,
  PS_HELLO_FLAG_READ_ONLY,
  PS_INVALIDATE_ALL,
  PS_PROTOCOL_VERSION,
  type PsMessage,
} from "./codec.ts";

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
  private meta: Uint8Array = new Uint8Array(0);
  private readonly channel: BlockingChannel;
  private readonly conn = ++nextConn;
  private nextId = 0;
  // broken is the sticky failure after the connection is lost (08006) or a fatal error.
  private broken: EngineError | null = null;
  private closed = false;
  // release frees the process-local open-path claim when the client closes.
  release: (() => void) | null = null;

  constructor(dbPath: string) {
    this.dbPath = dbPath;
    this.channel = socketWorker.acquire();
  }

  // connect dials the endpoint and runs hello.
  connect(endpoint: string): ConnectResult {
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
        flags: PS_HELLO_FLAG_READ_ONLY,
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
    return { ok: true };
  }

  // roundTrip sends one request and returns its reply. An error reply comes back as the engine error
  // it names; a lost connection is 08006 and breaks the client for good.
  private roundTrip(m: PsMessage): PsMessage {
    if (this.broken !== null) throw this.broken;
    const id = this.send(m, true);
    const answer = this.channel.wait() as ClientAnswer;
    if ("lost" in answer) {
      this.broken = connectionLost(answer.lost);
      throw this.broken;
    }
    if (!("frame" in answer)) {
      this.broken = engineError("protocol_violation", "unexpected answer from the socket thread");
      throw this.broken;
    }
    let decoded: { requestId: number; message: PsMessage };
    try {
      decoded = decodeFrame(answer.frame, this.pageSize);
    } catch (error) {
      this.breakWith(error as EngineError);
      throw error;
    }
    const reply = decoded.message;
    if (reply.kind === "error") {
      const failure = errorFromCode(reply.sqlstate, reply.message);
      if ((reply.flags & PS_ERROR_FLAG_FATAL) !== 0) this.breakWith(failure);
      throw failure;
    }
    if (decoded.requestId !== id) {
      this.breakWith(engineError("protocol_violation", "reply out of request order"));
      throw this.broken!;
    }
    return reply;
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

  // adopt makes reply's version the client's cache version: its meta serves page indices 0 and 1.
  adopt(reply: ClientReadBegun): void {
    this.meta = reply.meta;
    this.incarnation = reply.incarnation;
    this.adopted = reply.txid;
  }

  currentMeta(): Uint8Array {
    return this.meta;
  }

  // readPage fetches one page under the newest held pin.
  readPage(index: number): Uint8Array {
    let newest = -1n;
    for (const v of this.held) if (v > newest) newest = v;
    if (newest < 0n) {
      throw engineError("io_error", "a page client read a page with no pinned snapshot");
    }
    const reply = this.roundTrip({ kind: "read_pages", txid: newest, indices: [index] });
    if (reply.kind !== "pages" || reply.pages.length !== 1) {
      this.breakWith(engineError("protocol_violation", "unexpected reply to read_pages"));
      throw this.broken!;
    }
    return reply.pages[0]!;
  }

  // releasePin sends end_read for a held version v.
  releasePin(v: bigint): void {
    if (!this.held.delete(v)) return;
    this.endRead(v);
  }

  private endRead(v: bigint): void {
    if (this.broken === null && !this.closed) this.send({ kind: "end_read", txid: v }, false);
  }

  // close drops the connection (the owner releases every pin at EOF) and stops the socket thread.
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

function readOnlyError(): EngineError {
  return engineError("read_only_sql_transaction", "a page client cannot write the database");
}

// ServiceBlockStore is the client pager's byte store: page indices 0 and 1 are the adopted meta page,
// every other page comes from the owner. Read-only.
class ServiceBlockStore implements BlockStore {
  private readonly client: PageClient;

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
    const page = index < 2 ? this.client.currentMeta() : this.client.readPage(index);
    return page.slice(within, within + len);
  }

  writeAt(_offset: number, _bytes: Uint8Array): void {
    throw readOnlyError();
  }

  sync(): void {
    throw readOnlyError();
  }

  // size is the adopted snapshot's page count: every page a reader of it may name lies below it.
  size(): number {
    const m = this.client.currentMeta();
    const pageCount = ((m[24]! << 24) | (m[25]! << 16) | (m[26]! << 8) | m[27]!) >>> 0;
    return pageCount * this.client.pageSize;
  }

  setSize(_bytes: number): void {
    throw readOnlyError();
  }

  close(): void {
    this.client.close();
  }
}

export type ClientOpenOptions = { cacheBytes?: number; workMem?: number };

// openPageClient builds a read-only engine over a connected client (README §3.2): one begin_read pins
// and adopts the owner's committed snapshot, then the ordinary loader reads the catalog skeleton
// through the service. The open-time pin stays held; the caller's core releases it once open finishes
// (no local reader follows an open).
export function openPageClient(
  client: PageClient,
  opts: ClientOpenOptions,
  environment: Environment,
): Engine {
  const reply = client.beginRead();
  client.adopt(reply);
  const pager = Pager.fromStore(new ServiceBlockStore(client));
  pager.trustMeta = true;
  const paging = new SharedPaging(
    pager,
    cacheLeaves(opts.cacheBytes ?? DEFAULT_CACHE_BYTES, pager.pageSize),
  );
  const db = loadEnginePaged(paging, environment);
  db.path = client.dbPath;
  db.persistHook = persistImpl;
  db.readOnly = true;
  db.spillSink = new FileSpillSink(tmpdir());
  if (opts.workMem) db.session.workMem = opts.workMem;
  return db;
}
