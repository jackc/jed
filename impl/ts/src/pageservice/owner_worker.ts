// The serving owner's worker thread (spec/design/page-service.md §14, spec/pageservice/README.md §6.2).
// One worker per process serves every database the process owns (owner.ts starts it once and reuses
// it across opens): for each it owns the endpoint and every client connection, and reads pages through
// its own file handle, so a client request never waits for the engine thread's synchronous work. It
// shares only each database's ServiceState with the engine thread: begin_read pins the committed
// snapshot under the state lock, which the engine thread also holds when it publishes a commit or
// checks its reader watermark. Phase 2: the read path only — every connection is read-only.

import { closeSync, openSync, readSync } from "node:fs";
import { createServer, type Server, type Socket } from "node:net";
import { type MessagePort, workerData } from "node:worker_threads";

import "../crc32_node.ts";
import { EngineError } from "../errors.ts";
import { signalAnswer } from "./blocking.ts";
import {
  encodeFrame,
  FrameAccumulator,
  PS_ERROR_FLAG_FATAL,
  PS_HELLO_OK_FLAG_CONNECTION_READ_ONLY,
  PS_HELLO_OK_FLAG_OWNER_READ_ONLY,
  PS_INVALIDATE_ALL,
  PS_INVALIDATE_EXACT,
  PS_PROTOCOL_VERSION,
  psMaxReadPages,
  type PsHello,
  type PsMessage,
} from "./codec.ts";
import { ServiceState } from "./shared_state.ts";

// OwnerServe asks the worker to serve one owned database (owner.ts); OwnerStop stops serving it.
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
};
export type OwnerStop = { op: "stop"; id: number };

const { flag, port } = workerData as { flag: Int32Array; port: MessagePort };
let nextConnectionId = 0n;

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

function sameId(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

// Served is one database this worker serves: its shared state, read handle, endpoint, and
// connections.
class Served {
  readonly state: ServiceState;
  readonly dbPath: string;
  readonly pageSize: number;
  readonly maxReadPages: number;
  readonly formatVersion: number;
  readonly incarnation: Uint8Array;
  readonly ownerReadOnly: boolean;
  readonly connections = new Set<Connection>();
  readonly server: Server;
  // The read handle and the file generation it was opened at; compaction bumps the generation and
  // the next read reopens the (renamed-into-place) file.
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
    this.fd = openSync(m.dbPath, "r");
    this.fdGeneration = this.state.fileGeneration();
    this.server = createServer((socket) => {
      this.connections.add(new Connection(this, socket));
    });
  }

  // readPage reads one page from the file at its index.
  readPage(index: number): Uint8Array {
    const generation = this.state.fileGeneration();
    if (generation !== this.fdGeneration) {
      closeSync(this.fd);
      this.fd = openSync(this.dbPath, "r");
      this.fdGeneration = generation;
    }
    const ps = this.pageSize;
    const page = new Uint8Array(ps);
    let done = 0;
    while (done < ps) {
      const n = readSync(this.fd, page, done, ps - done, index * ps + done);
      if (n <= 0)
        throw new Failure("58030", `page service read: short read at page ${index}`, true);
      done += n;
    }
    return page;
  }

  closeFile(): void {
    try {
      closeSync(this.fd);
    } catch {
      // Already closed.
    }
  }
}

// Connection is one client connection: its frame reassembly and its pins (counted per txid; each is
// also one count in the shared pin table).
class Connection {
  private readonly db: Served;
  private readonly socket: Socket;
  private readonly id: bigint;
  private readonly frames = new FrameAccumulator();
  private readonly pins = new Map<bigint, number>();
  private helloDone = false;
  private dead = false;

  constructor(db: Served, socket: Socket) {
    this.db = db;
    this.socket = socket;
    this.id = ++nextConnectionId;
    socket.on("data", (chunk: Buffer) => this.receive(chunk));
    socket.on("error", () => {});
    socket.on("close", () => {
      this.releasePins();
      this.db.connections.delete(this);
    });
  }

  destroy(): void {
    this.socket.destroy();
  }

  private receive(chunk: Buffer): void {
    if (this.dead) return;
    let requestId = 0;
    try {
      this.frames.push(new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength));
      for (;;) {
        const frame = this.frames.next(this.db.pageSize);
        if (frame === null) return;
        requestId = frame.requestId;
        const reply = this.handle(frame.message);
        if (reply !== null) this.send(requestId, reply);
      }
    } catch (error) {
      this.fail(requestId, error);
    }
  }

  private handle(m: PsMessage): PsMessage | null {
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
        throw new Failure(
          "25006",
          "cannot write through a read-only page-service connection",
          false,
        );
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
    let flags = PS_HELLO_OK_FLAG_CONNECTION_READ_ONLY; // phase 2: every connection is read-only
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

  // beginRead pins the committed snapshot for this connection (README §3.2).
  private beginRead(incarnation: Uint8Array, lastTxid: bigint): PsMessage {
    this.db.state.lock();
    try {
      const txid = this.db.state.committedTxidLocked();
      if (!this.db.state.pinLocked(txid)) {
        throw new Failure("54000", "too many pinned snapshots on this owner", true);
      }
      this.pins.set(txid, (this.pins.get(txid) ?? 0) + 1);
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
    const n = this.pins.get(txid);
    if (n === undefined) throw violation("end_read names a txid this connection has not pinned");
    if (n === 1) this.pins.delete(txid);
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

  private send(requestId: number, m: PsMessage): void {
    this.socket.write(encodeFrame(requestId, m, this.db.pageSize));
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
    const frame = encodeFrame(
      requestId,
      { kind: "error", sqlstate, message, detail: "", flags: fatal ? PS_ERROR_FLAG_FATAL : 0 },
      this.db.pageSize,
    );
    if (!fatal) {
      this.socket.write(frame);
      return;
    }
    this.dead = true;
    this.socket.end(frame);
    this.releasePins();
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

// stop stops serving one database: no new connections, every client sees EOF (its pins released),
// then the read handle closes and the engine thread is told.
function stop(id: number): void {
  const db = served.get(id);
  if (db === undefined) {
    signalAnswer(port, flag, { closed: true });
    return;
  }
  served.delete(id);
  db.server.close(() => {
    db.closeFile();
    signalAnswer(port, flag, { closed: true });
  });
  for (const connection of db.connections) connection.destroy();
}

port.on("message", (message: OwnerServe | OwnerStop) => {
  if (message.op === "serve") serve(message);
  else stop(message.id);
});
