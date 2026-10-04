// Bounded, repeatable intermediate rows and partitioned state for blocking operators.
// Scratch is query-private and unmetered. Record traversal never determines SQL visitation:
// callers retain input order, and lookups compare the complete canonical key.
import {
  decodeSpillRow,
  encodeSpillRow,
  rowBytes,
  type SpillScratch,
  type SpillSink,
} from "./spill.ts";
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
  length = 0;
  constructor(budget: number, sink: SpillSink) {
    this.budget = budget;
    this.sink = sink;
  }
  push(row: Row): void {
    this.length++;
    if (this.file !== null) {
      writeRecord(this.file, row);
      return;
    }
    this.rows.push(row);
    this.bytes += rowBytes(row);
    if (this.bytes > this.budget) {
      this.file = this.sink.createScratch!();
      for (const buffered of this.rows) writeRecord(this.file, buffered);
      this.rows = [];
      this.bytes = 0;
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
  close(): void {
    this.rows = [];
    this.file?.close();
    this.file = null;
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
  private memory = new Map<string, Row>();
  private bytes = 0;
  private file: SpillScratch | null = null;
  private sink: SpillSink;
  private budget: number;
  constructor(budget: number, sink: SpillSink) {
    this.budget = budget;
    this.sink = sink;
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
    if (this.file === null) return this.memory.get(key);
    const { node } = this.find(textEncoder.encode(key));
    return node === 0 ? undefined : readRecord(this.file, numberAt(this.file, node + 8));
  }
  set(key: string, value: Row): void {
    if (this.file === null) {
      const old = this.memory.get(key);
      this.bytes += rowBytes(value) - (old === undefined ? 0 : rowBytes(old));
      if (old === undefined) this.bytes += key.length * 2 + 48;
      this.memory.set(key, value);
      if (this.bytes <= this.budget) return;
      this.file = this.sink.createScratch!();
      this.file.write(0, new Uint8Array(PARTITIONS * 8));
      for (const [k, v] of this.memory) this.write(k, v);
      this.memory.clear();
      this.bytes = 0;
      return;
    }
    this.write(key, value);
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
  close(): void {
    this.memory.clear();
    this.file?.close();
    this.file = null;
  }
}

// An insertion-ordered multimap used for hash JOIN buckets. Row chains are external immediately;
// the bounded map holds only first/last offsets. A skewed bucket never becomes a match-index array.
export class SpillMultiMap {
  private index: SpillMap;
  private file: SpillScratch | null = null;
  private memory = new Map<string, Row[]>();
  private bytes = 0;
  private budget: number;
  private sink: SpillSink;
  constructor(budget: number, sink: SpillSink) {
    this.budget = budget;
    this.sink = sink;
    this.index = new SpillMap(budget, sink);
  }
  append(key: string, row: Row): void {
    if (this.file === null) {
      const rows = this.memory.get(key);
      if (rows === undefined) {
        this.memory.set(key, [row]);
        this.bytes += key.length * 2 + 48;
      } else rows.push(row);
      this.bytes += rowBytes(row);
      if (this.bytes <= this.budget) return;
      this.file = this.sink.createScratch!();
      this.file.write(0, u64(0));
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
  close(): void {
    this.memory.clear();
    this.index.close();
    this.file?.close();
    this.file = null;
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
  total = 0;
  constructor(compare: (a: Row, b: Row) => number, budget: number, sink: SpillSink) {
    this.compare = compare;
    this.budget = budget;
    this.sink = sink;
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
    this.total++;
    this.chunk.push(row);
    this.bytes += rowBytes(row);
    if (this.bytes > this.budget) this.flush();
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
      },
    };
  }
  close(): void {
    for (const spool of this.owned) spool.close();
    this.owned.clear();
    this.levels = [];
    this.chunk = [];
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

export class SpillSet {
  private resident: Set<string> | null;
  private disk: SpillMap | null;
  constructor(budget: number, sink: SpillSink | null) {
    this.disk = budget > 0 && sink?.createScratch !== undefined ? new SpillMap(budget, sink) : null;
    this.resident = this.disk === null ? new Set<string>() : null;
  }
  has(key: string): boolean {
    return this.disk === null ? this.resident!.has(key) : this.disk.get(key) !== undefined;
  }
  add(key: string): void {
    if (this.disk === null) this.resident!.add(key);
    else this.disk.set(key, []);
  }
  close(): void {
    this.resident?.clear();
    this.disk?.close();
  }
}
