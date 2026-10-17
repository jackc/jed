// The deterministic-simulation lock host (spec/design/simulation-testing.md §14 S8), a port of the
// Go runner's sim_locks_test.go: simulated processes sharing one table of whole-file locks,
// standing in for the OS host (coordinator.ts osLocks). A process that dies loses its locks at
// once, as the kernel releases a dead process's locks. A contended acquisition never blocks the
// single simulation thread: each poll advances the simulated clock, so deadlines expire in
// simulated time. Each process's host also reaches the page service's owner records, endpoints, and
// connections on the shared simulated network (net.ts).

import type { LockFiles, LockHandle, LockHost } from "../lockhost.ts";
import type { PsOwnerRecord } from "../pageservice/codec.ts";
import type { SimNet } from "./net.ts";

const LOCK_POLL = 2000n; // microseconds of simulated time per poll of a contended lock
const LOCK_MAX_POLLS = 100000; // polls without success before a wait is reported as a deadlock

// SimDeadlock is a wait that never ends: a runner violation, not an engine error.
export class SimDeadlock extends Error {}

// SimLockTable is the kernel's view of every bundle lock: lock name → holding process → exclusive.
export class SimLockTable {
  holders = new Map<string, Map<number, boolean>>();
  polls = 0;
  private clock: { get(): bigint; advance(micros: bigint): void };

  constructor(clock: { get(): bigint; advance(micros: bigint): void }) {
    this.clock = clock;
  }

  // release drops every lock pid holds: the process is gone.
  release(pid: number): void {
    for (const h of this.holders.values()) h.delete(pid);
  }

  nowMicros(): number {
    return Number(this.clock.get());
  }

  advance(): void {
    this.clock.advance(LOCK_POLL);
  }
}

// SimLockHost is one simulated process's LockHost.
export class SimLockHost implements LockHost {
  private table: SimLockTable;
  private pidNum: number;
  private claimed = new Set<string>(); // the process-local open registry
  readonly pageNet: SimNet;

  constructor(table: SimLockTable, pid: number, net: SimNet) {
    this.table = table;
    this.pidNum = pid;
    this.pageNet = net;
  }

  claimPath(path: string): boolean {
    if (this.claimed.has(path)) return false;
    this.claimed.add(path);
    return true;
  }

  releasePath(path: string): void {
    this.claimed.delete(path);
  }

  openBundle(_path: string): LockFiles {
    return { presence: "presence", arrival: "arrival" };
  }

  validateMarker(_path: string): void {}

  closeFiles(_files: LockFiles): void {
    for (const holders of this.table.holders.values()) holders.delete(this.pidNum);
  }

  // tryLock takes or converts this process's lock on the named file if no other process conflicts:
  // an exclusive lock needs no other holder, a shared one no other exclusive holder.
  tryLock(handle: LockHandle, exclusive: boolean): boolean {
    const name = handle as string;
    let holders = this.table.holders.get(name);
    if (holders === undefined) {
      holders = new Map();
      this.table.holders.set(name, holders);
    }
    for (const [pid, ex] of holders) {
      if (pid !== this.pidNum && (exclusive || ex)) return false;
    }
    holders.set(this.pidNum, exclusive);
    this.table.polls = 0;
    return true;
  }

  unlock(handle: LockHandle): void {
    this.table.holders.get(handle as string)?.delete(this.pidNum);
  }

  nowMicros(): number {
    return this.table.nowMicros();
  }

  pause(): void {
    const t = this.table;
    t.polls++;
    if (t.polls > LOCK_MAX_POLLS) {
      throw new SimDeadlock(
        `simulated deadlock: process p${this.pidNum} waited ${t.polls} polls for a lock`,
      );
    }
    t.advance();
  }

  pid(): number {
    return this.pidNum;
  }

  // --- the page service's host operations, on the simulated network ---

  readOwnerRecord(dbPath: string): PsOwnerRecord | null {
    return this.pageNet.records.get(dbPath) ?? null;
  }

  writeOwnerRecord(dbPath: string, record: PsOwnerRecord): void {
    this.pageNet.records.set(dbPath, record);
  }

  removeOwnerRecord(dbPath: string): void {
    this.pageNet.records.delete(dbPath);
  }

  endpointPath(_dbPath: string, _uid: number, incarnation: Uint8Array): string {
    let hex = "";
    for (const b of incarnation.subarray(0, 8)) hex += b.toString(16).padStart(2, "0");
    return "sim-endpoint-" + hex;
  }

  removeEndpoint(endpoint: string): void {
    this.pageNet.removeEndpoint(endpoint);
  }
}
