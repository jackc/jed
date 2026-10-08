// Bounded, repeatable intermediate rows and partitioned state for blocking operators.
// Scratch is query-private and unmetered. Record traversal never determines SQL visitation:
// callers retain input order, and lookups compare the complete canonical key.
//
// Each structure measures its residency with the shared logical size schedule and charges the same
// number to the statement's query-memory account (spec/design/memory.md §6.6), so a spill happens at
// the same input row in every core: a row spool `row_bytes(row)`, a keyed state map `ENTRY +
// key_bytes(key) + key_bytes(value)`, a hash row table `ENTRY + row_bytes(row)`. An insert reserves its
// element's bytes; a spill releases the whole resident charge, after which the structure reserves
// nothing. TS has no destructors, so every owner releases (or closes) a structure at the point it is
// discarded. A rejected reservation throws 54P05 for the lane's Meter.costFirst (§6.7).
import { StateCharge } from "./cost.ts";
import { MEMORY_ENTRY } from "./costs.ts";
import { hashRowBytes, keyBytes, rowBytes } from "./memsize.ts";
import { decodeSpillRow, encodeSpillRow, type SpillScratch, type SpillSink } from "./spill.ts";
import type { Row } from "./storage.ts";

function u64(value: number): Uint8Array {
  const out = new Uint8Array(8);
  new DataView(out.buffer).setBigUint64(0, BigInt(value), true);
  return out;
}
function numberAt(file: SpillScratch, at: number): number {
  const bytes = file.read(at, 8);
  return Number(new DataView(bytes.buffer, bytes.byteOffset, 8).getBigUint64(0, true));
}
function writeRecord(file: SpillScratch, row: Row): number {
  const bytes = encodeSpillRow(row);
  const at = file.size;
  file.write(at, u64(bytes.length));
  file.write(at + 8, bytes);
  return at;
}
function readRecord(file: SpillScratch, at: number): Row {
  return decodeSpillRow(file.read(at + 8, numberAt(file, at)));
}
export class RowSpool implements Iterable<Row> {
  private rows: Row[] = [];
  private bytes = 0;
  private file: SpillScratch | null = null;
  private budget: number;
  private sink: SpillSink;
  // The resident rows' query-memory charge: row_bytes per resident row, returned when the spool spills,
  // and otherwise when the spool is discarded (release / close).
  private charge: StateCharge;
  length = 0;
  constructor(budget: number, sink: SpillSink, charge: StateCharge = new StateCharge()) {
    this.budget = budget;
    this.sink = sink;
    this.charge = charge;
  }
  push(row: Row): void {
    if (this.file !== null) {
      this.length++;
      writeRecord(this.file, row);
      return;
    }
    // The spool spills when its residency exceeds work_mem or when the account rejects the row — the
    // rejected row is never charged and goes to scratch with the resident rows (memory.md §6.6).
    const bytes = rowBytes(row);
    const rejected = !this.charge.tryReserveDirect(bytes);
    this.length++;
    this.rows.push(row);
    this.bytes += bytes;
    if (rejected || this.bytes > this.budget) {
      this.file = this.sink.createScratch!();
      for (const buffered of this.rows) writeRecord(this.file, buffered);
      this.rows = [];
      this.bytes = 0;
      // The rows left memory: the spool's whole resident charge goes with them.
      this.charge.releaseAll();
    }
  }
  *[Symbol.iterator](): Generator<Row> {
    if (this.file === null) {
      yield* this.rows;
      return;
    }
    const size = this.file.size;
    for (let at = 0; at < size; ) {
      const length = numberAt(this.file, at);
      yield decodeSpillRow(this.file.read(at + 8, length));
      at += 8 + length;
    }
  }
  // release returns the resident charge now — the spool (or its reader) is discarded at this stage
  // boundary; the rows stay readable until close.
  release(): void {
    this.charge.releaseAll();
  }
  close(): void {
    this.rows = [];
    this.file?.close();
    this.file = null;
    this.charge.releaseAll();
  }
}

const PARTITIONS = 4096;
const textEncoder = new TextEncoder();
function hashKey(key: Uint8Array): number {
  let hash = 2166136261;
  for (const byte of key) hash = Math.imul(hash ^ byte, 16777619);
  return hash >>> 0;
}
function equalBytes(a: Uint8Array, b: Uint8Array): boolean {
  return a.length === b.length && a.every((byte, i) => byte === b[i]);
}

// The disk directory is fixed-size; its chains stream one record at a time even if every key
// hashes to one partition. Existing keys replace a value pointer, so repeated updates never grow
// a resident directory or a chain of stale versions. A single value can exceed work_mem.
export class SpillMap {
  // Each resident entry keeps its measured key and value bytes, so a replacement charges the growth.
  private memory = new Map<string, { value: Row; keyLen: number; valueLen: number }>();
  private bytes = 0;
  private file: SpillScratch | null = null;
  private sink: SpillSink;
  private budget: number;
  // The resident entries' query-memory charge, returned on spill or discard.
  private charge: StateCharge;
  constructor(budget: number, sink: SpillSink, charge: StateCharge = new StateCharge()) {
    this.budget = budget;
    this.sink = sink;
    this.charge = charge;
  }
  private find(key: Uint8Array): { bucket: number; node: number } {
    const file = this.file!;
    const hash = hashKey(key);
    const bucket = (hash % PARTITIONS) * 8;
    let node = numberAt(file, bucket);
    while (node !== 0) {
      const keyLength = numberAt(file, node + 16);
      if (
        numberAt(file, node + 24) === hash &&
        keyLength === key.length &&
        equalBytes(file.read(node + 32, keyLength), key)
      )
        return { bucket, node };
      node = numberAt(file, node);
    }
    return { bucket, node: 0 };
  }
  get(key: string): Row | undefined {
    if (this.file === null) return this.memory.get(key)?.value;
    const { node } = this.find(textEncoder.encode(key));
    return node === 0 ? undefined : readRecord(this.file, numberAt(this.file, node + 8));
  }
  // set stores `value` under the canonical `key`. keyLen is key_bytes of the logical key row and
  // valueLen key_bytes of the logical value (memory.md §6.6): a new entry is resident as ENTRY + keyLen
  // + valueLen, a replaced value as its growth (or shrinkage).
  set(key: string, value: Row, keyLen = 0, valueLen = keyBytes(value)): void {
    if (this.file === null) {
      const old = this.memory.get(key);
      const delta = old === undefined ? MEMORY_ENTRY + keyLen + valueLen : valueLen - old.valueLen;
      // A growth the account rejects spills the map, the new entry with it, uncharged (memory.md §6.6).
      let rejected = false;
      if (delta > 0) rejected = !this.charge.tryReserveDirect(delta);
      else this.charge.release(-delta);
      this.memory.set(key, { value, keyLen: old?.keyLen ?? keyLen, valueLen });
      this.bytes += delta;
      if (!rejected && this.bytes <= this.budget) return;
      this.file = this.sink.createScratch!();
      this.file.write(0, new Uint8Array(PARTITIONS * 8));
      // The entries left memory: the map's whole resident charge goes with them.
      this.charge.releaseAll();
      for (const [k, v] of this.memory) this.write(k, v.value);
      this.memory.clear();
      this.bytes = 0;
      return;
    }
    this.write(key, value);
  }
  // insert adds `key` with an empty value unless present; reports whether it was new.
  insert(key: string, keyLen: number): boolean {
    if (this.get(key) !== undefined) return false;
    this.set(key, [], keyLen, 0);
    return true;
  }
  private write(key: string, value: Row): void {
    const file = this.file!;
    const bytes = textEncoder.encode(key);
    const { bucket, node } = this.find(bytes);
    const record = writeRecord(file, value);
    if (node !== 0) {
      file.write(node + 8, u64(record));
      return;
    }
    const at = file.size;
    file.write(at, u64(numberAt(file, bucket)));
    file.write(at + 8, u64(record));
    file.write(at + 16, u64(bytes.length));
    file.write(at + 24, u64(hashKey(bytes)));
    file.write(at + 32, bytes);
    file.write(bucket, u64(at));
  }
  // release returns the resident charge now — the map is discarded at this stage boundary.
  release(): void {
    this.charge.releaseAll();
  }
  close(): void {
    this.memory.clear();
    this.file?.close();
    this.file = null;
    this.charge.releaseAll();
  }
}

// An insertion-ordered multimap used for hash JOIN buckets. Row chains are external immediately;
// the bounded map holds only first/last offsets. A skewed bucket never becomes a match-index array.
export class SpillMultiMap {
  // The disk-chain index (used only once spilled): uncharged, since a spilled table reserves nothing.
  private index: SpillMap;
  private file: SpillScratch | null = null;
  private memory = new Map<string, Row[]>();
  private bytes = 0;
  private budget: number;
  private sink: SpillSink;
  // The resident rows' query-memory charge, returned on spill or discard.
  private charge: StateCharge;
  constructor(budget: number, sink: SpillSink, charge: StateCharge = new StateCharge()) {
    this.budget = budget;
    this.sink = sink;
    this.charge = charge;
    this.index = new SpillMap(budget, sink);
  }
  // append adds `row` to the bucket `key`. `bytes` is the row's resident measure, ENTRY + row_bytes of
  // the logical stored row (memory.md §6.6) — by default the row itself.
  append(key: string, row: Row, bytes = hashRowBytes(row)): void {
    if (this.file === null) {
      // A row the account rejects spills the table, the row with it, uncharged (memory.md §6.6).
      const rejected = !this.charge.tryReserveDirect(bytes);
      const rows = this.memory.get(key);
      if (rows === undefined) this.memory.set(key, [row]);
      else rows.push(row);
      this.bytes += bytes;
      if (!rejected && this.bytes <= this.budget) return;
      this.file = this.sink.createScratch!();
      this.file.write(0, u64(0));
      // The rows left memory: the table's whole resident charge goes with them.
      this.charge.releaseAll();
      for (const [k, bucket] of this.memory) for (const r of bucket) this.appendDisk(k, r);
      this.memory.clear();
      this.bytes = 0;
      return;
    }
    this.appendDisk(key, row);
  }
  private appendDisk(key: string, row: Row): void {
    const file = this.file!;
    const previous = this.index.get(key);
    const at = file.size;
    file.write(at, u64(0));
    writeRecord(file, row);
    const first = previous === undefined ? at : Number((previous[0]! as { int: bigint }).int);
    if (previous !== undefined) file.write(Number((previous[1]! as { int: bigint }).int), u64(at));
    this.index.set(key, [
      { kind: "int", int: BigInt(first) },
      { kind: "int", int: BigInt(at) },
    ]);
  }
  *get(key: string): Generator<Row> {
    if (this.file === null) {
      yield* this.memory.get(key) ?? [];
      return;
    }
    const bounds = this.index.get(key);
    if (bounds === undefined) return;
    let at = Number((bounds[0]! as { int: bigint }).int);
    while (at !== 0) {
      const next = numberAt(this.file, at);
      yield readRecord(this.file, at + 8);
      at = next;
    }
  }
  // release returns the resident charge now — the table is discarded at this stage boundary.
  release(): void {
    this.charge.releaseAll();
  }
  close(): void {
    this.memory.clear();
    this.index.close();
    this.file?.close();
    this.file = null;
    this.charge.releaseAll();
  }
}

// Incremental stable run compaction, shared by standalone ORDER BY and other blocking stages.
// At most one live run per exact-integer cardinality bit and two merge heads are retained.
export class SpoolSorter {
  private compare: (a: Row, b: Row) => number;
  private budget: number;
  private sink: SpillSink;
  private owned = new Set<RowSpool>();
  private levels: (RowSpool | undefined)[] = [];
  private chunk: Row[] = [];
  private bytes = 0;
  // The query-memory charge of the resident chunk (memory.md §6.4): reserved per pushed row, returned
  // when a run spills. The final chunk stays charged — logically the sorter's final in-memory run, even
  // though finishSpool writes it out to merge — until the owner releases it when the sorted output's
  // emission completes (release / the finish() handle's close). The internal run spools are uncharged.
  private charge: StateCharge;
  total = 0;
  constructor(
    compare: (a: Row, b: Row) => number,
    budget: number,
    sink: SpillSink,
    charge: StateCharge = new StateCharge(),
  ) {
    this.compare = compare;
    this.budget = budget;
    this.sink = sink;
    this.charge = charge;
  }
  private fresh(): RowSpool {
    const spool = new RowSpool(this.budget, this.sink);
    this.owned.add(spool);
    return spool;
  }
  private merge(a: RowSpool, b: RowSpool): RowSpool {
    const out = this.fresh();
    const ai = a[Symbol.iterator]();
    const bi = b[Symbol.iterator]();
    let av = ai.next();
    let bv = bi.next();
    while (!av.done || !bv.done) {
      if (bv.done || (!av.done && this.compare(av.value, bv.value) <= 0)) {
        out.push(av.value!);
        av = ai.next();
      } else {
        out.push(bv.value);
        bv = bi.next();
      }
    }
    a.close();
    b.close();
    this.owned.delete(a);
    this.owned.delete(b);
    return out;
  }
  push(row: Row): void {
    // The chunk spills when it exceeds work_mem or when the account rejects the row — the rejected row
    // is never charged and leaves with the run (memory.md §6.6).
    const bytes = rowBytes(row);
    const rejected = !this.charge.tryReserveDirect(bytes);
    this.total++;
    this.chunk.push(row);
    this.bytes += bytes;
    if (rejected || this.bytes > this.budget) {
      this.flush();
      // The run left memory: its rows' charge goes with it (memory.md §6.4/§6.6).
      this.charge.releaseAll();
    }
  }
  private flush(): void {
    this.chunk.sort(this.compare);
    let run = this.fresh();
    for (const row of this.chunk) run.push(row);
    this.chunk = [];
    this.bytes = 0;
    let level = 0;
    while (this.levels[level] !== undefined) {
      run = this.merge(this.levels[level]!, run);
      this.levels[level] = undefined;
      level++;
    }
    this.levels[level] = run;
  }
  finishSpool(): RowSpool {
    if (this.chunk.length > 0) this.flush();
    let result: RowSpool | undefined;
    for (let level = this.levels.length - 1; level >= 0; level--) {
      const run = this.levels[level];
      if (run !== undefined) result = result === undefined ? run : this.merge(result, run);
    }
    result ??= this.fresh();
    this.owned.delete(result);
    this.levels = [];
    return result;
  }
  finish(): { next(): Row | null; close(): void } {
    const out = this.finishSpool();
    const iterator = out[Symbol.iterator]();
    return {
      next: () => {
        const row = iterator.next();
        return row.done ? null : row.value;
      },
      close: () => {
        iterator.return(undefined);
        out.close();
        this.charge.releaseAll();
      },
    };
  }
  // release returns the final run's charge — the sorted output's emission completed.
  release(): void {
    this.charge.releaseAll();
  }
  close(): void {
    for (const spool of this.owned) spool.close();
    this.owned.clear();
    this.levels = [];
    this.chunk = [];
    this.charge.releaseAll();
  }
}
export function sortSpool(
  input: RowSpool,
  compare: (a: Row, b: Row) => number,
  budget: number,
  sink: SpillSink,
): RowSpool {
  const sorter = new SpoolSorter(compare, budget, sink);
  try {
    for (const row of input) sorter.push(row);
    return sorter.finishSpool();
  } finally {
    sorter.close();
  }
}

// SpillSet is a DISTINCT dedup set: an in-memory set (no scratch or unlimited work_mem), or a
// spill-capable SpillMap. Either way each first occurrence reserves its entry, ENTRY + key_bytes(row),
// while resident (memory.md §6.2/§6.6).
export class SpillSet {
  private resident: Set<string> | null;
  private disk: SpillMap | null;
  private charge: StateCharge;
  constructor(budget: number, sink: SpillSink | null, charge: StateCharge = new StateCharge()) {
    this.charge = charge;
    this.disk =
      budget > 0 && sink?.createScratch !== undefined ? new SpillMap(budget, sink, charge) : null;
    this.resident = this.disk === null ? new Set<string>() : null;
  }
  // insert adds `row` under its canonical `key` and reports whether it was new; a rejected reservation
  // throws 54P05 for the caller's Meter.costFirst.
  insert(key: string, row: Row): boolean {
    if (this.disk !== null) return this.disk.insert(key, keyBytes(row));
    if (this.resident!.has(key)) return false;
    this.resident!.add(key);
    if (this.charge.active()) this.charge.reserveDirect(MEMORY_ENTRY + keyBytes(row));
    return true;
  }
  // release returns the set's charge — the DISTINCT pass completed (memory.md §6.2).
  release(): void {
    this.charge.releaseAll();
  }
  close(): void {
    this.resident?.clear();
    this.disk?.close();
    this.charge.releaseAll();
  }
}
