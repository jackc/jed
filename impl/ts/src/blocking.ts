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
import { KeyMap, KeySet, keyHash } from "./keyset.ts";
import { hashRowBytes, keyBytes, rowBytes } from "./memsize.ts";
import { decodeSpillRow, encodeSpillRow, type SpillScratch, type SpillSink } from "./spill.ts";
import type { Row } from "./storage.ts";

// Appended scratch bytes gather in a buffer, written once it fills; the buffer starts small and doubles
// to this fixed bound, so a structure holds one bounded I/O buffer however much it spills.
const TAIL_BYTES = 64 * 1024;
// A sequential reader takes scratch in chunks of this many bytes.
const READ_CHUNK = 64 * 1024;

// BufferedScratch is a spilled structure's scratch file with its appends buffered. Reads and rewrites
// see buffered bytes as if written.
class BufferedScratch {
  private file: SpillScratch;
  // File bytes [tailStart, tailStart + tailLength) are still in tail, unwritten.
  private tail = new Uint8Array(4096);
  private tailStart = 0;
  private tailLength = 0;
  constructor(file: SpillScratch) {
    this.file = file;
  }
  get end(): number {
    return this.tailStart + this.tailLength;
  }
  private flush(): void {
    if (this.tailLength === 0) return;
    this.file.write(this.tailStart, this.tail.subarray(0, this.tailLength));
    this.tailStart += this.tailLength;
    this.tailLength = 0;
  }
  // read is a copy of bytes [at, at + length), all before end.
  read(at: number, length: number): Uint8Array {
    if (at >= this.tailStart)
      return this.tail.slice(at - this.tailStart, at - this.tailStart + length);
    if (at + length > this.tailStart) this.flush();
    return this.file.read(at, length);
  }
  // overwrite replaces bytes already written.
  overwrite(at: number, bytes: Uint8Array): void {
    if (at >= this.tailStart) {
      this.tail.set(bytes, at - this.tailStart);
      return;
    }
    if (at + bytes.length > this.tailStart) this.flush();
    this.file.write(at, bytes);
  }
  // append adds bytes at the end and returns their offset.
  append(bytes: Uint8Array): number {
    const at = this.end;
    const need = this.tailLength + bytes.length;
    if (need > this.tail.length && this.tail.length < TAIL_BYTES) {
      const grown = new Uint8Array(Math.min(TAIL_BYTES, Math.max(need, this.tail.length * 2)));
      grown.set(this.tail.subarray(0, this.tailLength));
      this.tail = grown;
    }
    if (need > this.tail.length) {
      this.flush();
      if (bytes.length > this.tail.length) {
        this.file.write(at, bytes);
        this.tailStart += bytes.length;
        return at;
      }
    }
    this.tail.set(bytes, this.tailLength);
    this.tailLength += bytes.length;
    return at;
  }
  // reserve adds `length` zero bytes at the end and returns their offset. Writing the last eight
  // extends the file over them; the rest read as zeros.
  reserve(length: number): number {
    this.flush();
    const at = this.tailStart;
    this.file.write(at + length - 8, new Uint8Array(8));
    this.tailStart += length;
    return at;
  }
  close(): void {
    this.tail = new Uint8Array(0);
    this.tailLength = 0;
    this.file.close();
  }
}

// ChunkReader reads a scratch region front to back a chunk at a time. A view it returns is valid until
// the next call.
class ChunkReader {
  private scratch: BufferedScratch;
  private limit: number;
  private chunk: Uint8Array = new Uint8Array(0);
  private chunkStart = 0;
  constructor(scratch: BufferedScratch, limit: number) {
    this.scratch = scratch;
    this.limit = limit;
  }
  bytes(at: number, length: number): Uint8Array {
    const from = at - this.chunkStart;
    if (from < 0 || from + length > this.chunk.length) {
      this.chunkStart = at;
      this.chunk = this.scratch.read(at, Math.max(length, Math.min(READ_CHUNK, this.limit - at)));
      return this.chunk.subarray(0, length);
    }
    return this.chunk.subarray(from, from + length);
  }
}

function u32At(bytes: Uint8Array, at: number): number {
  return new DataView(bytes.buffer, bytes.byteOffset, bytes.length).getUint32(at, true);
}
function f64At(bytes: Uint8Array, at: number): number {
  return new DataView(bytes.buffer, bytes.byteOffset, bytes.length).getFloat64(at, true);
}
function f64(value: number): Uint8Array {
  const out = new Uint8Array(8);
  new DataView(out.buffer).setFloat64(0, value, true);
  return out;
}
// A record is a u32 length and an encoded row, after `prefix` bytes the caller fills.
function record(row: Row, prefix = 0): Uint8Array {
  const bytes = encodeSpillRow(row);
  const out = new Uint8Array(prefix + 4 + bytes.length);
  new DataView(out.buffer).setUint32(prefix, bytes.length, true);
  out.set(bytes, prefix + 4);
  return out;
}

export class RowSpool implements Iterable<Row> {
  private rows: Row[] = [];
  private bytes = 0;
  private file: BufferedScratch | null = null;
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
      this.file.append(record(row));
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
      this.file = new BufferedScratch(this.sink.createScratch!());
      for (const buffered of this.rows) this.file.append(record(buffered));
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
    const size = this.file.end;
    const reader = new ChunkReader(this.file, size);
    for (let at = 0; at < size; ) {
      const length = u32At(reader.bytes(at, 4), 0);
      yield decodeSpillRow(reader.bytes(at + 4, length));
      at += 4 + length;
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

const textEncoder = new TextEncoder();
function equalBytes(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  let i = 0;
  // Compare by 32-bit words where both views allow it; a spilled key can be megabytes long.
  if ((a.byteOffset & 3) === 0 && (b.byteOffset & 3) === 0) {
    const words = a.length >>> 2;
    const wa = new Int32Array(a.buffer, a.byteOffset, words);
    const wb = new Int32Array(b.buffer, b.byteOffset, words);
    for (; i < words; i++) if (wa[i] !== wb[i]) return false;
    i = words << 2;
  }
  for (; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

// A spilled SpillMap's scratch is a linear-hashing table. Placement is private: lookups compare the
// complete canonical key, and callers keep their own visitation order.
//   directory: bucket heads (f64 node offsets, 0 = empty) in segments. Segment 0 holds
//     INITIAL_BUCKETS heads at offset 0; segment k ≥ 1 holds INITIAL_BUCKETS·2^(k-1) heads and is
//     reserved at the end of the file when a split first needs it. Only the segment offsets are
//     resident, at most MAX_SEGMENTS of them, so the metadata stays fixed however many keys spill.
//   node: next f64 | value block offset f64 | value capacity u32 | key length u32 | hash u32 | 0 u32 |
//     key bytes | its first value block
//   value block: length u32 | encoded row, in a reserved capacity a replacement reuses when it fits.
// Each new key may split one bucket, so chains average at most MAX_LOAD nodes and a lookup reads one
// head and about one node. A skewed bucket (one hash for every key) still streams one node at a time.
const INITIAL_BUCKETS = 4096;
// 4096·2^20 = 2^32 buckets: every 32-bit hash has its own, so splitting stops there.
const MAX_SEGMENTS = 21;
const MAX_LOAD = 1;
const NODE_HEADER = 32;
// A node read also takes this many bytes past the key, so a small first value block arrives with it.
const PEEK = 64;
// Keys up to this many bytes are read with their node's header.
const INLINE_KEY = 4096;

// valueBlock is a value block holding `encoded` in `capacity` bytes.
function valueBlock(encoded: Uint8Array, capacity: number): Uint8Array {
  const block = new Uint8Array(4 + capacity);
  new DataView(block.buffer).setUint32(0, encoded.length, true);
  block.set(encoded, 4);
  return block;
}
// mix32 avalanches keyHash's bits, so linear hashing's low-bit bucket addresses spread evenly.
function mix32(hash: number): number {
  hash ^= hash >>> 16;
  hash = Math.imul(hash, 0x85ebca6b);
  hash ^= hash >>> 13;
  hash = Math.imul(hash, 0xc2b2ae35);
  hash ^= hash >>> 16;
  return hash >>> 0;
}

// A key's place in a spilled SpillMap: its encoded bytes, hash, bucket and that bucket's head, and its
// node (0 while absent) with the node's value block offset, capacity, and encoded value once read.
type Located = {
  key: string;
  bytes: Uint8Array;
  hash: number;
  bucket: number;
  head: number;
  node: number;
  valueAt: number;
  capacity: number;
  value: Uint8Array | null;
};

// Existing keys rewrite their value block in place, or move it when it outgrows its capacity, so
// repeated updates never grow a resident directory or a chain of stale versions. A single value can
// exceed work_mem.
export class SpillMap {
  // Each resident entry keeps its measured key and value bytes, so a replacement charges the growth.
  private memory = new KeyMap<{ value: Row; keyLen: number; valueLen: number }>();
  private bytes = 0;
  private file: BufferedScratch | null = null;
  private sink: SpillSink;
  private budget: number;
  // The resident entries' query-memory charge, returned on spill or discard.
  private charge: StateCharge;
  // The spilled table has base + split buckets; each bucket below split has been split into itself
  // and itself + base. keys counts the spilled keys.
  private base = INITIAL_BUCKETS;
  private split = 0;
  private keys = 0;
  private segments: number[] = [];
  // The last disk lookup, so a get then set of one key (a GROUP BY state update, an insert) encodes,
  // hashes, and walks its chain once.
  private located: Located | null = null;
  constructor(budget: number, sink: SpillSink, charge: StateCharge = new StateCharge()) {
    this.budget = budget;
    this.sink = sink;
    this.charge = charge;
  }
  private headAt(bucket: number): number {
    if (bucket < INITIAL_BUCKETS) return bucket * 8;
    const segment = 32 - Math.clz32(Math.floor(bucket / INITIAL_BUCKETS));
    return this.segments[segment]! + (bucket - INITIAL_BUCKETS * 2 ** (segment - 1)) * 8;
  }
  private head(bucket: number): number {
    return f64At(this.file!.read(this.headAt(bucket), 8), 0);
  }
  private address(hash: number): number {
    const bucket = hash % this.base;
    return bucket < this.split ? hash % (this.base * 2) : bucket;
  }
  private locate(key: string): Located {
    const last = this.located;
    if (last !== null && last.key === key) return last;
    const file = this.file!;
    const bytes = textEncoder.encode(key);
    const hash = mix32(keyHash(key));
    const bucket = this.address(hash);
    const head = this.head(bucket);
    const loc: Located = {
      key,
      bytes,
      hash,
      bucket,
      head,
      node: 0,
      valueAt: 0,
      capacity: 0,
      value: null,
    };
    const end = file.end;
    // A short key's node read takes the header, a key of this length, and often the first value
    // block; a long key is read only when the hash and length match.
    const inline = bytes.length <= INLINE_KEY;
    for (let node = head; node !== 0; ) {
      const want = inline ? NODE_HEADER + bytes.length + PEEK : NODE_HEADER;
      const found = file.read(node, Math.min(want, end - node));
      const view = new DataView(found.buffer, found.byteOffset, found.length);
      if (
        view.getUint32(24, true) === hash &&
        view.getUint32(20, true) === bytes.length &&
        equalBytes(
          inline
            ? found.subarray(NODE_HEADER, NODE_HEADER + bytes.length)
            : file.read(node + NODE_HEADER, bytes.length),
          bytes,
        )
      ) {
        loc.node = node;
        loc.valueAt = view.getFloat64(8, true);
        loc.capacity = view.getUint32(16, true);
        const within = loc.valueAt - node;
        if (within + 4 <= found.length) {
          const length = view.getUint32(within, true);
          if (within + 4 + length <= found.length)
            loc.value = found.subarray(within + 4, within + 4 + length);
        }
        break;
      }
      node = view.getFloat64(0, true);
    }
    this.located = loc;
    return loc;
  }
  get(key: string): Row | undefined {
    if (this.file === null) return this.memory.get(key)?.value;
    const loc = this.locate(key);
    if (loc.node === 0) return undefined;
    if (loc.value === null) {
      const block = this.file.read(loc.valueAt, 4 + loc.capacity);
      loc.value = block.subarray(4, 4 + u32At(block, 0));
    }
    return decodeSpillRow(loc.value);
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
      this.file = new BufferedScratch(this.sink.createScratch!());
      this.segments.push(this.file.reserve(INITIAL_BUCKETS * 8));
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
    const loc = this.locate(key);
    const encoded = encodeSpillRow(value);
    loc.value = encoded;
    if (loc.node !== 0) {
      if (encoded.length <= loc.capacity) {
        file.overwrite(loc.valueAt, valueBlock(encoded, encoded.length));
        return;
      }
      // The value outgrew its block: move it to a new one with room to grow.
      loc.capacity = encoded.length + (encoded.length >>> 1);
      loc.valueAt = file.append(valueBlock(encoded, loc.capacity));
      const pointer = new Uint8Array(12);
      const view = new DataView(pointer.buffer);
      view.setFloat64(0, loc.valueAt, true);
      view.setUint32(8, loc.capacity, true);
      file.overwrite(loc.node + 8, pointer);
      return;
    }
    const keyLength = loc.bytes.length;
    const node = new Uint8Array(NODE_HEADER + keyLength + 4 + encoded.length);
    const view = new DataView(node.buffer);
    const at = file.end;
    view.setFloat64(0, loc.head, true);
    view.setFloat64(8, at + NODE_HEADER + keyLength, true);
    view.setUint32(16, encoded.length, true);
    view.setUint32(20, keyLength, true);
    view.setUint32(24, loc.hash, true);
    node.set(loc.bytes, NODE_HEADER);
    view.setUint32(NODE_HEADER + keyLength, encoded.length, true);
    node.set(encoded, NODE_HEADER + keyLength + 4);
    file.append(node);
    file.overwrite(this.headAt(loc.bucket), f64(at));
    loc.head = loc.node = at;
    loc.valueAt = at + NODE_HEADER + keyLength;
    loc.capacity = encoded.length;
    if (++this.keys > MAX_LOAD * (this.base + this.split)) this.splitBucket();
  }
  // splitBucket divides bucket `split` between itself and split + base by the next hash bit, keeping
  // each node's relative order and rewriting only the links that change.
  private splitBucket(): void {
    const file = this.file!;
    const from = this.split;
    const to = this.base + from;
    if (to === INITIAL_BUCKETS * 2 ** (this.segments.length - 1)) {
      if (this.segments.length === MAX_SEGMENTS) return;
      this.segments.push(file.reserve(to * 8));
    }
    const original = this.head(from);
    // Each list's first node, last node, and that last node's link as written.
    const lists = [
      { first: 0, last: 0, link: 0 },
      { first: 0, last: 0, link: 0 },
    ];
    for (let node = original; node !== 0; ) {
      const header = file.read(node, NODE_HEADER);
      const next = f64At(header, 0);
      const list = lists[u32At(header, 24) % (this.base * 2) === from ? 0 : 1]!;
      if (list.last === 0) list.first = node;
      else if (list.link !== node) file.overwrite(list.last, f64(node));
      list.last = node;
      list.link = next;
      node = next;
    }
    for (const list of lists)
      if (list.last !== 0 && list.link !== 0) file.overwrite(list.last, f64(0));
    if (lists[0]!.first !== original) file.overwrite(this.headAt(from), f64(lists[0]!.first));
    if (lists[1]!.first !== 0) file.overwrite(this.headAt(to), f64(lists[1]!.first));
    if (++this.split === this.base) {
      this.base *= 2;
      this.split = 0;
    }
    // Bucket addresses moved.
    this.located = null;
  }
  // release returns the resident charge now — the map is discarded at this stage boundary.
  release(): void {
    this.charge.releaseAll();
  }
  close(): void {
    this.memory.clear();
    this.file?.close();
    this.file = null;
    this.located = null;
    this.charge.releaseAll();
  }
}

// An insertion-ordered multimap used for hash JOIN buckets. Row chains are external immediately;
// the bounded map holds only first/last offsets. A skewed bucket never becomes a match-index array.
export class SpillMultiMap {
  // The disk-chain index (used only once spilled): uncharged, since a spilled table reserves nothing.
  private index: SpillMap;
  private file: BufferedScratch | null = null;
  private memory = new KeyMap<Row[]>();
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
      this.file = new BufferedScratch(this.sink.createScratch!());
      // Offset 0 ends a chain, so no row record starts there.
      this.file.append(new Uint8Array(8));
      // The rows left memory: the table's whole resident charge goes with them.
      this.charge.releaseAll();
      for (const [k, bucket] of this.memory) for (const r of bucket) this.appendDisk(k, r);
      this.memory.clear();
      this.bytes = 0;
      return;
    }
    this.appendDisk(key, row);
  }
  // Each chained row is a next-row link (f64, 0 at the end) and a record.
  private appendDisk(key: string, row: Row): void {
    const file = this.file!;
    const previous = this.index.get(key);
    const at = file.append(record(row, 8));
    const first = previous === undefined ? at : Number((previous[0]! as { int: bigint }).int);
    if (previous !== undefined)
      file.overwrite(Number((previous[1]! as { int: bigint }).int), f64(at));
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
      const header = this.file.read(at, 12);
      yield decodeSpillRow(this.file.read(at + 12, u32At(header, 8)));
      at = f64At(header, 0);
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
  // merge stably merges two adjacent runs (a earlier) into a fresh run. Each head read back from a run
  // reserves its rowBytes on a sibling of the sorter's charge while it is in the merge (memory.md
  // §6.8): the replacement head is reserved before the leaving head is released. The merge cannot
  // spill further, so a head the account rejects is held uncharged.
  private merge(a: RowSpool, b: RowSpool): RowSpool {
    const out = this.fresh();
    const heads = this.charge.sibling();
    const ai = a[Symbol.iterator]();
    const bi = b[Symbol.iterator]();
    let av = ai.next();
    let ac = av.done ? 0 : chargeHead(heads, av.value);
    let bv = bi.next();
    let bc = bv.done ? 0 : chargeHead(heads, bv.value);
    while (!av.done || !bv.done) {
      if (bv.done || (!av.done && this.compare(av.value, bv.value) <= 0)) {
        const row = av.value!;
        const held = ac;
        av = ai.next();
        ac = av.done ? 0 : chargeHead(heads, av.value);
        heads.release(held);
        out.push(row);
      } else {
        const row = bv.value;
        const held = bc;
        bv = bi.next();
        bc = bv.done ? 0 : chargeHead(heads, bv.value);
        heads.release(held);
        out.push(row);
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
    }
  }
  private flush(): void {
    this.chunk.sort(this.compare);
    let run = this.fresh();
    for (const row of this.chunk) run.push(row);
    this.chunk = [];
    this.bytes = 0;
    // The run left memory: its rows' charge goes with it (memory.md §6.4/§6.6), before any compaction
    // merge reserves its heads.
    this.charge.releaseAll();
    let level = 0;
    while (this.levels[level] !== undefined) {
      run = this.merge(this.levels[level]!, run);
      this.levels[level] = undefined;
      level++;
    }
    this.levels[level] = run;
  }
  // finishSpool returns the sorted output: the final chunk, stable-sorted, stays in memory (still charged
  // with the sorter) and is k-way merged with the spilled runs, earliest input first (memory.md §6.8).
  finishSpool(): SortedSpool {
    this.chunk.sort(this.compare);
    const runs: RowSpool[] = [];
    for (let level = this.levels.length - 1; level >= 0; level--) {
      const run = this.levels[level];
      if (run !== undefined) {
        runs.push(run);
        this.owned.delete(run);
      }
    }
    this.levels = [];
    const mem = this.chunk;
    this.chunk = [];
    this.bytes = 0;
    return new SortedSpool(runs, mem, this.compare, this.charge.sibling());
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
  let sorted: SortedSpool | null = null;
  try {
    for (const row of input) sorter.push(row);
    sorted = sorter.finishSpool();
    const out = new RowSpool(budget, sink);
    for (const row of sorted) out.push(row);
    return out;
  } finally {
    sorted?.close();
    sorter.close();
  }
}

// chargeHead reserves a merge head read back from a spilled run if it fits, returning the bytes it
// holds: zero when the account is unlimited or rejects it (memory.md §6.8).
function chargeHead(heads: StateCharge, row: Row): number {
  if (!heads.active()) return 0;
  const bytes = rowBytes(row);
  return heads.tryReserveDirect(bytes) ? bytes : 0;
}

type MergeHead = { row: Row; source: number; charged: number };

// SortedSpool is a SpoolSorter's sorted output, read once: a k-way merge of the spilled runs (earliest
// input first) and the final in-memory chunk last, ties broken by the lower source — exactly input
// order. The heads read back from runs reserve their rowBytes while they are in the merge heap; the
// in-memory chunk's rows are already charged with the sorter, so its heads reserve nothing (memory.md
// §6.8). The heads are read (and reserved) when the output is built, as the merge is primed. close
// discards the runs and returns any head still charged.
export class SortedSpool implements Iterable<Row> {
  private runs: RowSpool[];
  private mem: Row[];
  private compare: (a: Row, b: Row) => number;
  private heads: StateCharge;
  private sources: Iterator<Row>[] = [];
  private heap: MergeHead[] = [];
  constructor(
    runs: RowSpool[],
    mem: Row[],
    compare: (a: Row, b: Row) => number,
    heads: StateCharge,
  ) {
    this.runs = runs;
    this.mem = mem;
    this.compare = compare;
    this.heads = heads;
    if (runs.length === 0) return;
    this.sources = runs.map((run) => run[Symbol.iterator]());
    this.sources.push(mem[Symbol.iterator]());
    // Read each source's first row into the heap, in source order.
    for (let i = 0; i < this.sources.length; i++) {
      const next = this.sources[i]!.next();
      if (!next.done)
        this.heapPush(this.heap, {
          row: next.value,
          source: i,
          charged: this.charge(i, next.value),
        });
    }
  }
  // charge reserves a head from source if it is a spilled run's (the in-memory chunk's hold nothing).
  private charge(source: number, row: Row): number {
    return source === this.sources.length - 1 ? 0 : chargeHead(this.heads, row);
  }
  *[Symbol.iterator](): Generator<Row> {
    if (this.runs.length === 0) {
      yield* this.mem;
      return;
    }
    const sources = this.sources;
    const heap = this.heap;
    const charge = (source: number, row: Row): number => this.charge(source, row);
    while (heap.length > 0) {
      // The popped head's source replaces it (reserved first), then the popped head's charge is
      // released as the row leaves the merge.
      const top = this.heapPop(heap);
      const next = sources[top.source]!.next();
      if (!next.done) {
        this.heapPush(heap, {
          row: next.value,
          source: top.source,
          charged: charge(top.source, next.value),
        });
      }
      this.heads.release(top.charged);
      yield top.row;
    }
  }
  close(): void {
    for (const run of this.runs) run.close();
    this.runs = [];
    this.mem = [];
    this.heads.releaseAll();
  }
  // less(a, b) is true when a comes out first: smaller by the order keys, ties by lower source.
  private less(a: MergeHead, b: MergeHead): boolean {
    const c = this.compare(a.row, b.row);
    if (c !== 0) return c < 0;
    return a.source < b.source;
  }
  private heapPush(h: MergeHead[], item: MergeHead): void {
    h.push(item);
    let i = h.length - 1;
    while (i > 0) {
      const parent = (i - 1) >> 1;
      if (!this.less(h[i]!, h[parent]!)) break;
      [h[i], h[parent]] = [h[parent]!, h[i]!];
      i = parent;
    }
  }
  private heapPop(h: MergeHead[]): MergeHead {
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

// SpillSet is a DISTINCT dedup set: an in-memory set (no scratch or unlimited work_mem), or a
// spill-capable SpillMap. Either way each first occurrence reserves its entry, ENTRY + key_bytes(row),
// while resident (memory.md §6.2/§6.6).
export class SpillSet {
  private resident: KeySet | null;
  private disk: SpillMap | null;
  private charge: StateCharge;
  constructor(budget: number, sink: SpillSink | null, charge: StateCharge = new StateCharge()) {
    this.charge = charge;
    this.disk =
      budget > 0 && sink?.createScratch !== undefined ? new SpillMap(budget, sink, charge) : null;
    this.resident = this.disk === null ? new KeySet() : null;
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
