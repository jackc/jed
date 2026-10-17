// The page-service owner, engine-thread half (spec/design/page-service.md §4.1, §14;
// spec/pageservice/README.md §6.2): it draws the incarnation, records the committed meta, writes the
// owner record, and asks the process's service worker (owner_worker.ts, one per process, reused across
// opens) to bind the endpoint and serve.
// Afterwards the engine thread touches the shared state only to publish a main-database commit, to
// read client pins into its reader watermark, and to reset the window at compaction — the owner's
// own sessions keep the alone fast path. Node-only.

import { randomFillSync } from "node:crypto";
import process from "node:process";

import { parseCommitMeta } from "../commit_manifest.ts";
import { engineError } from "../errors.ts";
import type { Engine } from "../executor.ts";
import { FORMAT_VERSION } from "../format.ts";
import type { SharedPaging } from "../paging.ts";
import type { ServiceOwnerHandle } from "../shared.ts";
import { type BlockingChannel, ProcessWorker } from "./blocking.ts";
import {
  currentUid,
  endpointPath,
  ownerRecordPath,
  removeQuietly,
  writeOwnerRecord,
} from "./endpoint.ts";
import type { OwnerServe, OwnerStop } from "./owner_worker.ts";
import { ServiceState } from "./shared_state.ts";

// The process's one service worker, shared by every database it owns and reused across opens.
const serviceWorker = new ProcessWorker(new URL("./owner_worker.ts", import.meta.url));
let nextServedId = 0;

function newIncarnation(): Uint8Array {
  const id = new Uint8Array(16);
  while (id.every((b) => b === 0)) randomFillSync(id);
  return id;
}

// readCommittedMeta reads the meta slot that published txid. Every commit writes slot txid & 1, so it
// is the one to read; the parse check guards a slot a later commit already overwrote.
function readCommittedMeta(paging: SharedPaging, txid: bigint): Uint8Array {
  const slot = Number(txid & 1n);
  for (const index of [slot, slot ^ 1]) {
    const block = paging.readBlock(index);
    const meta = parseCommitMeta(block, paging.pageSize(), paging.physicalPages());
    if (meta !== null && meta.txid === txid) return block;
  }
  throw engineError("data_corrupted", "no meta slot carries the committed txid");
}

export class ServiceOwner implements ServiceOwnerHandle {
  private readonly state = new ServiceState();
  private readonly dbPath: string;
  private readonly endpoint: string;
  // channel is the service worker's while this owner serves (id names it there); null otherwise.
  private channel: BlockingChannel | null = null;
  private readonly id = ++nextServedId;

  private constructor(dbPath: string, endpoint: string) {
    this.dbPath = dbPath;
    this.endpoint = endpoint;
  }

  // start makes the storage engine at dbPath a serving owner. The caller holds presence EX. An owner
  // that cannot pick a private endpoint directory still owns and records flags = 0.
  static start(storage: Engine, dbPath: string): ServiceOwner {
    const paging = storage.paging;
    if (paging === null) throw engineError("io_error", "database storage is closed");
    const incarnation = newIncarnation();
    const txid = storage.committed.txid;
    const uid = currentUid();
    const owner = new ServiceOwner(dbPath, endpointPath(dbPath, uid, incarnation));
    owner.state.resetLocked(txid, readCommittedMeta(paging, txid), false);
    writeOwnerRecord(dbPath, {
      serving: owner.endpoint !== "",
      uid,
      incarnation,
      pid: process.pid,
      endpoint: owner.endpoint,
    });
    if (owner.endpoint === "") return owner;
    // Only a presence-EX holder reaches here, so a socket already at the path is a dead owner's.
    removeQuietly(owner.endpoint);
    const channel = serviceWorker.acquire();
    const serve: OwnerServe = {
      op: "serve",
      id: owner.id,
      state: owner.state.buffer,
      dbPath,
      endpoint: owner.endpoint,
      pageSize: paging.pageSize(),
      formatVersion: FORMAT_VERSION,
      incarnation,
      ownerReadOnly: storage.readOnly,
    };
    const answer = channel.request(serve) as { ok?: boolean; error?: string };
    if (answer.ok !== true) {
      serviceWorker.release();
      removeQuietly(ownerRecordPath(dbPath));
      throw engineError("io_error", `bind page-service endpoint: ${answer.error ?? "failed"}`);
    }
    owner.channel = channel;
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
  published(txid: bigint, meta: Uint8Array, written: number[]): void {
    this.state.lock();
    try {
      this.state.publishLocked(txid, meta, written);
    } finally {
      this.state.unlock();
    }
  }

  // compactedLocked resets the window after compaction replaced the file at txid: every older cache is
  // invalid, and the worker reopens its read handle. The caller holds the lock (and no pin exists).
  compactedLocked(txid: bigint, paging: SharedPaging): void {
    this.state.resetLocked(txid, readCommittedMeta(paging, txid), true);
  }

  // close stops serving: every client sees EOF, then the endpoint and owner record are removed. The
  // caller releases presence afterwards.
  close(): void {
    if (this.channel !== null) {
      this.channel.request({ op: "stop", id: this.id } satisfies OwnerStop);
      this.channel = null;
      serviceWorker.release();
      removeQuietly(this.endpoint);
    }
    removeQuietly(ownerRecordPath(this.dbPath));
  }
}
