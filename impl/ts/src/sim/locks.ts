// The deterministic-simulation lock host (spec/design/simulation-testing.md §14 S5), a port of the
// Go runner's sim_locks_test.go: simulated processes sharing one table of whole-file locks, standing
// in for the OS host (coordinator.ts osLocks). A process that dies loses its locks at once, as the
// kernel releases a dead process's locks. A contended acquisition never blocks the single simulation
// thread: each poll runs the other processes' presence probes and advances the simulated clock, so
// the holder can react and deadlines expire in simulated time.

import type { LockFiles, LockHandle, LockHost } from "../lockhost.ts";

const LOCK_POLL = 2000n; // microseconds of simulated time per poll of a contended lock
const LOCK_MAX_POLLS = 100000; // polls without success before a wait is reported as a deadlock

// SimDeadlock is a wait that never ends: a runner violation, not an engine error.
export class SimDeadlock extends Error {}

// SimLockTable is the kernel's view of every bundle lock: lock name → holding process → exclusive.
export class SimLockTable {
  holders = new Map<string, Map<number, boolean>>();
  probes = new Map<number, () => void>(); // each live process's presence probe
  polls = 0;
  private clock: { get(): bigint; advance(micros: bigint): void };
  private busy: (pid: number) => boolean; // whether a process's probe would block (writer gate held)

  constructor(
    clock: { get(): bigint; advance(micros: bigint): void },
    busy: (pid: number) => boolean,
  ) {
    this.clock = clock;
    this.busy = busy;
  }

  // release drops every lock pid holds and forgets its probe: the process is gone.
  release(pid: number): void {
    for (const h of this.holders.values()) h.delete(pid);
    this.probes.delete(pid);
  }

  // runProbes runs every live process's probe except skip's, in process order.
  runProbes(skip: number): void {
    const pids = [...this.probes.keys()].sort((a, b) => a - b);
    for (const pid of pids) {
      const probe = this.probes.get(pid);
      if (pid !== skip && probe !== undefined && !this.busy(pid)) probe();
    }
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

  constructor(table: SimLockTable, pid: number) {
    this.table = table;
    this.pidNum = pid;
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
    return {
      presence: "presence",
      arrival: "arrival",
      transition: "transition",
      writer: "writer",
      commit: "commit",
    };
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
    t.runProbes(this.pidNum);
    t.advance();
  }

  startProbe(tick: () => void): () => void {
    this.table.probes.set(this.pidNum, tick);
    // The runner forgets the probe when the process ends (SimLockTable.release), as Go's does.
    return () => {};
  }

  pid(): number {
    return this.pidNum;
  }
}
