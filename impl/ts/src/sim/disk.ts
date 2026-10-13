// The deterministic-simulation filesystem (spec/design/simulation-testing.md §6), a port of the Go
// runner's simFS / simDisk (impl/go/sim_disk_test.go): named files, each a BlockStore whose
// durability, crashes, and I/O errors are decided by a seeded, content-keyed function, and a
// directory whose entries become durable only at a directory barrier. A whole multi-commit history
// replays exactly from its seed, and a crash image is identical in every core.
//
// A file's state (§6.1): durable bytes survive a power loss unconditionally; pending holds, per byte
// offset, every version written since that offset was last covered by a barrier; cache is what reads
// see (durable overlaid with the newest pending version). sync and a growing setSize are barriers.
//
// Every fault decision is decide(seed, lane, step, offset + file key, version) (§6.2): keyed by WHAT
// was written WHERE, never by a write's position in the op log, so cores whose write order differs
// within a commit still produce the same post-crash bytes.

import type { BlockStore } from "../blockstore.ts";
import { engineError, type EngineError } from "../errors.ts";
import type { FileHost, WriteAt } from "../filehost.ts";
import { compareUtf8, decide, Fnv64, utf8 } from "./util.ts";

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
const LANE_DIR = 10n; // power loss: how many pending directory changes survive (% changes+1)

const MASK64 = 0xffff_ffff_ffff_ffffn;

// The database file's name in the simulated filesystem — the path the engine is given.
export const MAIN_FILE = "sim.jed";

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
  nth: number; // power_loss/process_crash: the barrier it fires at (default 1); io_error op=sync: the sync
};

type SimWrite = { off: number; version: number };

// DirOp is one directory change: create a file, rename one over another, or remove one.
type DirOp = { kind: "c" | "r" | "d"; from: string; to: string; file: SimFile };

function injected(what: string): EngineError {
  return engineError("io_error", "I/O error: simulated " + what);
}

function grow(buf: Uint8Array, size: number): Uint8Array {
  if (size <= buf.length) return buf;
  const next = new Uint8Array(size);
  next.set(buf);
  return next;
}

// fileKey keeps two files' fault decisions independent (§6.2): the database file's is 0, so its
// decisions are unchanged by the existence of other files; another file's is its name's FNV-1a 32
// plus one, shifted above any offset (mod 2^64, as in Go).
function fileKey(name: string): bigint {
  if (name === MAIN_FILE) return 0n;
  let h = 0x811c9dc5;
  for (const b of utf8(name)) h = Math.imul(h ^ b, 0x01000193) >>> 0;
  return ((BigInt(h) + 1n) << 40n) & MASK64;
}

// SimFS is the simulated filesystem (§6.1, slice S4). It outlives every Database opened over it: a
// crash discards the process but keeps the disk, as a real filesystem would. The per-step fault state
// lives here, because a fault arms a step, not a file. It is also the FileHost the simulated
// database's compaction and directory barrier go through.
export class SimFS implements FileHost {
  readonly seed: bigint;
  readonly sector: number;

  files = new Map<string, SimFile>(); // the kernel's view of the directory
  private durable = new Map<string, SimFile>(); // the directory as of the last directory barrier
  dirOps: DirOp[] = []; // directory changes since that barrier, in order

  // Per-step state, reset by beginStep.
  step = 0n;
  fault: SimFault | null = null;
  fired = ""; // "" | "barrier" | "write" | "sync" | "read" | "enospc"
  dead = false; // the process died mid-step; every later call fails
  syncs = 0; // file syncs this step (io_error op=sync nth)
  private barriers = 0; // file and directory barriers this step (power_loss/process_crash nth)
  writes = 0;
  barrierOK = false; // the database file's barrier completed this step, its directory entry durable
  oracle = false; // the runner's own oracle reads are in progress: read faults do not apply

  private ops = new Fnv64(); // FNV-1a fold of every mutating call, for self-determinism (§8.5)

  constructor(seed: bigint, sector: number, image: Uint8Array) {
    this.seed = seed;
    this.sector = sector;
    const main = new SimFile(this, MAIN_FILE);
    main.durable = image.slice();
    main.cache = image.slice();
    this.files.set(MAIN_FILE, main);
    this.durable = new Map([[MAIN_FILE, main]]);
  }

  // main is the database file, or undefined if a crash lost it.
  main(): SimFile | undefined {
    return this.files.get(MAIN_FILE);
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
    this.barriers = 0;
    this.writes = 0;
    this.barrierOK = false;
    for (const d of this.allFiles()) d.unissued = [];
  }

  // allFiles lists every file the directory, its durable state, or a pending change refers to, each
  // once, in a deterministic order.
  private allFiles(): SimFile[] {
    const seen = new Set<SimFile>();
    const out: SimFile[] = [];
    const add = (d: SimFile | undefined): void => {
      if (d !== undefined && !seen.has(d)) {
        seen.add(d);
        out.push(d);
      }
    };
    for (const dir of [this.files, this.durable]) {
      for (const name of [...dir.keys()].sort(compareUtf8)) add(dir.get(name));
    }
    for (const op of this.dirOps) add(op.file);
    return out;
  }

  fold(kind: string, key: bigint, off: number, p: Uint8Array | null): void {
    const buf = new Uint8Array(25);
    const dv = new DataView(buf.buffer);
    dv.setBigUint64(0, this.ops.value(), false);
    buf[8] = kind.charCodeAt(0);
    dv.setBigUint64(9, key, false);
    dv.setBigUint64(17, BigInt.asUintN(64, BigInt(off)), false);
    const h = new Fnv64();
    h.update(buf);
    if (p !== null) h.update(p);
    this.ops = h;
  }

  // barrier counts one file or directory barrier and reports whether an armed power loss or process
  // crash fires at it: the barrier never completes, because the machine or process is gone.
  barrier(): boolean {
    this.barriers++;
    const f = this.fault;
    if (
      f === null ||
      this.fired !== "" ||
      (f.kind !== "power_loss" && f.kind !== "process_crash")
    ) {
      return false;
    }
    const nth = f.nth <= 0 ? 1 : f.nth;
    if (this.barriers !== nth) return false;
    this.fired = "barrier";
    this.dead = true;
    return true;
  }

  // powerLoss applies a power cut to the whole filesystem: every file loses what its own decisions
  // say, and the directory keeps its durable entries plus an ordered prefix of the pending changes —
  // a journaling filesystem applies metadata changes in order (§6.3).
  powerLoss(step: number, survive: number, tear: number): void {
    const st = BigInt(step);
    for (const d of this.allFiles()) d.powerLoss(st, survive, tear);
    const keep = Number(decide(this.seed, LANE_DIR, st, 0n, 0n) % BigInt(this.dirOps.length + 1));
    const dir = new Map(this.durable);
    for (const op of this.dirOps.slice(0, keep)) {
      if (op.kind === "c") dir.set(op.to, op.file);
      else if (op.kind === "r") {
        dir.set(op.to, op.file);
        dir.delete(op.from);
      } else dir.delete(op.from);
    }
    this.files = dir;
    this.durable = new Map(dir);
    this.dirOps = [];
  }

  // processDeath ends the process: the kernel cache and directory survive, every issued write still
  // pending (not durable), so a later power loss can still drop it (§6.3).
  processDeath(): void {
    for (const d of this.allFiles()) d.processDeath();
    this.fault = null;
    this.dead = false;
  }

  // --- the FileHost the simulated database's compaction goes through (filehost.ts) --------------

  realPath(path: string): string {
    return path;
  }

  writeFile(
    path: string,
    _modeFrom: string,
    noSync: boolean,
    fill: (writeAt: WriteAt) => void,
  ): void {
    if (this.dead) throw injected("create after process death");
    const d = new SimFile(this, path);
    this.files.set(path, d);
    this.dirOps.push({ kind: "c", from: "", to: path, file: d });
    this.fold("c", d.key, 0, null);
    fill((offset, bytes) => d.writeAt(offset, bytes));
    if (!noSync) d.sync();
  }

  rename(from: string, to: string): void {
    if (this.dead) throw injected("rename after process death");
    const d = this.files.get(from);
    if (d === undefined)
      throw engineError("io_error", "I/O error: rename of a missing file " + from);
    this.files.set(to, d);
    this.files.delete(from);
    this.dirOps.push({ kind: "r", from, to, file: d });
    this.fold("r", d.key, 0, null);
  }

  remove(path: string): void {
    if (this.dead) return;
    const d = this.files.get(path);
    if (d !== undefined) {
      this.files.delete(path);
      this.dirOps.push({ kind: "d", from: path, to: "", file: d });
      this.fold("d", d.key, 0, null);
    }
  }

  // syncDir is a fault point only when it has something to make durable: a directory barrier with no
  // pending change cannot lose anything, and counting it would shift every later barrier's number.
  syncDir(_dir: string): void {
    this.fold("D", 0n, 0, null);
    if (this.dead || this.dirOps.length === 0 || this.barrier()) return;
    this.durable = new Map(this.files);
    this.dirOps = [];
  }

  openStore(path: string, _noSync: boolean): BlockStore {
    if (this.dead) throw injected("open after process death");
    const d = this.files.get(path);
    if (d === undefined) throw engineError("io_error", "I/O error: no such file " + path);
    return d;
  }
}

// SimFile is one simulated file (the Go runner's simDisk).
export class SimFile implements BlockStore {
  private fs: SimFS;
  readonly key: bigint; // folded into fault decisions' offsets; 0 for the database file
  durable: Uint8Array = new Uint8Array();
  cache: Uint8Array = new Uint8Array();
  private pending = new Map<number, Uint8Array[]>();
  // unissued are the writes an armed process_crash decided never reached the kernel; processDeath
  // removes them.
  unissued: SimWrite[] = [];

  constructor(fs: SimFS, name: string) {
    this.fs = fs;
    this.key = fileKey(name);
  }

  private decide(lane: bigint, step: bigint, off: bigint, version: bigint): bigint {
    return decide(this.fs.seed, lane, step, (off + this.key) & MASK64, version);
  }

  readAt(offset: number, len: number): Uint8Array {
    const fs = this.fs;
    const f = fs.fault;
    if (
      f !== null &&
      f.kind === "read_error" &&
      !fs.dead &&
      !fs.oracle &&
      this.decide(LANE_READ, fs.step, BigInt(offset), 0n) % 1000n < BigInt(f.p)
    ) {
      fs.fired = "read";
      throw injected("read error");
    }
    if (offset < 0 || offset + len > this.cache.length) {
      throw engineError("io_error", "I/O error: short read past the end of the simulated disk");
    }
    return this.cache.slice(offset, offset + len);
  }

  writeAt(offset: number, bytes: Uint8Array): void {
    const fs = this.fs;
    fs.fold("w", this.key, offset, bytes);
    if (fs.dead) throw injected("write after process death");
    const version = this.pending.get(offset)?.length ?? 0;
    const f = fs.fault;
    if (f !== null) {
      if (f.kind === "process_crash") {
        // The live process sees all its own writes; only at its death does a write that was never
        // issued to the kernel disappear (processDeath).
        const d = this.decide(LANE_ISSUE, fs.step, BigInt(offset), BigInt(version));
        if (d % 1000n >= BigInt(f.issue)) this.unissued.push({ off: offset, version });
      } else if (f.kind === "io_write") {
        if (
          fs.fired === "" &&
          this.decide(LANE_IO_WRITE, fs.step, BigInt(offset), BigInt(version)) % 1000n < BigInt(f.p)
        ) {
          fs.fired = "write";
          throw injected("write error");
        }
      }
    }
    fs.writes++;
    this.cache = grow(this.cache, offset + bytes.length);
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
    // Only the database file's own barrier, with its directory entry durable, makes the generation it
    // holds durable (the floor, §8.2); a compaction's temp file syncing proves nothing about it.
    if (this === this.fs.main() && this.fs.dirOps.length === 0) this.fs.barrierOK = true;
    this.unissued = []; // reaching a barrier means every earlier write was issued
  }

  sync(): void {
    const fs = this.fs;
    fs.fold("s", this.key, 0, null);
    if (fs.dead) throw injected("sync after process death");
    fs.syncs++;
    if (fs.barrier()) throw injected("crash at a durability barrier");
    const f = fs.fault;
    if (f !== null && f.kind === "io_sync" && fs.fired === "" && fs.syncs === f.nth) {
      fs.fired = "sync";
      this.fsyncgate();
      throw injected("sync error");
    }
    this.flush();
  }

  // fsyncgate models a failed fsync on Linux: each pending write independently became durable or
  // was dropped from the cache, and the error is reported once (§6.3).
  private fsyncgate(): void {
    for (const off of this.sortedPending()) {
      const vs = this.pending.get(off)!;
      const newest = vs[vs.length - 1]!;
      if (this.decide(LANE_LAND, this.fs.step, BigInt(off), BigInt(vs.length - 1)) % 2n === 0n) {
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
  // hosts.md §2.1) or truncates. It is never a fault point for a crash (simulation-testing.md §6.3).
  setSize(n: number): void {
    const fs = this.fs;
    fs.fold("z", this.key, n, null);
    if (fs.dead) throw injected("set_size after process death");
    const cur = this.cache.length;
    const f = fs.fault;
    if (f !== null && f.kind === "enospc" && n > cur && fs.fired === "") {
      fs.fired = "enospc";
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

  // powerLoss replaces this file's contents with what survives a power cut during step (§6.3): for
  // each pending offset, nothing lands or one pending version (any, not only the newest) lands,
  // possibly torn sector by sector against the durable bytes. The cache is discarded.
  powerLoss(st: bigint, survive: number, tear: number): void {
    const sector = this.fs.sector;
    for (const off of this.sortedPending()) {
      const vs = this.pending.get(off)!;
      const o = BigInt(off);
      if (this.decide(LANE_SURVIVE, st, o, 0n) % 1000n >= BigInt(survive)) continue;
      const idx = this.decide(LANE_VERSION, st, o, 0n) % BigInt(vs.length);
      const data = vs[Number(idx)]!.slice();
      if (this.decide(LANE_TEAR, st, o, idx) % 1000n < BigInt(tear)) {
        for (let s = 0; s < data.length; s += sector) {
          if ((this.decide(LANE_SECTOR, st, o + BigInt(s), idx) & 1n) === 1n) continue; // new bytes
          for (let i = s; i < s + sector && i < data.length; i++) {
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
  }
}
