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

import type { BlockStore } from "./blockstore.ts";
import { StateCharge } from "./cost.ts";
import { Decimal } from "./decimal.ts";
import { jsonbIn, jsonbOut } from "./json.ts";
import { rowBytes } from "./memsize.ts";
import type { Row } from "./storage.ts";
import {
  type Value,
  emptyRangeValue,
  float32Value,
  float64Value,
  jsonValue,
  jsonbValue,
  rangeValue,
  sentinelTypeRef,
} from "./value.ts";

// SpillSink is the host backing for spilled runs — the Node `fs` implementation is FileSpillSink
// (spillfile.ts), injected on the Engine handle by a durable host that can spill to disk. null for an
// in-memory / OPFS database (never spill — spill.md §2). Keeping it an interface here is what frees
// spill.ts — and thus the whole executor — of any `node:*` import, so the engine runs in a browser
// bundle (the OPFS host). The run codec below is a per-core internal format, never the §8 on-disk
// format (spill.md §6).
export interface SpillSink {
  // writeRun persists one sorted run's bytes and returns a handle to read it back.
  writeRun(bytes: Uint8Array): SpillRun;
  // Repeatable positional scratch for bounded blocking operators.
  createScratch?(): SpillScratch;
  // createScratchStore creates the private scratch BlockStore a session-local temp domain spills to
  // once it outgrows temp_buffers (spec/design/temp-tables.md §6): an empty, never-fsynced file in the
  // host spill directory, unlinked at creation where the OS allows. The owner MUST close it (JS has no
  // destructor), which also removes a file the OS would not unlink while open.
  createScratchStore?(): BlockStore;
  // encrypts reports whether every file this sink makes is sealed (scratch encryption,
  // spec/fileformat/encrypted.md §11). Absent means false.
  encrypts?(): boolean;
}
export interface SpillScratch {
  size: number;
  read(position: number, length: number): Uint8Array;
  write(position: number, bytes: Uint8Array): void;
  close(): void;
}

// SpillRun is one written run; open() streams it back exactly once (the merge opens each run once).
export interface SpillRun {
  open(): SpillByteReader;
}
// SpillByteReader is a forward byte reader over a run (the codec below pulls bytes through it). close()
// releases and deletes the run.
export interface SpillByteReader {
  byte(): number;
  bytes(n: number): Uint8Array;
  u64(): bigint;
  close(): void;
}

// DEFAULT_WORK_MEM is the default work-memory budget, in bytes (256 MiB) — the OpenOptions.workMem
// default (spec/design/spill.md §2, api.md §2.1). Matches the buffer-pool default so a RAM-sized
// ORDER BY stays fully in memory under the default; a host bounds a hostile/large sort by lowering
// it. A handle setting, never stored in the file.
export const DEFAULT_WORK_MEM = 256 * 1024 * 1024;

// A stable comparator over the ORDER BY keys: < 0 if a precedes b, 0 on a full tie (the caller's
// stable sort keeps input order — spill.md §6). Injected by the executor so this module never imports
// keyCmp (which would form a cycle with executor.ts).
export type RowCompare = (a: Row, b: Row) => number;

// Sorter is the external merge sorter (spec/design/spill.md §4). Push rows, then finish to read them
// back in ORDER BY order. Bounds resident memory to budget bytes by spilling sorted runs; an
// in-memory database (spillDir === null) or unlimited budget keeps everything resident and just
// stable-sorts at the end. A row's resident bytes are the shared logical size schedule rowBytes
// (spec/design/memory.md §3) — the same number the query-memory account charges for sort state (§6.4),
// so every core spills at the same input row. Rows reach a sorter with their untouched slots NULLed
// (§6.1), so no lazy reference hides a backing block.
export class Sorter {
  private compare: RowCompare;
  private budget: number;
  private sink: SpillSink | null;
  private buf: Row[] = [];
  private bufBytes = 0;
  // Spilled runs, in input order (run 0 = first chunk — spill.md §6).
  private runs: SpillRun[] = [];
  total = 0;
  // The query-memory charge of the resident run (memory.md §6.4): reserved per pushed row, returned when
  // a run spills, and handed to the SortedRows at finish so it is returned when the sorted output's
  // emission completes.
  private charge: StateCharge;

  // compare orders rows; budget is the work-memory bound in bytes (0 ⇒ unlimited); sink persists
  // spilled runs, or null for an in-memory / OPFS database (never spill — spill.md §2).
  constructor(
    compare: RowCompare,
    budget: number,
    sink: SpillSink | null,
    charge: StateCharge = new StateCharge(),
  ) {
    this.compare = compare;
    this.budget = budget;
    this.sink = sink;
    this.charge = charge;
  }

  private canSpill(): boolean {
    return this.sink !== null && this.budget > 0;
  }

  // push adds one row, spilling the current run when the in-memory buffer exceeds the budget or when the
  // account rejects the row's reservation — the rejected row is never charged and leaves with the run
  // (memory.md §6.6). A sorter that cannot spill throws the 54P05 for the caller's Meter.costFirst
  // (memory.md §6.7).
  push(row: Row): void {
    let rejected = false;
    if (this.canSpill()) {
      const bytes = rowBytes(row);
      rejected = !this.charge.tryReserveDirect(bytes);
      this.bufBytes += bytes;
    } else if (this.charge.active()) {
      this.charge.reserveDirect(rowBytes(row));
    }
    this.total += 1;
    this.buf.push(row);
    if (this.canSpill() && (rejected || this.bufBytes > this.budget)) this.spillRun();
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
    this.runs.push(this.sink!.writeRun(w.result()));
    this.buf = [];
    this.bufBytes = 0;
    // The run left memory: its rows' charge goes with it (memory.md §6.4/§6.6).
    this.charge.releaseAll();
  }

  close(): void {
    for (const run of this.runs) {
      try {
        run.open().close();
      } catch {
        /* preserve the original failure */
      }
    }
    this.runs = [];
    this.buf = [];
    this.charge.releaseAll();
  }

  // finish returns the rows in ORDER BY order. With no spilled run this is the unchanged in-memory
  // stable sort (the dominant RAM-sized fast path); otherwise it stable-sorts the final partial
  // buffer and k-way-merges it with the runs.
  finish(): SortedRows {
    this.sortBuf();
    if (this.runs.length === 0) return new SortedRows(this.buf, null, this.charge);
    // Sources: each spilled run, then the final in-memory buffer last (the latest input positions →
    // the highest source index, the tie-break that reproduces input order — spill.md §6).
    const sources: MergeSource[] = [];
    try {
      for (const run of this.runs) sources.push(new RunSource(run.open()));
    } catch (e) {
      for (const s of sources) s.close();
      throw e;
    }
    sources.push(new MemSource(this.buf));
    return new SortedRows(
      null,
      new Merger(sources, this.compare, this.charge.sibling()),
      this.charge,
    );
  }
}

// SortedRows is the sorted output stream (spec/design/spill.md §4). The window/projection loop pulls
// rows one at a time, so neither the input nor the output is re-materialized in the spill case.
export class SortedRows {
  private mem: Row[] | null;
  private merge: Merger | null;
  private pos = 0;
  // The sort state's query-memory charge (memory.md §6.4): the final in-memory run, returned when the
  // sorted output's emission completes (close).
  private charge: StateCharge;
  constructor(mem: Row[] | null, merge: Merger | null, charge: StateCharge = new StateCharge()) {
    this.mem = mem;
    this.merge = merge;
    this.charge = charge;
  }

  // next returns the next row in sort order, or null at the end.
  next(): Row | null {
    if (this.merge !== null) return this.merge.next();
    if (this.pos >= this.mem!.length) return null;
    return this.mem![this.pos++]!;
  }

  // close releases any spill run files still open (a LIMIT can stop the merge before every run is
  // drained — spill.md §4), and returns the sort state's charge — the emission completed (memory.md
  // §6.4).
  close(): void {
    this.merge?.close();
    this.charge.releaseAll();
  }
}

// Merger is the k-way merge over the run/buffer sources via a binary min-heap keyed by the order
// keys, ties broken by the lowest source index — exactly input order, reproducing the in-memory
// stable sort (spec/design/spill.md §6).
class Merger {
  private sources: MergeSource[];
  private compare: RowCompare;
  private heap: HeapItem[] = [];
  // The heads read back from spilled runs (memory.md §6.8): each reserves its rowBytes as it enters the
  // heap and is released when it leaves. The final in-memory buffer's rows are already charged with it,
  // so its heads reserve nothing. The merge cannot spill further, so a head whose reservation the
  // account rejects is held uncharged (at most one row per run).
  private heads: StateCharge;
  // Reads each source's first row into the heap, in source order.
  constructor(sources: MergeSource[], compare: RowCompare, heads: StateCharge = new StateCharge()) {
    this.sources = sources;
    this.compare = compare;
    this.heads = heads;
    for (let i = 0; i < sources.length; i++) {
      const row = sources[i]!.next();
      if (row !== null) this.heapPush({ row, source: i, charged: this.chargeHead(i, row) });
    }
  }

  // chargeHead reserves a head from source i if it fits, returning the bytes it holds: a spilled run's
  // row is new memory; a rejected head, and the in-memory buffer's, hold nothing.
  private chargeHead(i: number, row: Row): number {
    if (!this.heads.active() || !(this.sources[i] instanceof RunSource)) return 0;
    const bytes = rowBytes(row);
    return this.heads.tryReserveDirect(bytes) ? bytes : 0;
  }

  // next is the next row in order. The popped head's source replaces it (reserved first), then the
  // popped head's charge is released as the row leaves the merge.
  next(): Row | null {
    if (this.heap.length === 0) return null;
    const top = this.heapPop();
    const row = this.sources[top.source]!.next();
    if (row !== null) {
      this.heapPush({ row, source: top.source, charged: this.chargeHead(top.source, row) });
    }
    this.heads.release(top.charged);
    return top.row;
  }

  close(): void {
    for (const s of this.sources) s.close();
    this.heads.releaseAll();
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

// charged is the query-memory bytes this head holds (zero for the in-memory buffer's rows).
type HeapItem = { row: Row; source: number; charged: number };

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

// RunSource is a merge input backed by a spilled run, read lazily one row at a time through the
// injected SpillByteReader (the Node `fs` reader lives in spillfile.ts). The leading u64 is the run's
// row count; when drained it closes the reader (which deletes the run file — eager cleanup so a LIMIT
// that stops the merge early leaks nothing, spill.md §4).
class RunSource implements MergeSource {
  private reader: SpillByteReader;
  private remaining: bigint;

  constructor(reader: SpillByteReader) {
    this.reader = reader;
    this.remaining = reader.u64();
  }

  next(): Row | null {
    if (this.remaining === 0n) {
      this.reader.close();
      return null;
    }
    this.remaining -= 1n;
    return readRow(this.reader);
  }

  close(): void {
    this.reader.close();
  }
}

// ---- per-core self-describing run codec (spill.md §4) -----------------------------------------

// ByteWriter is a growable little-endian byte buffer for serializing one run.
class ByteWriter {
  private buf: Uint8Array;
  private view: DataView;
  private len = 0;
  constructor(capacity = 1024) {
    this.buf = new Uint8Array(capacity);
    this.view = new DataView(this.buf.buffer);
  }
  private ensure(n: number): void {
    if (this.len + n <= this.buf.length) return;
    let cap = this.buf.length * 2;
    while (cap < this.len + n) cap *= 2;
    const nb = new Uint8Array(cap);
    nb.set(this.buf.subarray(0, this.len));
    this.buf = nb;
    this.view = new DataView(nb.buffer);
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
    this.ensure(8);
    this.view.setBigUint64(this.len, BigInt.asUintN(64, n), true);
    this.len += 8;
  }
  f64(n: number): void {
    this.ensure(8);
    this.view.setFloat64(this.len, n, true); // little-endian, matching this format's u32/u64
    this.len += 8;
  }
  f32(n: number): void {
    const dv = new DataView(new ArrayBuffer(4));
    dv.setFloat32(0, n, true);
    this.raw(new Uint8Array(dv.buffer));
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
    case "date":
      // Date — tag 17 (the i32 day count); internal merge-sort scratch format (spec/design/date.md).
      w.u8(17);
      w.u64(v.days);
      break;
    case "f64":
      // The 8 IEEE bytes (DataView, the spill format is per-core internal — bits round-trip incl
      // -0/NaN/±Inf so an ORDER BY over / carrying a float column spills correctly).
      w.u8(13);
      w.f64(v.value);
      break;
    case "f32":
      w.u8(14);
      w.f32(v.value);
      break;
    case "interval":
      // Interval — tag 12 (tags 9/10/11 are the Unfetched forms below); months, days, micros.
      w.u8(12);
      w.u32(v.iv.months);
      w.u32(v.iv.days);
      w.u64(v.iv.micros);
      break;
    case "composite":
      // Composite — tag 15: field count then each field value, recursive (spec/design/composite.md).
      // Internal merge-sort scratch format only, so the recursion needs no type context.
      w.u8(15);
      w.u32(v.fields.length);
      for (const f of v.fields) writeValue(w, f);
      break;
    case "array":
      // Array — tag 16: ndim, then per-dimension (length, lower bound), then each element value,
      // recursive (spec/design/array.md). Internal merge-sort scratch format; the full shape
      // round-trips (multidim + custom bounds).
      w.u8(16);
      w.u32(v.dims.length);
      for (let d = 0; d < v.dims.length; d++) {
        w.u32(v.dims[d]!);
        w.u32(v.lbounds[d]! >>> 0);
      }
      for (const el of v.elements) writeValue(w, el);
      break;
    case "range": {
      // Range — tag 18: the flags byte (EMPTY/LB_INF/UB_INF/LB_INC/UB_INC) then each present bound
      // value, recursive (spec/design/ranges.md §4). Internal merge-sort scratch format only, so the
      // recursion needs no element-type context — the bound values round-trip themselves. A range
      // column can ride a spilling sort as a carried (non-key) column even before range ORDER BY
      // lands (R3), so it must spill faithfully now.
      let flags = 0;
      if (v.empty) flags |= 0x01;
      if (v.lower === null) flags |= 0x02;
      if (v.upper === null) flags |= 0x04;
      if (v.lowerInc) flags |= 0x08;
      if (v.upperInc) flags |= 0x10;
      w.u8(18);
      w.u8(flags);
      if (!v.empty) {
        if (v.lower !== null) writeValue(w, v.lower);
        if (v.upper !== null) writeValue(w, v.upper);
      }
      break;
    }
    case "json":
      // json — tag 19: the verbatim text. Internal merge-sort scratch format only
      // (spec/design/json.md); a json column can ride a spilling sort as a carried column.
      w.u8(19);
      w.bytesField(new TextEncoder().encode(v.text));
      break;
    case "jsonb":
      // jsonb — tag 20: the canonical text (jsonbOut → jsonbIn round-trips exactly, since the output
      // is canonical). Internal merge-sort scratch format only; a jsonb column can ride a spilling
      // sort as a carried (jsonb also a key) column, so it must spill faithfully.
      w.u8(20);
      w.bytesField(new TextEncoder().encode(jsonbOut(v.node)));
      break;
    case "jsonpath":
      // A derived/CTE input can carry this non-storable value through a blocking operator.
      w.u8(22);
      w.bytesField(new TextEncoder().encode(v.text));
      break;
    case "unfetched":
      // An untouched large-value reference rides along to the output unread (spill.md §4); spill it
      // opaquely so it round-trips, never resolving it. The same pass-through covers an
      // inline-deferred value (lazy-record.md §5b) — tag 21.
      switch (v.ref.form) {
        case 0x00:
          w.u8(21);
          w.bytesField(v.ref.comp ?? new Uint8Array(0));
          break;
        case 0x02:
          w.u8(9);
          w.u32(v.ref.firstPage);
          w.raw(v.ref.firstTag ?? NO_SPILL_TAG);
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
          w.raw(v.ref.firstTag ?? NO_SPILL_TAG);
          w.u32(v.ref.storedLen);
          w.u32(v.ref.rawLen);
          break;
      }
      break;
  }
}

// A spilled external value's first-page pointer carries its 16-byte tag (an encrypted file's — zero
// in a plaintext one; spec/fileformat/encrypted.md §2).
const NO_SPILL_TAG = new Uint8Array(16);

function readTag(r: SpillByteReader): Uint8Array | undefined {
  const tag = r.bytes(16).slice();
  return tag.some((b) => b !== 0) ? tag : undefined;
}

function readU32(r: SpillByteReader): number {
  const b = r.bytes(4);
  return (b[0]! | (b[1]! << 8) | (b[2]! << 16) | (b[3]! << 24)) >>> 0;
}

function readRow(r: SpillByteReader): Row {
  const ncols = readU32(r);
  const row: Row = new Array(ncols);
  for (let i = 0; i < ncols; i++) row[i] = readValue(r);
  return row;
}

// ignoreBOM keeps a leading U+FEFF in a reloaded text value (an ordinary code point, never a byte-order
// mark).
const SPILL_UTF8 = new TextDecoder("utf-8", { ignoreBOM: true });

function readValue(r: SpillByteReader): Value {
  const tag = r.byte();
  switch (tag) {
    case 0:
      return { kind: "null" };
    case 1:
      return { kind: "int", int: BigInt.asIntN(64, r.u64()) };
    case 2:
      return { kind: "bool", value: r.byte() !== 0 };
    case 3:
      return { kind: "text", text: SPILL_UTF8.decode(r.bytes(readU32(r))) };
    case 22:
      return { kind: "jsonpath", text: SPILL_UTF8.decode(r.bytes(readU32(r))) };
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
      return { kind: "timestamp", micros: BigInt.asIntN(64, r.u64()) };
    case 8:
      return { kind: "timestamptz", micros: BigInt.asIntN(64, r.u64()) };
    case 17:
      return { kind: "date", days: BigInt.asIntN(64, r.u64()) };
    case 9:
      // A reloaded deferred value carries SENTINEL handles (the run file cannot carry runtime
      // handles): it rides the sort output UNREAD by contract (spill.md §4), and touching one stays
      // the loud pre-B4 poison (resolveUnfetchedSelf).
      return {
        kind: "unfetched",
        ref: {
          form: 0x02,
          firstPage: readU32(r),
          firstTag: readTag(r),
          storedLen: readU32(r),
          rawLen: 0,
          comp: undefined,
          ty: sentinelTypeRef(),
          paging: null,
        },
      };
    case 10: {
      const rawLen = readU32(r);
      const comp = r.bytes(readU32(r));
      return {
        kind: "unfetched",
        ref: {
          form: 0x03,
          firstPage: 0,
          storedLen: 0,
          rawLen,
          comp,
          ty: sentinelTypeRef(),
          paging: null,
        },
      };
    }
    case 11:
      return {
        kind: "unfetched",
        ref: {
          form: 0x04,
          firstPage: readU32(r),
          firstTag: readTag(r),
          storedLen: readU32(r),
          rawLen: readU32(r),
          comp: undefined,
          ty: sentinelTypeRef(),
          paging: null,
        },
      };
    case 21: {
      const body = r.bytes(readU32(r));
      return {
        kind: "unfetched",
        ref: {
          form: 0x00,
          firstPage: 0,
          storedLen: 0,
          rawLen: 0,
          comp: body,
          ty: sentinelTypeRef(),
          paging: null,
        },
      };
    }
    case 12: {
      const months = readU32(r) | 0; // signed i32
      const days = readU32(r) | 0;
      const micros = BigInt.asIntN(64, r.u64()); // signed i64
      return { kind: "interval", iv: { months, days, micros } };
    }
    case 13: {
      const b = r.bytes(8);
      return float64Value(new DataView(b.buffer, b.byteOffset, b.byteLength).getFloat64(0, true));
    }
    case 14: {
      const b = r.bytes(4);
      return float32Value(new DataView(b.buffer, b.byteOffset, b.byteLength).getFloat32(0, true));
    }
    case 15: {
      const n = readU32(r);
      const fields: Value[] = new Array(n);
      for (let i = 0; i < n; i++) fields[i] = readValue(r);
      return { kind: "composite", fields };
    }
    case 16: {
      const ndim = readU32(r);
      const dims: number[] = new Array(ndim);
      const lbounds: number[] = new Array(ndim);
      let n = ndim === 0 ? 0 : 1;
      for (let d = 0; d < ndim; d++) {
        dims[d] = readU32(r);
        lbounds[d] = readU32(r) | 0;
        n *= dims[d]!;
      }
      const elements: Value[] = new Array(n);
      for (let i = 0; i < n; i++) elements[i] = readValue(r);
      return { kind: "array", dims, lbounds, elements };
    }
    case 18: {
      const flags = r.byte();
      if ((flags & 0x01) !== 0) return emptyRangeValue();
      const lbInf = (flags & 0x02) !== 0;
      const ubInf = (flags & 0x04) !== 0;
      const lower = lbInf ? null : readValue(r);
      const upper = ubInf ? null : readValue(r);
      return rangeValue(lower, upper, (flags & 0x08) !== 0, (flags & 0x10) !== 0);
    }
    case 19:
      return jsonValue(SPILL_UTF8.decode(r.bytes(readU32(r))));
    case 20:
      return jsonbValue(jsonbIn(SPILL_UTF8.decode(r.bytes(readU32(r)))));
    default:
      throw new Error("bad spill value tag");
  }
}

// The same internal value codec serves repeatable operator spools.
export function encodeSpillRow(row: Row): Uint8Array {
  // Most spooled rows are small; one is encoded per spilled row.
  const writer = new ByteWriter(64);
  writeRow(writer, row);
  return writer.result();
}
export function decodeSpillRow(bytes: Uint8Array): Row {
  let position = 0;
  const reader: SpillByteReader = {
    byte: () => bytes[position++]!,
    bytes: (n) => {
      const out = bytes.slice(position, position + n);
      position += n;
      return out;
    },
    u64: () => {
      const n = new DataView(bytes.buffer, bytes.byteOffset + position, 8).getBigInt64(0, true);
      position += 8;
      return n;
    },
    close: () => {},
  };
  return readRow(reader);
}
