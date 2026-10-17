// The deterministic-simulation network (spec/design/simulation-testing.md §14 S8), the TS port of the
// Go runner's sim_net_test.go: the page service's owner records, endpoints, and stream connections
// between simulated processes, standing in for the bundle files and Unix sockets of the OS host.
//
// The Go runner keeps the engine's connection goroutines and waits for the network to settle after
// every step. Here nothing runs on another thread: an owner's service (SimServed) runs the shared
// connection protocol (pageservice/owner_conn.ts) on the engine thread, and a client's link
// (SimClientLink) delivers each frame to it synchronously, so the owner's handler runs to completion —
// a commit's maintenance commit included — and its replies are queued for the client before the
// client's wait returns. A connection's end is reported to the owner's side after the handler that
// caused it returns, as the Go owner's goroutine notices end of stream only after its current request.
// Every frame either side writes is one write, counted for the armed `disconnect nth=N` fault, which
// ends the connection carrying the step's Nth frame instead of delivering it. A process that dies
// loses its listener and connections at once, as the kernel closes a dead process's sockets.

import { metaChecksum } from "../commit_manifest.ts";
import { engineError } from "../errors.ts";
import type { BlockStore } from "../blockstore.ts";
import type { Engine } from "../executor.ts";
import type { FileHost } from "../filehost.ts";
import type { ClientAnswer } from "../pageservice/client_worker.ts";
import {
  PS_CODE_NOTICE,
  PS_NOTICE_YIELD_BEGUN,
  PS_NOTICE_YIELD_CANCELLED,
  type PsIndexedPage,
  type PsOwnerRecord,
  psMaxReadPages,
} from "../pageservice/codec.ts";
import type { Delta } from "../pageservice/delta.ts";
import type { ClientLink, OwnerLink, OwnerServeArgs, PageNet } from "../pageservice/net.ts";
import {
  type ConnSocket,
  Connection,
  type ServedDb,
  type WriteTxn,
} from "../pageservice/owner_conn.ts";
import { OWNER_SERVING, OWNER_YIELDING, type ServiceState } from "../pageservice/shared_state.ts";

// SimEnd is one side of a simulated stream connection. closed is set once this side closed its end;
// eof once its peer did. An owner's side runs a Connection (onData, onClose); a client's side hands
// what arrives to its link.
class SimEnd {
  readonly pid: number;
  peer!: SimEnd;
  closed = false;
  eof = false;
  // The owner's side: the connection's handlers, and whether its end of stream was reported.
  onData: ((frame: Uint8Array) => void) | null = null;
  onClose: (() => void) | null = null;
  closeReported = false;
  // The client's side: its link.
  link: SimClientLink | null = null;

  constructor(pid: number) {
    this.pid = pid;
  }
}

// SimListener is a simulated endpoint: accept runs the owner's side of each new connection.
type SimListener = {
  endpoint: string;
  pid: number;
  accept: (end: SimEnd) => void;
  closed: boolean;
};

// SimNet is the network all simulated processes share.
export class SimNet implements PageNet {
  readonly records = new Map<string, PsOwnerRecord>();
  private readonly listeners = new Map<string, SimListener>();
  private readonly ends = new Set<SimEnd>();
  // frames counts the frames written during the step; at disconnectAt (0: none) the connection
  // carrying that frame ends instead of delivering it (the armed `disconnect` fault).
  frames = 0;
  private disconnectAt = 0;
  disconnected = false;
  // Deferred owner-side events (a connection's end of stream), run once the current handler returns.
  private readonly queue: (() => void)[] = [];
  private pumping = false;

  // beginStep resets the step's frame count and arms a disconnect at frame nth (0: none).
  beginStep(nth: number): void {
    this.frames = 0;
    this.disconnectAt = nth;
    this.disconnected = false;
  }

  // pump runs the deferred events until none is left.
  pump(): void {
    if (this.pumping) return;
    this.pumping = true;
    try {
      for (let next = this.queue.shift(); next !== undefined; next = this.queue.shift()) next();
    } finally {
      this.pumping = false;
    }
  }

  // kill closes every listener and connection of process pid.
  kill(pid: number): void {
    for (const l of [...this.listeners.values()]) if (l.pid === pid) this.closeListener(l);
    for (const e of [...this.ends]) if (e.pid === pid) this.close(e);
    this.pump();
  }

  // --- endpoints ---

  listen(pid: number, endpoint: string, accept: (end: SimEnd) => void): SimListener {
    const old = this.listeners.get(endpoint);
    if (old !== undefined) this.closeListener(old);
    const l: SimListener = { endpoint, pid, accept, closed: false };
    this.listeners.set(endpoint, l);
    return l;
  }

  closeListener(l: SimListener): void {
    l.closed = true;
    if (this.listeners.get(l.endpoint) === l) this.listeners.delete(l.endpoint);
  }

  removeEndpoint(endpoint: string): void {
    const l = this.listeners.get(endpoint);
    if (l !== undefined) this.closeListener(l);
  }

  // dial connects process pid to endpoint, running the owner's accept at once; null when refused.
  dial(pid: number, endpoint: string, link: SimClientLink): SimEnd | null {
    const l = this.listeners.get(endpoint);
    if (l === undefined || l.closed) return null;
    const client = new SimEnd(pid);
    const server = new SimEnd(l.pid);
    client.peer = server;
    server.peer = client;
    client.link = link;
    this.ends.add(client);
    this.ends.add(server);
    l.accept(server);
    return client;
  }

  // --- the byte stream ---

  // write sends one frame from end: false when the connection is gone (not counted) or the armed
  // disconnect ended it at this frame.
  write(end: SimEnd, frame: Uint8Array): boolean {
    if (end.closed || end.eof) return false;
    this.frames++;
    if (this.disconnectAt > 0 && this.frames === this.disconnectAt) {
      this.disconnected = true;
      this.close(end);
      this.close(end.peer);
      return false;
    }
    const to = end.peer;
    if (to.onData !== null) to.onData(frame);
    else to.link?.arrive(frame);
    return true;
  }

  // close ends one side: its peer reads end of stream once its inbox drains. The owner's side of the
  // connection reports end of stream once, after the current handler, and then closes its own end.
  close(end: SimEnd): void {
    if (!end.closed) {
      end.closed = true;
      end.peer.eof = true;
    }
    for (const side of [end, end.peer]) {
      if (side.onClose === null || side.closeReported) continue;
      side.closeReported = true;
      const onClose = side.onClose;
      this.queue.push(() => {
        onClose();
        this.close(side);
      });
    }
    if (end.closed && end.peer.closed) {
      this.ends.delete(end);
      this.ends.delete(end.peer);
    }
  }

  // --- PageNet ---

  link(pid: number): ClientLink {
    return new SimClientLink(this, pid);
  }

  serve(pid: number, args: OwnerServeArgs): OwnerLink {
    return new SimServed(this, pid, args);
  }
}

// SimClientLink is a page client's connection on the simulated network.
class SimClientLink implements ClientLink {
  private readonly net: SimNet;
  private readonly pid: number;
  private end: SimEnd | null = null;
  // inbox holds the reply frames not yet waited for; answer is a pending immediate answer (a written
  // reply-less frame, or a frame the connection's end left unsent).
  private readonly inbox: Uint8Array[] = [];
  private answer: ClientAnswer | null = null;
  private paused: bigint | null = null;

  constructor(net: SimNet, pid: number) {
    this.net = net;
    this.pid = pid;
  }

  // arrive receives one frame from the owner: a notice is recorded (as the socket worker does), any
  // other frame queued.
  arrive(frame: Uint8Array): void {
    const requestId = ((frame[5]! << 24) | (frame[6]! << 16) | (frame[7]! << 8) | frame[8]!) >>> 0;
    if (requestId !== 0 || frame[4] !== PS_CODE_NOTICE) {
      this.inbox.push(frame);
      return;
    }
    if (frame.length < 18) return;
    const kind = frame[9]!;
    if (kind === PS_NOTICE_YIELD_BEGUN) {
      this.paused = new DataView(frame.buffer, frame.byteOffset, frame.byteLength).getBigInt64(10);
    } else if (kind === PS_NOTICE_YIELD_CANCELLED) {
      this.paused = null;
    }
  }

  connect(endpoint: string): ClientAnswer {
    this.end = this.net.dial(this.pid, endpoint, this);
    this.net.pump();
    return this.end === null ? { lost: "simulated connection refused" } : { ok: true };
  }

  private gone(): boolean {
    return this.end === null || this.end.closed || this.end.eof;
  }

  send(frame: Uint8Array, reply: boolean): void {
    const end = this.end;
    if (end === null || this.gone()) {
      this.answer = { lost: "the owner closed the connection", unsent: true };
      return;
    }
    const written = this.net.write(end, frame);
    this.net.pump();
    if (!written) this.answer = { lost: "the owner closed the connection", unsent: true };
    else this.answer = reply ? null : { ok: true };
  }

  wait(): ClientAnswer {
    const answer = this.answer;
    if (answer !== null) {
      this.answer = null;
      return answer;
    }
    return this.next();
  }

  // next takes the next queued frame. With none queued the connection must have ended: the owner
  // answers every request before the request's send returns, so a wait with nothing to read and the
  // connection up would never end (a violation of the simulation's synchronous model).
  next(): ClientAnswer {
    const frame = this.inbox.shift();
    if (frame !== undefined) return { frame };
    if (this.gone()) return { lost: "the owner closed the connection" };
    throw new Error("simulated page service: the owner did not answer a request synchronously");
  }

  disconnect(): void {
    if (this.end !== null) this.net.close(this.end);
    this.net.pump();
  }

  close(): void {
    this.disconnect();
  }

  lost(): boolean {
    return this.end === null || this.end.closed || (this.end.eof && this.inbox.length === 0);
  }

  waitForLoss(_deadlineMs: number): boolean {
    return this.lost();
  }

  pausedBy(): bigint | null {
    return this.paused;
  }

  lostAtMs(): number {
    return 0;
  }

  // onLost: a simulated client never re-elects in the background (simulation-testing.md §14 S8).
  onLost(_handler: () => void): void {}
}

// SimServed is a simulated owner's service: the shared connection protocol on the engine thread. Its
// page reads go through a read handle on the file host, as the Go owner's do; a client commit's pages
// go through the owner's pager — the same reservation, writes, and single durable barrier as the Go
// owner's — and after each client commit it hands the gate to the engine thread at once, which adopts
// the commit and runs a maintenance commit if one is due, before the next frame is read.
class SimServed implements ServedDb, OwnerLink {
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
  readonly connections = new Set<Connection>();
  readonly gateWaits = false;
  stopped = false;
  private readonly net: SimNet;
  private readonly args: OwnerServeArgs;
  private readonly storage: Engine;
  private readonly files: FileHost;
  private readonly listener: SimListener;
  private file: BlockStore;
  private fileGeneration: number;

  constructor(net: SimNet, pid: number, args: OwnerServeArgs) {
    this.net = net;
    this.args = args;
    this.state = args.state;
    this.pageSize = args.pageSize;
    this.maxReadPages = psMaxReadPages(args.pageSize);
    this.formatVersion = args.formatVersion;
    this.incarnation = args.incarnation;
    this.ownerReadOnly = args.ownerReadOnly;
    this.preferYield = args.preferYield;
    this.idleTimeoutMs = args.idleTimeoutMs;
    this.idleSessionTimeoutMs = args.idleSessionTimeoutMs;
    this.maxConnections = args.maxConnections;
    this.storage = args.storage;
    const files = args.storage.fileHost;
    if (files === null) throw engineError("io_error", "a simulated owner needs a file host");
    this.files = files;
    this.file = files.openStore(args.dbPath, true, true);
    this.fileGeneration = this.state.fileGeneration();
    this.listener = net.listen(pid, args.endpoint, (end) => this.accept(end));
  }

  private accept(end: SimEnd): void {
    const net = this.net;
    const socket: ConnSocket = {
      write: (frame) => void net.write(end, frame),
      end: (frame) => {
        net.write(end, frame);
        net.close(end);
      },
      destroy: () => net.close(end),
    };
    const connection = new Connection(this, socket);
    this.connections.add(connection);
    end.onData = (frame) => connection.receive(frame);
    end.onClose = () => connection.closed();
  }

  // readHandle is the file read handle, reopened after compaction replaced the file.
  private readHandle(): BlockStore {
    const generation = this.state.fileGeneration();
    if (generation !== this.fileGeneration) {
      this.file = this.files.openStore(this.args.dbPath, true, true);
      this.fileGeneration = generation;
    }
    return this.file;
  }

  readPage(index: number): Uint8Array {
    return this.readHandle().readAt(index * this.pageSize, this.pageSize);
  }

  writeDelta(w: WriteTxn, pages: PsIndexedPage[], pageCount: number): void {
    try {
      const paging = this.storage.paging;
      if (paging === null) throw engineError("io_error", "database storage is closed");
      if (!w.started) {
        paging.beginDurableCommit();
        w.started = true;
        paging.reserve(pageCount);
      }
      for (const pg of pages) {
        paging.writeBlock(pg.index, pg.page);
        paging.invalidate(pg.index);
      }
    } catch (error) {
      this.stopServing();
      throw error;
    }
  }

  abandonDelta(_w: WriteTxn): void {
    this.storage.paging?.abandonDurableCommit();
  }

  commitDelta(d: Delta): boolean {
    try {
      const paging = this.storage.paging;
      if (paging === null) return false;
      paging.writeBlock(Number(d.txid & 1n), d.meta);
      paging.sync();
      paging.finishDurableCommit(d.txid, metaChecksum(d.meta));
      paging.cacheValidation(d.meta, new Set(d.written));
      return true;
    } catch {
      return false;
    }
  }

  // maintenanceDue hands every client commit to the engine thread, which adopts it and decides the
  // maintenance commit on its own exact facts (the Go owner's order).
  maintenanceDue(_computed: boolean): boolean {
    return true;
  }

  maintain(): void {
    this.args.onMaintain();
  }

  localOldest(): bigint | null {
    return this.args.localOldest();
  }

  yieldAccepted(): void {
    this.args.onYield();
  }

  stopServing(): void {
    this.state.poison();
    this.stopped = true;
    this.net.closeListener(this.listener);
    for (const connection of this.connections) connection.destroy();
  }

  broadcast(kind: number, connectionId: bigint): void {
    for (const connection of this.connections) connection.notice(kind, connectionId);
  }

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
  }

  // --- OwnerLink ---

  stop(): void {
    this.stopped = true;
    this.net.closeListener(this.listener);
    for (const connection of this.connections) connection.destroy();
    this.net.pump();
  }

  poison(): void {
    this.stopServing();
    this.net.pump();
  }

  notice(kind: number): void {
    this.broadcast(kind, 0n);
    this.net.pump();
  }
}
