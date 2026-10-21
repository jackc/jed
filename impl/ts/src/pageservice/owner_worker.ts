// The serving owner's worker thread (spec/design/page-service.md §14, spec/pageservice/README.md §6.2,
// §6.4). One worker per process serves every database the process owns (owner.ts starts it once and
// reuses it across opens): for each it owns the endpoint and every client connection, reads pages
// through its own file handle, and commits client CommitDeltas through that handle — the owner process
// is still the only one touching the file — so a client request never waits for the engine thread's
// synchronous work. It shares only each database's ServiceState with the engine thread: pins, the
// published meta and windows, and the writer gate. The connections' protocol is owner_conn.ts, shared
// with the deterministic simulator's same-thread service.

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
// The worker checks and reseals an encrypted client commit's meta (spec/fileformat/encrypted.md §14).
import "../crypto_node.ts";
import { signalAnswer } from "./blocking.ts";
import { PS_NOTICE_YIELD_CANCELLED, psMaxReadPages, type PsIndexedPage } from "./codec.ts";
import type { Delta } from "./delta.ts";
import {
  type ConnSocket,
  Connection,
  Failure,
  type ServedDb,
  type WriteTxn,
} from "./owner_conn.ts";
import { OWNER_SERVING, OWNER_YIELDING, ServiceState } from "./shared_state.ts";

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
  // preferYield: the owner's ownership preference is yield (hello_ok bit 2, README §6.5).
  preferYield: boolean;
  noSync: boolean;
  idleTimeoutMs: number;
  // idleSessionTimeoutMs closes a connection idle between transactions (0: never); maxConnections
  // refuses further hellos with 53300 (0: unlimited). README §6.5.
  idleSessionTimeoutMs: number;
  maxConnections: number;
  notify: MessagePort;
};
export type OwnerStop = { op: "stop"; id: number };
// OwnerNotice sends a notice to every connection of one database (a graceful close's kind 3).
export type OwnerNotice = { op: "notice"; id: number; kind: number };
// OwnerPoison makes the worker stop serving one database whose owner handle failed a commit.
export type OwnerPoison = { op: "poison"; id: number };

const { flag, port } = workerData as { flag: Int32Array; port: MessagePort };

// socketOf adapts a net.Socket to the connection's byte stream.
function socketOf(socket: Socket): ConnSocket {
  return {
    write: (frame) => void socket.write(frame),
    end: (frame) => void socket.end(frame),
    destroy: () => void socket.destroy(),
  };
}

// Served is one database this worker serves: its shared state, file handle, endpoint, and
// connections.
class Served implements ServedDb {
  readonly state: ServiceState;
  readonly dbPath: string;
  readonly pageSize: number;
  readonly maxReadPages: number;
  readonly formatVersion: number;
  readonly incarnation: Uint8Array;
  readonly ownerReadOnly: boolean;
  readonly preferYield: boolean;
  readonly noSync: boolean;
  readonly idleTimeoutMs: number;
  readonly idleSessionTimeoutMs: number;
  readonly maxConnections: number;
  readonly notify: MessagePort;
  readonly connections = new Set<Connection>();
  readonly server: Server;
  readonly gateWaits = true;
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
    this.preferYield = m.preferYield;
    this.noSync = m.noSync;
    this.idleTimeoutMs = m.idleTimeoutMs;
    this.idleSessionTimeoutMs = m.idleSessionTimeoutMs;
    this.maxConnections = m.maxConnections;
    this.notify = m.notify;
    this.fd = openSync(m.dbPath, this.ownerReadOnly ? "r" : "r+");
    this.fdGeneration = this.state.fileGeneration();
    this.server = createServer((socket) => {
      const connection = new Connection(this, socketOf(socket));
      this.connections.add(connection);
      socket.on("data", (chunk: Buffer) =>
        connection.receive(new Uint8Array(chunk.buffer, chunk.byteOffset, chunk.byteLength)),
      );
      socket.on("error", () => {});
      socket.on("close", () => connection.closed());
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
  private writePage(index: number, page: Uint8Array): void {
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
  private growTo(pageCount: number): void {
    const fd = this.file();
    const bytes = pageCount * this.pageSize;
    if (fstatSync(fd).size < bytes) ftruncateSync(fd, bytes);
  }

  // writeDelta writes validated delta pages; a write failure poisons the owner.
  writeDelta(w: WriteTxn, pages: PsIndexedPage[], pageCount: number): void {
    try {
      this.growTo(pageCount);
      for (const pg of pages) this.writePage(pg.index, pg.page);
      w.started = true;
    } catch (error) {
      this.stopServing();
      throw new Failure(
        "58030",
        `page service write: ${error instanceof Error ? error.message : String(error)}`,
        true,
      );
    }
  }

  // abandonDelta: the pages already written are unreachable orphans; nothing to undo.
  abandonDelta(_w: WriteTxn): void {}

  // commitDelta writes the meta to the slot not holding the committed meta (the begin_write flush made
  // it the floor, README §3.5) and runs the single durable barrier (skipped under fsync=off): a client
  // commit always keeps its own barrier, so its slot is the floor afterwards.
  commitDelta(d: Delta): boolean {
    try {
      const slot = this.state.floorSlot() ^ 1;
      this.writePage(slot, d.meta);
      if (!this.noSync) fdatasyncSync(this.file());
      this.state.publishBatch(slot, false);
      return true;
    } catch {
      return false;
    }
  }

  // flushBatch is the owner's batch flush before a begin_write (validated-cow.md §4): a durable
  // barrier on this handle covers the engine thread's writes too (fsync is per file).
  flushBatch(): boolean {
    try {
      if (!this.noSync) fdatasyncSync(this.file());
      this.state.batchFlushed();
      return true;
    } catch {
      return false;
    }
  }

  maintenanceDue(computed: boolean): boolean {
    return computed;
  }

  maintain(): void {
    this.notify.postMessage("maintain");
  }

  // localOldest is the engine thread's oldest local pin as of its last publication: a stale (older)
  // value only withholds a grant.
  localOldest(): bigint | null {
    return this.state.engineOldestLocked();
  }

  yieldAccepted(): void {
    this.notify.postMessage("yield");
  }

  // stopServing is the response to a write or sync failure in a client commit (README §3.5): the owner
  // is poisoned, every connection closes, and its own handle refuses commits until reopened.
  stopServing(): void {
    this.state.poison();
    this.stopped = true;
    this.server.close();
    for (const connection of this.connections) connection.destroy();
  }

  // broadcast sends a notice to every connection (README §3.7).
  broadcast(kind: number, connectionId: bigint): void {
    for (const connection of this.connections) connection.notice(kind, connectionId);
  }

  // cancelYield resumes serving after the requester disconnected during the drain (README §6.5):
  // notice kind 2, then the parked begins are answered. A drain that already ended (releasing) is
  // not cancelled.
  cancelYield(requester: bigint): void {
    this.state.lock();
    let cancelled = false;
    try {
      if (
        this.state.ownerState() === OWNER_YIELDING &&
        this.state.requesterLocked() === requester
      ) {
        this.state.setOwnerStateLocked(OWNER_SERVING);
        this.state.setRequesterLocked(0n);
        cancelled = true;
      }
    } finally {
      this.state.unlock();
    }
    if (!cancelled) return;
    this.broadcast(PS_NOTICE_YIELD_CANCELLED, 0n);
    for (const connection of this.connections) connection.answerParked();
    this.notify.postMessage("cancel");
  }

  closeFile(): void {
    try {
      closeSync(this.fd);
    } catch {
      // Already closed.
    }
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

port.on("message", (message: OwnerServe | OwnerStop | OwnerPoison | OwnerNotice) => {
  if (message.op === "serve") serve(message);
  else if (message.op === "stop") stop(message.id);
  else if (message.op === "notice") {
    served.get(message.id)?.broadcast(message.kind, 0n);
    signalAnswer(port, flag, { sent: true });
  } else {
    served.get(message.id)?.stopServing();
    signalAnswer(port, flag, { poisoned: true });
  }
});
