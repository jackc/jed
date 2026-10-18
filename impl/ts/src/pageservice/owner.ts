// The page-service owner, engine-thread half (spec/design/page-service.md §4.1, §14;
// spec/pageservice/README.md §6.2, §6.4): it draws the incarnation, records the committed meta, writes
// the owner record, and asks the process's service worker (owner_worker.ts, one per process, reused
// across opens) to bind the endpoint and serve — or, in the deterministic simulator, the lock host's
// in-memory network (net.ts), which serves on this thread.
// Afterwards the engine thread touches the shared state only to publish a main-database commit, to
// read client pins into its reader watermark, to reset the window at compaction, to take the writer
// gate, and — when a client commit was published or the gate handed over, which one atomic load at each
// transaction begin notices — to adopt client commits and run maintenance commits. The owner's own
// sessions otherwise run as on an unshared file. Node-only.

import process from "node:process";
import { MessageChannel, type MessagePort } from "node:worker_threads";

import { parseCommitMeta } from "../commit_manifest.ts";
import { engineError } from "../errors.ts";
import type { Engine } from "../executor.ts";
import { FORMAT_VERSION } from "../format.ts";
import type { LockHost } from "../lockhost.ts";
import type { SharedPaging } from "../paging.ts";
import type { ServiceEngineFacts, ServiceOwnerHandle, ServicePending } from "../shared.ts";
import { type BlockingChannel, ProcessWorker } from "./blocking.ts";
import { currentUid, newIncarnation } from "./endpoint.ts";
import { PS_NOTICE_SHUTDOWN_BEGUN } from "./codec.ts";
import type { OwnerLink, OwnerServeArgs } from "./net.ts";
import type { OwnerNotice, OwnerPoison, OwnerServe, OwnerStop } from "./owner_worker.ts";
import {
  ACK_CRASH,
  ACK_HANG_UP,
  GATE_ENGINE,
  GATE_FREE,
  GATE_HANDED,
  GATE_WORKER,
  OWNER_CLOSING,
  OWNER_RELEASING,
  OWNER_SERVING,
  OWNER_YIELDING,
  ServiceState,
} from "./shared_state.ts";

// The process's one service worker, shared by every database it owns and reused across opens.
const serviceWorker = new ProcessWorker(new URL("./owner_worker.ts", import.meta.url));
let nextServedId = 0;

// DEFAULT_IDLE_IN_TRANSACTION_TIMEOUT_MS is the owner's idle-in-transaction timeout for client write
// transactions (page-service.md §11.1).
export const DEFAULT_IDLE_IN_TRANSACTION_TIMEOUT_MS = 60000;

// OwnerLimits are an owner's limits and preference (page-service.md §8.2, §11.1; README §6.5), read
// from its own handle's options.
export type OwnerLimits = {
  idleInTransactionTimeoutMs: number;
  idleSessionTimeoutMs: number;
  maxConnections: number;
  preferYield: boolean;
};

// readCommittedMeta reads the meta slot that published txid. A commit writes slot txid & 1 unless a
// relaxed batch put it in the other slot (format.md, *Slot alternation*), so both are tried; the parse
// check guards a slot a later commit already overwrote.
function readCommittedMeta(paging: SharedPaging, txid: bigint): Uint8Array {
  const slot = Number(txid & 1n);
  for (const index of [slot, slot ^ 1]) {
    const block = paging.readBlock(index);
    const meta = parseCommitMeta(block, paging.pageSize(), paging.physicalPages());
    if (meta !== null && meta.txid === txid) return block;
  }
  throw engineError("data_corrupted", "no meta slot carries the committed txid");
}

// WorkerOwnerLink serves one database on the process's service worker over an OS socket.
class WorkerOwnerLink implements OwnerLink {
  readonly gateWaits = true;
  private readonly channel: BlockingChannel;
  private readonly id = ++nextServedId;
  private readonly notify: MessagePort;

  // The worker binds the endpoint and serves; a bind failure throws.
  constructor(args: OwnerServeArgs) {
    const channel = serviceWorker.acquire();
    const { port1, port2 } = new MessageChannel();
    const serve: OwnerServe = {
      op: "serve",
      id: this.id,
      state: args.state.buffer,
      dbPath: args.dbPath,
      endpoint: args.endpoint,
      pageSize: args.pageSize,
      formatVersion: args.formatVersion,
      incarnation: args.incarnation,
      ownerReadOnly: args.ownerReadOnly,
      preferYield: args.preferYield,
      noSync: args.noSync,
      idleTimeoutMs: args.idleTimeoutMs,
      idleSessionTimeoutMs: args.idleSessionTimeoutMs,
      maxConnections: args.maxConnections,
      notify: port2,
    };
    const answer = channel.request(serve, [port2]) as { ok?: boolean; error?: string };
    if (answer.ok !== true) {
      port1.close();
      serviceWorker.release();
      throw engineError("io_error", `bind page-service endpoint: ${answer.error ?? "failed"}`);
    }
    // A gate handed over between transactions is taken at the next event-loop turn, and an accepted
    // ownership request starts the drain there; the port never keeps the process alive.
    port1.on("message", (message: string) => {
      if (message === "maintain") args.onMaintain();
      else if (message === "yield") args.onYield();
    });
    port1.unref();
    this.channel = channel;
    this.notify = port1;
  }

  stop(): void {
    this.channel.request({ op: "stop", id: this.id } satisfies OwnerStop);
    serviceWorker.release();
    // A closed port stays a GC root until the event loop next turns, and its listener's closure holds
    // the whole storage engine: a host that opens and closes in one synchronous run would keep every
    // engine it closed. Detach the listener first so the engine is collectable at once.
    this.notify.removeAllListeners();
    this.notify.close();
  }

  poison(): void {
    this.channel.request({ op: "poison", id: this.id } satisfies OwnerPoison);
  }

  notice(kind: number): void {
    this.channel.request({ op: "notice", id: this.id, kind } satisfies OwnerNotice);
  }
}

export class ServiceOwner implements ServiceOwnerHandle {
  private readonly state = new ServiceState();
  private readonly dbPath: string;
  private readonly endpoint: string;
  private readonly host: LockHost;
  // link is the service serving this database; null for an owner that could not bind, or after close.
  private link: OwnerLink | null = null;
  // handoff is the core's handler for a gate handed over for a maintenance commit.
  private handoff: (() => void) | null = null;
  // yieldRequested is the core's handler for an accepted request_ownership (README §6.5).
  private yieldRequested: (() => void) | null = null;
  // localOldest is the core's oldest local pin, for a same-thread service (the simulator).
  private localOldest: (() => bigint | null) | null = null;

  private constructor(dbPath: string, endpoint: string, host: LockHost) {
    this.dbPath = dbPath;
    this.endpoint = endpoint;
    this.host = host;
  }

  // start makes the storage engine at dbPath a serving owner through host. The caller holds presence
  // EX. An owner that cannot pick a private endpoint directory still owns and records flags = 0.
  static start(storage: Engine, dbPath: string, limits: OwnerLimits, host: LockHost): ServiceOwner {
    const paging = storage.paging;
    if (paging === null) throw engineError("io_error", "database storage is closed");
    const incarnation = newIncarnation();
    const txid = storage.committed.txid;
    const uid = currentUid();
    const owner = new ServiceOwner(dbPath, host.endpointPath(dbPath, uid, incarnation), host);
    const meta = readCommittedMeta(paging, txid);
    // The resolution window starts with the recovered meta slots (README §4): the committed one and,
    // when the other slot holds the commit just before it, that one too.
    if (txid > 0n) {
      for (let slot = 0; slot < 2; slot++) {
        const other = paging.readBlock(slot);
        const parsed = parseCommitMeta(other, paging.pageSize(), paging.physicalPages());
        if (parsed !== null && parsed.txid === txid - 1n) {
          owner.state.recordResolutionLocked(parsed.txid, other);
          break;
        }
      }
    }
    owner.state.resetLocked(txid, meta, false);
    // The service sees the owner's batched-durability state (validated-cow.md §4): it flushes a pending
    // batch before granting a begin_write and writes a client commit's meta to the non-floor slot.
    paging.setBatchMirror(owner.state);
    owner.state.setEngineFactsLocked(storage.freeGenTxid, storage.liveAtCompaction, null);
    host.writeOwnerRecord(dbPath, {
      serving: owner.endpoint !== "",
      uid,
      incarnation,
      pid: process.pid,
      endpoint: owner.endpoint,
    });
    if (owner.endpoint === "") return owner;
    // Only a presence-EX holder reaches here, so a socket already at the path is a dead owner's.
    host.removeEndpoint(owner.endpoint);
    const args: OwnerServeArgs = {
      state: owner.state,
      storage,
      dbPath,
      endpoint: owner.endpoint,
      pageSize: paging.pageSize(),
      formatVersion: FORMAT_VERSION,
      incarnation,
      ownerReadOnly: storage.readOnly,
      preferYield: limits.preferYield,
      noSync: paging.skipsSync(),
      idleTimeoutMs: limits.idleInTransactionTimeoutMs,
      idleSessionTimeoutMs: limits.idleSessionTimeoutMs,
      maxConnections: limits.maxConnections,
      onMaintain: () => owner.handoff?.(),
      onYield: () => owner.yieldRequested?.(),
      localOldest: () => owner.localOldest?.() ?? null,
    };
    try {
      owner.link =
        host.pageNet !== undefined
          ? host.pageNet.serve(host.pid(), args)
          : new WorkerOwnerLink(args);
    } catch (error) {
      host.removeOwnerRecord(dbPath);
      throw error;
    }
    return owner;
  }

  lock(): void {
    this.state.lock();
  }

  unlock(): void {
    this.state.unlock();
  }

  oldestPinnedLocked(): bigint | null {
    return this.state.oldestPinnedLocked();
  }

  // published records a main-database commit with its root swap (README §4, §6.2).
  published(txid: bigint, meta: Uint8Array, written: number[], facts: ServiceEngineFacts): void {
    this.state.lock();
    try {
      this.state.publishLocked(txid, meta, written);
      this.state.setEngineFactsLocked(facts.freeGenTxid, facts.liveAtCompaction, facts.localOldest);
    } finally {
      this.state.unlock();
    }
  }

  // compactedLocked resets the window after compaction replaced the file at txid: every older cache is
  // invalid, and the worker reopens its file handle. The caller holds the lock (and no pin exists).
  compactedLocked(txid: bigint, paging: SharedPaging, facts: ServiceEngineFacts): void {
    this.state.resetLocked(txid, readCommittedMeta(paging, txid), true);
    paging.setBatchMirror(this.state);
    this.state.setEngineFactsLocked(facts.freeGenTxid, facts.liveAtCompaction, facts.localOldest);
  }

  epoch(): number {
    return this.state.epoch();
  }

  // pending reports the client commits published after since: the newest txid and meta, and the pages
  // written in between.
  pending(since: bigint): ServicePending {
    this.state.lock();
    try {
      const txid = this.state.committedTxidLocked();
      if (txid <= since) return { txid, meta: new Uint8Array(0), all: false, indices: [] };
      const inv = this.state.invalidationLocked(true, since, txid);
      return { txid, meta: this.state.metaLocked(), all: inv.all, indices: inv.indices };
    } finally {
      this.state.unlock();
    }
  }

  // acquireGate waits in Atomics.wait for the writer gate (README §4): free, or handed over by the
  // worker for a maintenance commit (returns true). timeoutMs 0 waits without a deadline; on expiry
  // 55P03.
  acquireGate(timeoutMs: number): boolean {
    const deadline = timeoutMs > 0 ? Date.now() + timeoutMs : Number.POSITIVE_INFINITY;
    for (;;) {
      if (this.state.tryGate(GATE_ENGINE)) return false;
      const seen = this.state.gate();
      if (seen === GATE_HANDED && this.state.takeHanded()) return true;
      if (seen === GATE_FREE || seen === GATE_HANDED) continue;
      const remaining = this.link === null || this.link.gateWaits ? deadline - Date.now() : 0;
      if (remaining <= 0) {
        throw engineError("lock_not_available", "could not obtain the database writer lock");
      }
      this.state.waitGate(seen, remaining);
    }
  }

  tryGate(): boolean {
    return this.state.tryGate(GATE_ENGINE);
  }

  takeHanded(): boolean {
    return this.state.takeHanded();
  }

  releaseGate(): void {
    this.state.releaseGate();
  }

  poisoned(): boolean {
    return this.state.poisoned();
  }

  // stopServing poisons the owner after its own handle failed a maintenance commit: the worker closes
  // every connection and accepts no more.
  stopServing(): void {
    this.state.poison();
    this.link?.poison();
  }

  clearLocalOldest(): void {
    this.state.lock();
    this.state.clearEngineOldestLocked();
    this.state.unlock();
  }

  onHandoff(handler: () => void): void {
    this.handoff = handler;
  }

  onYield(handler: () => void): void {
    this.yieldRequested = handler;
  }

  onLocalOldest(oldest: () => bigint | null): void {
    this.localOldest = oldest;
  }

  hangUpNextAck(): void {
    this.state.armAckHook(ACK_HANG_UP);
  }

  crashBeforeNextAck(): void {
    this.state.armAckHook(ACK_CRASH);
  }

  // resolve answers commit_status from the resolution window (README §3.5). A client commit is
  // published by the service worker within one event-loop turn, so none is half published.
  resolve(txid: bigint, digest: bigint): number {
    this.state.lock();
    try {
      return this.state.resolveLocked(txid, digest);
    } finally {
      this.state.unlock();
    }
  }

  // --- handoff (README §6.5) ---

  // serving reports whether a service serves this database (false for an owner that could not bind,
  // or after close).
  serving(): boolean {
    return this.link !== null;
  }

  // admitting reports whether the owner serves normally; otherwise its own begins wait for the drain.
  admitting(): boolean {
    return this.state.ownerState() === OWNER_SERVING;
  }

  yielding(): boolean {
    return this.state.ownerState() === OWNER_YIELDING;
  }

  // beginRelease ends a yield's drain: true when the owner was still yielding (now releasing), false
  // when the requester disconnected first and the yield was cancelled.
  beginRelease(): boolean {
    this.state.lock();
    try {
      if (this.state.ownerState() !== OWNER_YIELDING) return false;
      this.state.setOwnerStateLocked(OWNER_RELEASING);
      return true;
    } finally {
      this.state.unlock();
    }
  }

  // beginClosing starts a graceful close: no more admissions or ownership requests, and notice kind 3
  // to every connection.
  beginClosing(): void {
    this.state.lock();
    this.state.setOwnerStateLocked(OWNER_CLOSING);
    this.state.unlock();
    this.link?.notice(PS_NOTICE_SHUTDOWN_BEGUN);
  }

  // clientsIdle reports whether no client transaction is running: no connection pins a snapshot and
  // none holds the writer gate. With own set, the engine thread's writer (or a handed gate) counts too;
  // its readers are the core's to check.
  clientsIdle(own: boolean): boolean {
    this.state.lock();
    const pinned = this.state.oldestPinnedLocked() !== null;
    this.state.unlock();
    if (pinned) return false;
    const gate = this.state.gate();
    if (gate === GATE_WORKER) return false;
    return !own || gate === GATE_FREE;
  }

  // close stops serving: every client sees EOF, then the endpoint and owner record are removed. The
  // caller releases presence afterwards.
  close(): void {
    if (this.link !== null) {
      this.link.stop();
      this.link = null;
      this.host.removeEndpoint(this.endpoint);
    }
    this.handoff = null;
    this.yieldRequested = null;
    this.localOldest = null;
    this.host.removeOwnerRecord(this.dbPath);
  }
}
