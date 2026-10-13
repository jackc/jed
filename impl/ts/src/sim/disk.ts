// The deterministic-simulation disk (spec/design/simulation-testing.md §6), a port of the Go
// runner's simDisk (impl/go/sim_disk_test.go): a BlockStore whose durability, crashes, and I/O
// errors are decided by a seeded, content-keyed function, so a whole multi-commit history replays
// exactly from its seed and a crash image is identical in every core.
//
// State (§6.1): durable bytes survive a power loss unconditionally; pending holds, per byte offset,
// every version written since that offset was last covered by a barrier; cache is what reads see
// (durable overlaid with the newest pending version). sync and a growing setSize are barriers.

import type { BlockStore } from "../blockstore.ts";
import { engineError, type EngineError } from "../errors.ts";
import { decide, Fnv64 } from "./util.ts";

// Decision lanes (§6.2); the values are part of the cross-core contract.
const LANE_SURVIVE = 1n; // power loss: does anything land at this offset?
const LANE_VERSION = 2n; // power loss: which pending version lands
const LANE_TEAR = 3n; // power loss: is the landing version torn?
const LANE_SECTOR = 4n; // power loss: per-sector old/new choice of a torn version
const LANE_IO_WRITE = 5n; // io_error op=write: does this write fail?
const LANE_ISSUE = 6n; // process_crash: was this write issued before the process died?
const LANE_LAND = 7n; // io_error op=sync: does this pending write land or vanish (fsyncgate)?
export const LANE_SESSION = 8n; // a session's entropy-seam seed
const LANE_READ = 9n; // read_error: do reads of this offset fail during the step?

// read_error and enospc are per-core faults: which reads happen depends on buffer-pool policy, and
// when the file grows on preallocation policy (simulation-testing.md §6.3).
export type SimFaultKind =
  | "power_loss"
  | "process_crash"
  | "io_write"
  | "io_sync"
  | "read_error"
  | "enospc";

// SimFault is one armed fault (simulation-testing.md §6.3). Permille fields are 0..1000.
export type SimFault = {
  kind: SimFaultKind;
  survive: number; // power_loss
  tear: number; // power_loss
  issue: number; // process_crash
  p: number; // io_error op=write, read_error
  nth: number; // io_error op=sync
};

type SimWrite = { off: number; version: number };

function injected(what: string): EngineError {
  return engineError("io_error", "I/O error: simulated " + what);
}

function grow(buf: Uint8Array, size: number): Uint8Array {
  if (size <= buf.length) return buf;
  const next = new Uint8Array(size);
  next.set(buf);
  return next;
}

// SimDisk is the simulated device. It outlives every Database opened over it: a crash discards the
// process (the Database) but keeps the disk, as a real file would.
export class SimDisk implements BlockStore {
  private seed: bigint;
  private sector: number;
  private durable: Uint8Array;
  cache: Uint8Array;
  private pending = new Map<number, Uint8Array[]>();

  // Per-step state, reset by beginStep.
  private step = 0n;
  private fault: SimFault | null = null;
  fired = ""; // "" | "barrier" | "write" | "sync" | "read" | "enospc"
  private dead = false; // the process died mid-step; every later call fails
  private syncs = 0;
  writes = 0;
  barrierOK = false; // a barrier completed this step
  oracle = false; // the runner's own oracle reads are in progress: read faults do not apply

  private ops = new Fnv64(); // FNV-1a fold of every mutating call, for self-determinism (§8.5)

  // unissued are the writes an armed process_crash decided never reached the kernel; processDeath
  // removes them.
  private unissued: SimWrite[] = [];

  constructor(seed: bigint, sector: number, image: Uint8Array) {
    this.seed = seed;
    this.sector = sector;
    this.durable = image.slice();
    this.cache = image.slice();
  }

  opsHex(): string {
    return this.ops.hex();
  }

  // beginStep resets the per-step state and arms f (null for an unfaulted step).
  beginStep(step: number, f: SimFault | null): void {
    this.step = BigInt(step);
    this.fault = f;
    this.fired = "";
    this.dead = false;
    this.syncs = 0;
    this.writes = 0;
    this.barrierOK = false;
    this.unissued = [];
  }

  private fold(kind: number, off: number, p: Uint8Array | null): void {
    const buf = new Uint8Array(17);
    const dv = new DataView(buf.buffer);
    dv.setBigUint64(0, this.ops.value(), false);
    buf[8] = kind;
    dv.setBigUint64(9, BigInt.asUintN(64, BigInt(off)), false);
    const h = new Fnv64();
    h.update(buf);
    if (p !== null) h.update(p);
    this.ops = h;
  }

  readAt(offset: number, len: number): Uint8Array {
    const f = this.fault;
    if (
      f !== null &&
      f.kind === "read_error" &&
      !this.dead &&
      !this.oracle &&
      decide(this.seed, LANE_READ, this.step, BigInt(offset), 0n) % 1000n < BigInt(f.p)
    ) {
      this.fired = "read";
      throw injected("read error");
    }
    if (offset < 0 || offset + len > this.cache.length) {
      throw engineError("io_error", "I/O error: short read past the end of the simulated disk");
    }
    return this.cache.slice(offset, offset + len);
  }

  writeAt(offset: number, bytes: Uint8Array): void {
    this.fold(0x77, offset, bytes); // 'w'
    if (this.dead) throw injected("write after process death");
    const version = this.pending.get(offset)?.length ?? 0;
    const f = this.fault;
    if (f !== null) {
      if (f.kind === "process_crash") {
        // The live process sees all its own writes; only at its death does a write that was never
        // issued to the kernel disappear (processDeath).
        const d = decide(this.seed, LANE_ISSUE, this.step, BigInt(offset), BigInt(version));
        if (d % 1000n >= BigInt(f.issue)) this.unissued.push({ off: offset, version });
      } else if (f.kind === "io_write") {
        if (
          this.fired === "" &&
          decide(this.seed, LANE_IO_WRITE, this.step, BigInt(offset), BigInt(version)) % 1000n <
            BigInt(f.p)
        ) {
          this.fired = "write";
          throw injected("write error");
        }
      }
    }
    this.writes++;
    const end = offset + bytes.length;
    this.cache = grow(this.cache, end);
    this.cache.set(bytes, offset);
    const vs = this.pending.get(offset);
    if (vs === undefined) this.pending.set(offset, [bytes.slice()]);
    else vs.push(bytes.slice());
  }

  // sortedPending returns the pending offsets in ascending order: no map order may leak.
  private sortedPending(): number[] {
    return [...this.pending.keys()].sort((a, b) => a - b);
  }

  private putDurable(off: number, p: Uint8Array): void {
    this.durable = grow(this.durable, off + p.length);
    this.durable.set(p, off);
  }

  // flush makes every pending write durable: a successful barrier.
  private flush(): void {
    for (const off of this.sortedPending()) {
      const vs = this.pending.get(off)!;
      this.putDurable(off, vs[vs.length - 1]!);
    }
    this.durable = grow(this.durable, this.cache.length);
    this.pending = new Map();
    this.barrierOK = true;
    this.unissued = []; // reaching a barrier means every earlier write was issued
  }

  sync(): void {
    this.fold(0x73, 0, null); // 's'
    if (this.dead) throw injected("sync after process death");
    this.syncs++;
    const f = this.fault;
    if (f !== null && this.fired === "") {
      if (f.kind === "power_loss" || f.kind === "process_crash") {
        // The barrier never completes: the machine or process is gone.
        this.fired = "barrier";
        this.dead = true;
        throw injected("crash at a durability barrier");
      }
      if (f.kind === "io_sync" && this.syncs === f.nth) {
        this.fired = "sync";
        this.fsyncgate();
        throw injected("sync error");
      }
    }
    this.flush();
  }

  // fsyncgate models a failed fsync on Linux: each pending write independently became durable or
  // was dropped from the cache, and the error is reported once (§6.3).
  private fsyncgate(): void {
    for (const off of this.sortedPending()) {
      const vs = this.pending.get(off)!;
      const newest = vs[vs.length - 1]!;
      if (decide(this.seed, LANE_LAND, this.step, BigInt(off), BigInt(vs.length - 1)) % 2n === 0n) {
        this.putDurable(off, newest);
        continue;
      }
      const end = off + newest.length;
      for (let i = off; i < end; i++)
        this.cache[i] = i < this.durable.length ? this.durable[i]! : 0;
    }
    this.pending = new Map();
  }

  size(): number {
    return this.cache.length;
  }

  // setSize grows durably (zero blocks + a full fsync, which also flushes every pending write —
  // hosts.md §2.1) or truncates. It is never a fault point (simulation-testing.md §6.3).
  setSize(n: number): void {
    this.fold(0x7a, n, null); // 'z'
    if (this.dead) throw injected("set_size after process death");
    const cur = this.cache.length;
    const f = this.fault;
    if (f !== null && f.kind === "enospc" && n > cur && this.fired === "") {
      this.fired = "enospc";
      throw injected("no space left on device");
    }
    if (n > cur) {
      this.cache = grow(this.cache, n);
      this.flush();
    } else if (n < cur) {
      this.cache = this.cache.slice(0, n);
      if (this.durable.length > n) this.durable = this.durable.slice(0, n);
      for (const off of [...this.pending.keys()]) if (off >= n) this.pending.delete(off);
    }
  }

  close(): void {}

  // powerLoss replaces the device contents with what survives a power cut during step (§6.3): for
  // each pending offset, nothing lands or one pending version (any, not only the newest) lands,
  // possibly torn sector by sector against the durable bytes. The cache is discarded.
  powerLoss(step: number, survive: number, tear: number): void {
    const st = BigInt(step);
    for (const off of this.sortedPending()) {
      const vs = this.pending.get(off)!;
      const o = BigInt(off);
      if (decide(this.seed, LANE_SURVIVE, st, o, 0n) % 1000n >= BigInt(survive)) continue;
      const idx = decide(this.seed, LANE_VERSION, st, o, 0n) % BigInt(vs.length);
      const data = vs[Number(idx)]!.slice();
      if (decide(this.seed, LANE_TEAR, st, o, idx) % 1000n < BigInt(tear)) {
        for (let s = 0; s < data.length; s += this.sector) {
          if ((decide(this.seed, LANE_SECTOR, st, o + BigInt(s), idx) & 1n) === 1n) continue; // new bytes
          for (let i = s; i < s + this.sector && i < data.length; i++) {
            const at = off + i;
            data[i] = at < this.durable.length ? this.durable[at]! : 0;
          }
        }
      }
      this.putDurable(off, data);
    }
    this.pending = new Map();
    this.cache = this.durable.slice();
  }

  // processDeath ends the process: the kernel cache survives with every issued write still pending
  // (not durable), so a later power loss can still drop it (§6.3).
  processDeath(): void {
    // Undo the never-issued writes, newest first so each version index stays valid. A barrier that
    // completed after such a write already made it durable, so only still-pending versions go.
    for (let i = this.unissued.length - 1; i >= 0; i--) {
      const w = this.unissued[i]!;
      let vs = this.pending.get(w.off);
      if (vs === undefined || w.version >= vs.length) continue;
      const size = vs[w.version]!.length;
      vs = [...vs.slice(0, w.version), ...vs.slice(w.version + 1)];
      if (vs.length === 0) this.pending.delete(w.off);
      else this.pending.set(w.off, vs);
      for (let j = 0; j < size; j++) {
        const at = w.off + j;
        if (vs.length > 0) this.cache[at] = vs[vs.length - 1]![j]!;
        else if (at < this.durable.length) this.cache[at] = this.durable[at]!;
        else this.cache[at] = 0;
      }
    }
    this.unissued = [];
    this.fault = null;
    this.dead = false;
  }
}
