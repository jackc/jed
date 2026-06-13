// External merge sort with spill-to-disk for ORDER BY (spec/design/spill.md). A Sorter accumulates
// pushed rows up to a work-memory budget; when a file-backed database exceeds it, the sorter
// stable-sorts the in-memory run and spills it to a temporary file, then k-way-merges all runs at
// finish, reproducing the in-memory stable sort byte-for-byte (spill.md §4/§6).
//
// Not a §8 byte contract (spill.md §6): spill changes WHEN rows are resident, never WHAT a query
// observes (results + cost are invariant — the sort is unmetered, cost.md §3). So the run file's
// bytes are a per-core internal self-describing row codec, round-tripped only within one core during
// one query while the database file is unchanged — not the §8 on-disk record format. Node stdlib I/O
// only (no dependency — CLAUDE.md §14).

import { closeSync, openSync, readSync, unlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { Decimal } from "./decimal.ts";
import type { Row } from "./storage.ts";
import { type Value } from "./value.ts";

// DEFAULT_WORK_MEM is the default work-memory budget, in bytes (256 MiB) — the OpenOptions.workMem
// default (spec/design/spill.md §2, api.md §2.1). Matches the buffer-pool default so a RAM-sized
// ORDER BY stays fully in memory under the default; a host bounds a hostile/large sort by lowering
// it. A handle setting, never stored in the file.
export const DEFAULT_WORK_MEM = 256 * 1024 * 1024;

// A stable comparator over the ORDER BY keys: < 0 if a precedes b, 0 on a full tie (the caller's
// stable sort keeps input order — spill.md §6). Injected by the executor so this module never imports
// keyCmp (which would form a cycle with executor.ts).
export type RowCompare = (a: Row, b: Row) => number;

// A unique-per-process counter for spill file names (combined with the process id), so concurrent
// sorters never collide. Internal — it never affects results (spill.md §6).
let spillSeq = 0;

// valueBytes is a cheap, deterministic estimate of a value's resident bytes (spill.md §2): a fixed
// base plus the variable-width payload. It need not be exact — it only decides spill timing, which is
// invisible to results and cost.
function valueBytes(v: Value): number {
  const base = 24;
  switch (v.kind) {
    case "text":
      return base + v.text.length;
    case "bytea":
    case "uuid":
      return base + v.bytes.length;
    case "decimal":
      return base + v.dec.toCodec()[2].length * 2;
    case "unfetched":
      return base + (v.ref.comp?.length ?? 0);
    default:
      return base;
  }
}

function rowBytes(row: Row): number {
  let n = 8;
  for (const v of row) n += valueBytes(v);
  return n;
}

// Sorter is the external merge sorter (spec/design/spill.md §4). Push rows, then finish to read them
// back in ORDER BY order. Bounds resident memory to budget bytes by spilling sorted runs; an
// in-memory database (spillDir === null) or unlimited budget keeps everything resident and just
// stable-sorts at the end.
export class Sorter {
  private compare: RowCompare;
  private budget: number;
  private spillDir: string | null;
  private buf: Row[] = [];
  private bufBytes = 0;
  // Spilled run file paths, in input order (run 0 = first chunk — spill.md §6).
  private runs: string[] = [];
  total = 0;

  // compare orders rows; budget is the work-memory bound in bytes (0 ⇒ unlimited); spillDir is the
  // directory for run files, or null for an in-memory database (never spill).
  constructor(compare: RowCompare, budget: number, spillDir: string | null) {
    this.compare = compare;
    this.budget = budget;
    this.spillDir = spillDir;
  }

  private canSpill(): boolean {
    return this.spillDir !== null && this.budget > 0;
  }

  // push adds one row, spilling the current run when the in-memory buffer exceeds the budget.
  push(row: Row): void {
    this.total += 1;
    if (this.canSpill()) this.bufBytes += rowBytes(row);
    this.buf.push(row);
    if (this.canSpill() && this.bufBytes > this.budget) this.spillRun();
  }

  private sortBuf(): void {
    // Array.prototype.sort is stable (ES2019+), so equal-key rows keep input order — spill.md §6.
    this.buf.sort(this.compare);
  }

  // spillRun stable-sorts the in-memory buffer and writes it as one sorted run file, then clears it.
  private spillRun(): void {
    this.sortBuf();
    const w = new ByteWriter();
    w.u64(BigInt(this.buf.length));
    for (const row of this.buf) writeRow(w, row);
    const path = join(this.spillDir!, `jed-spill-${process.pid}-${spillSeq++}.tmp`);
    writeFileSync(path, w.result());
    this.runs.push(path);
    this.buf = [];
    this.bufBytes = 0;
  }

  // finish returns the rows in ORDER BY order. With no spilled run this is the unchanged in-memory
  // stable sort (the dominant RAM-sized fast path); otherwise it stable-sorts the final partial
  // buffer and k-way-merges it with the runs.
  finish(): SortedRows {
    this.sortBuf();
    if (this.runs.length === 0) return new SortedRows(this.buf, null);
    // Sources: each spilled run, then the final in-memory buffer last (the latest input positions →
    // the highest source index, the tie-break that reproduces input order — spill.md §6).
    const sources: MergeSource[] = [];
    try {
      for (const path of this.runs) sources.push(new FileSource(path));
    } catch (e) {
      for (const s of sources) s.close();
      throw e;
    }
    sources.push(new MemSource(this.buf));
    return new SortedRows(null, new Merger(sources, this.compare));
  }
}

// SortedRows is the sorted output stream (spec/design/spill.md §4). The window/projection loop pulls
// rows one at a time, so neither the input nor the output is re-materialized in the spill case.
export class SortedRows {
  private mem: Row[] | null;
  private merge: Merger | null;
  private pos = 0;
  constructor(mem: Row[] | null, merge: Merger | null) {
    this.mem = mem;
    this.merge = merge;
  }

  // next returns the next row in sort order, or null at the end.
  next(): Row | null {
    if (this.merge !== null) return this.merge.next();
    if (this.pos >= this.mem!.length) return null;
    return this.mem![this.pos++]!;
  }

  // close releases any spill run files still open (a LIMIT can stop the merge before every run is
  // drained — spill.md §4). A no-op for the in-memory case.
  close(): void {
    this.merge?.close();
  }
}

// Merger is the k-way merge over the run/buffer sources via a binary min-heap keyed by the order
// keys, ties broken by the lowest source index — exactly input order, reproducing the in-memory
// stable sort (spec/design/spill.md §6).
class Merger {
  private sources: MergeSource[];
  private compare: RowCompare;
  private heap: HeapItem[] = [];
  constructor(sources: MergeSource[], compare: RowCompare) {
    this.sources = sources;
    this.compare = compare;
    for (let i = 0; i < sources.length; i++) {
      const row = sources[i]!.next();
      if (row !== null) this.heapPush({ row, source: i });
    }
  }

  next(): Row | null {
    if (this.heap.length === 0) return null;
    const top = this.heapPop();
    const row = this.sources[top.source]!.next();
    if (row !== null) this.heapPush({ row, source: top.source });
    return top.row;
  }

  close(): void {
    for (const s of this.sources) s.close();
  }

  // less(a, b) is true when a should come out first: smaller by the order keys, ties by lower source.
  private less(a: HeapItem, b: HeapItem): boolean {
    const c = this.compare(a.row, b.row);
    if (c !== 0) return c < 0;
    return a.source < b.source;
  }

  private heapPush(item: HeapItem): void {
    const h = this.heap;
    h.push(item);
    let i = h.length - 1;
    while (i > 0) {
      const parent = (i - 1) >> 1;
      if (!this.less(h[i]!, h[parent]!)) break;
      [h[i], h[parent]] = [h[parent]!, h[i]!];
      i = parent;
    }
  }

  private heapPop(): HeapItem {
    const h = this.heap;
    const top = h[0]!;
    const last = h.pop()!;
    if (h.length > 0) {
      h[0] = last;
      let i = 0;
      for (;;) {
        const l = 2 * i + 1;
        const r = 2 * i + 2;
        let m = i;
        if (l < h.length && this.less(h[l]!, h[m]!)) m = l;
        if (r < h.length && this.less(h[r]!, h[m]!)) m = r;
        if (m === i) break;
        [h[i], h[m]] = [h[m]!, h[i]!];
        i = m;
      }
    }
    return top;
  }
}

type HeapItem = { row: Row; source: number };

// A merge input: a spilled run file (read back lazily, one row at a time) or the final in-memory buffer.
interface MergeSource {
  next(): Row | null;
  close(): void;
}

class MemSource implements MergeSource {
  private rows: Row[];
  private pos = 0;
  constructor(rows: Row[]) {
    this.rows = rows;
  }
  next(): Row | null {
    return this.pos < this.rows.length ? this.rows[this.pos++]! : null;
  }
  close(): void {}
}

const READ_CHUNK = 1 << 16; // 64 KiB — a bounded read buffer per open run

class FileSource implements MergeSource {
  private path: string;
  private fd: number;
  private filePos = 0;
  private chunk = Buffer.allocUnsafe(READ_CHUNK);
  private chunkLen = 0;
  private chunkPos = 0;
  private remaining: bigint;
  private closed = false;

  constructor(path: string) {
    this.path = path;
    this.fd = openSync(path, "r");
    this.remaining = this.u64();
  }

  next(): Row | null {
    if (this.remaining === 0n) {
      this.close(); // exhausted — close + delete the run file eagerly
      return null;
    }
    this.remaining -= 1n;
    return readRow(this);
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    closeSync(this.fd);
    try {
      unlinkSync(this.path);
    } catch {
      // best-effort cleanup
    }
  }

  // --- the streaming byte reader (a bounded chunk buffer keeps peak memory at one chunk per run) ---
  private refill(): void {
    this.chunkLen = readSync(this.fd, this.chunk, 0, this.chunk.length, this.filePos);
    this.filePos += this.chunkLen;
    this.chunkPos = 0;
  }
  byte(): number {
    if (this.chunkPos >= this.chunkLen) {
      this.refill();
      if (this.chunkLen === 0) throw new Error("unexpected EOF in spill run");
    }
    return this.chunk[this.chunkPos++]!;
  }
  bytes(n: number): Uint8Array {
    const out = new Uint8Array(n);
    let off = 0;
    while (off < n) {
      if (this.chunkPos >= this.chunkLen) {
        this.refill();
        if (this.chunkLen === 0) throw new Error("unexpected EOF in spill run");
      }
      const take = Math.min(n - off, this.chunkLen - this.chunkPos);
      out.set(this.chunk.subarray(this.chunkPos, this.chunkPos + take), off);
      this.chunkPos += take;
      off += take;
    }
    return out;
  }
  u64(): bigint {
    let n = 0n;
    for (let i = 0n; i < 8n; i++) n |= BigInt(this.byte()) << (i * 8n);
    return n;
  }
}

// ---- per-core self-describing run codec (spill.md §4) -----------------------------------------

// ByteWriter is a growable little-endian byte buffer for serializing one run.
class ByteWriter {
  private buf = new Uint8Array(1024);
  private len = 0;
  private ensure(n: number): void {
    if (this.len + n <= this.buf.length) return;
    let cap = this.buf.length * 2;
    while (cap < this.len + n) cap *= 2;
    const nb = new Uint8Array(cap);
    nb.set(this.buf.subarray(0, this.len));
    this.buf = nb;
  }
  u8(b: number): void {
    this.ensure(1);
    this.buf[this.len++] = b & 0xff;
  }
  u32(n: number): void {
    this.u8(n);
    this.u8(n >>> 8);
    this.u8(n >>> 16);
    this.u8(n >>> 24);
  }
  u64(n: bigint): void {
    let x = BigInt.asUintN(64, n);
    for (let i = 0; i < 8; i++) {
      this.u8(Number(x & 0xffn));
      x >>= 8n;
    }
  }
  bytesField(b: Uint8Array): void {
    this.u32(b.length);
    this.ensure(b.length);
    this.buf.set(b, this.len);
    this.len += b.length;
  }
  raw(b: Uint8Array): void {
    this.ensure(b.length);
    this.buf.set(b, this.len);
    this.len += b.length;
  }
  result(): Uint8Array {
    return this.buf.subarray(0, this.len);
  }
}

function writeRow(w: ByteWriter, row: Row): void {
  w.u32(row.length);
  for (const v of row) writeValue(w, v);
}

function writeValue(w: ByteWriter, v: Value): void {
  switch (v.kind) {
    case "null":
      w.u8(0);
      break;
    case "int":
      w.u8(1);
      w.u64(v.int);
      break;
    case "bool":
      w.u8(2);
      w.u8(v.value ? 1 : 0);
      break;
    case "text":
      w.u8(3);
      w.bytesField(new TextEncoder().encode(v.text));
      break;
    case "decimal": {
      w.u8(4);
      const [neg, scale, groups] = v.dec.toCodec();
      w.u8(neg ? 1 : 0);
      w.u32(scale);
      w.u32(groups.length);
      for (const g of groups) {
        w.u8(g);
        w.u8(g >>> 8);
      }
      break;
    }
    case "bytea":
      w.u8(5);
      w.bytesField(v.bytes);
      break;
    case "uuid":
      w.u8(6);
      w.raw(v.bytes); // exactly 16 bytes
      break;
    case "timestamp":
      w.u8(7);
      w.u64(v.micros);
      break;
    case "timestamptz":
      w.u8(8);
      w.u64(v.micros);
      break;
    case "interval":
      // Interval — tag 12 (tags 9/10/11 are the Unfetched forms below); months, days, micros.
      w.u8(12);
      w.u32(v.iv.months);
      w.u32(v.iv.days);
      w.u64(v.iv.micros);
      break;
    case "unfetched":
      // An untouched large-value reference rides along to the output unread (spill.md §4); spill it
      // opaquely so it round-trips, never resolving it.
      switch (v.ref.form) {
        case 0x02:
          w.u8(9);
          w.u32(v.ref.firstPage);
          w.u32(v.ref.storedLen);
          break;
        case 0x03:
          w.u8(10);
          w.u32(v.ref.rawLen);
          w.bytesField(v.ref.comp ?? new Uint8Array(0));
          break;
        case 0x04:
          w.u8(11);
          w.u32(v.ref.firstPage);
          w.u32(v.ref.storedLen);
          w.u32(v.ref.rawLen);
          break;
      }
      break;
  }
}

function readU32(r: FileSource): number {
  const b = r.bytes(4);
  return (b[0]! | (b[1]! << 8) | (b[2]! << 16) | (b[3]! << 24)) >>> 0;
}

function readRow(r: FileSource): Row {
  const ncols = readU32(r);
  const row: Row = new Array(ncols);
  for (let i = 0; i < ncols; i++) row[i] = readValue(r);
  return row;
}

function readValue(r: FileSource): Value {
  const tag = r.byte();
  switch (tag) {
    case 0:
      return { kind: "null" };
    case 1:
      return { kind: "int", int: r.u64() };
    case 2:
      return { kind: "bool", value: r.byte() !== 0 };
    case 3:
      return { kind: "text", text: new TextDecoder().decode(r.bytes(readU32(r))) };
    case 4: {
      const neg = r.byte() !== 0;
      const scale = readU32(r);
      const ng = readU32(r);
      const groups: number[] = new Array(ng);
      for (let i = 0; i < ng; i++) groups[i] = r.byte() | (r.byte() << 8);
      return { kind: "decimal", dec: Decimal.fromCodec(neg, scale, groups) };
    }
    case 5:
      return { kind: "bytea", bytes: r.bytes(readU32(r)) };
    case 6:
      return { kind: "uuid", bytes: r.bytes(16) };
    case 7:
      return { kind: "timestamp", micros: r.u64() };
    case 8:
      return { kind: "timestamptz", micros: r.u64() };
    case 9:
      return {
        kind: "unfetched",
        ref: { form: 0x02, firstPage: readU32(r), storedLen: readU32(r), rawLen: 0, comp: undefined },
      };
    case 10: {
      const rawLen = readU32(r);
      const comp = r.bytes(readU32(r));
      return { kind: "unfetched", ref: { form: 0x03, firstPage: 0, storedLen: 0, rawLen, comp } };
    }
    case 11:
      return {
        kind: "unfetched",
        ref: { form: 0x04, firstPage: readU32(r), storedLen: readU32(r), rawLen: readU32(r), comp: undefined },
      };
    case 12: {
      const months = readU32(r) | 0; // signed int32
      const days = readU32(r) | 0;
      const micros = BigInt.asIntN(64, r.u64()); // signed int64
      return { kind: "interval", iv: { months, days, micros } };
    }
    default:
      throw new Error("bad spill value tag");
  }
}
