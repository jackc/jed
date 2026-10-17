// The serving owner's shared state (spec/design/page-service.md §14 "Native TS on Node"): the state the
// engine thread and the service worker both touch, in one SharedArrayBuffer guarded by a small
// Atomics lock. It holds the committed txid and the meta page that published it, the written-index
// window (spec/pageservice/README.md §4), the pin table (every client connection's pins, counted per
// txid — part of the owner's reader watermark, transactions.md §8), and a file generation the engine
// thread bumps when compaction replaces the file, so the worker reopens its read handle.
//
// Browser-safe: no node: imports, so it is unit-testable on its own; only the Node owner uses it.
//
// The window is a bounded ring of at most PS_INVALIDATION_WINDOW_COMMITS commits whose indices share a
// fixed arena. When a commit's indices do not fit, the oldest commits are dropped (their txids then
// answer "invalidate all", which README §3.2 permits for anything older than the retained window); a
// single commit larger than the arena empties the window.

import { PS_INVALIDATION_WINDOW_COMMITS, PS_MAX_INVALIDATION_ENTRIES } from "./codec.ts";

// The pin table holds this many distinct pinned txids at once; a begin_read that needs one more slot
// is refused (54000), so the table never grows past its buffer.
export const PIN_SLOTS = 4096;
// The window's index arena, in u32 entries (1 MiB).
export const WINDOW_ARENA = 1 << 18;
const META_MAX = 65536;

// Int32 header slots.
const LOCK = 0;
const FILE_GENERATION = 1;
const WINDOW_ENTRIES = 2;
const ARENA_USED = 3;
const META_LENGTH = 4;
const HEADER_I32 = 16;

// Byte offsets of the other regions (each 8-byte aligned).
const BIG_OFFSET = HEADER_I32 * 4; // committed txid, window start
const ENTRY_TXID_OFFSET = BIG_OFFSET + 16;
const ENTRY_START_OFFSET = ENTRY_TXID_OFFSET + PS_INVALIDATION_WINDOW_COMMITS * 8;
const ENTRY_COUNT_OFFSET = ENTRY_START_OFFSET + PS_INVALIDATION_WINDOW_COMMITS * 4;
const PIN_TXID_OFFSET = ENTRY_COUNT_OFFSET + PS_INVALIDATION_WINDOW_COMMITS * 4;
const PIN_COUNT_OFFSET = PIN_TXID_OFFSET + PIN_SLOTS * 8;
const ARENA_OFFSET = PIN_COUNT_OFFSET + PIN_SLOTS * 4;
const META_OFFSET = ARENA_OFFSET + WINDOW_ARENA * 4;
const TOTAL_BYTES = META_OFFSET + META_MAX;

// Invalidation is begin_read's cache answer: everything, or the exact ascending index list.
export type Invalidation = { all: boolean; indices: number[] };

export class ServiceState {
  readonly buffer: SharedArrayBuffer;
  private readonly header: Int32Array;
  private readonly big: BigInt64Array;
  private readonly entryTxid: BigInt64Array;
  private readonly entryStart: Int32Array;
  private readonly entryCount: Int32Array;
  private readonly pinTxid: BigInt64Array;
  private readonly pinCount: Int32Array;
  private readonly arena: Uint32Array;
  private readonly metaBytes: Uint8Array;

  constructor(buffer: SharedArrayBuffer = new SharedArrayBuffer(TOTAL_BYTES)) {
    this.buffer = buffer;
    this.header = new Int32Array(buffer, 0, HEADER_I32);
    this.big = new BigInt64Array(buffer, BIG_OFFSET, 2);
    this.entryTxid = new BigInt64Array(buffer, ENTRY_TXID_OFFSET, PS_INVALIDATION_WINDOW_COMMITS);
    this.entryStart = new Int32Array(buffer, ENTRY_START_OFFSET, PS_INVALIDATION_WINDOW_COMMITS);
    this.entryCount = new Int32Array(buffer, ENTRY_COUNT_OFFSET, PS_INVALIDATION_WINDOW_COMMITS);
    this.pinTxid = new BigInt64Array(buffer, PIN_TXID_OFFSET, PIN_SLOTS);
    this.pinCount = new Int32Array(buffer, PIN_COUNT_OFFSET, PIN_SLOTS);
    this.arena = new Uint32Array(buffer, ARENA_OFFSET, WINDOW_ARENA);
    this.metaBytes = new Uint8Array(buffer, META_OFFSET, META_MAX);
  }

  // lock takes the state lock, sleeping in Atomics.wait while another thread holds it (no spin).
  lock(): void {
    while (Atomics.compareExchange(this.header, LOCK, 0, 1) !== 0) {
      Atomics.wait(this.header, LOCK, 1);
    }
  }

  unlock(): void {
    Atomics.store(this.header, LOCK, 0);
    Atomics.notify(this.header, LOCK, 1);
  }

  // fileGeneration changes each time compaction replaces the file; read without the lock.
  fileGeneration(): number {
    return Atomics.load(this.header, FILE_GENERATION);
  }

  // --- the rest is called with the lock held ---

  committedTxidLocked(): bigint {
    return this.big[0]!;
  }

  // metaLocked copies the committed meta page.
  metaLocked(): Uint8Array {
    return this.metaBytes.slice(0, this.header[META_LENGTH]);
  }

  // pageCountLocked is the committed meta's page_count (offset 24, spec/fileformat/format.md).
  pageCountLocked(): number {
    const m = this.metaBytes;
    return ((m[24]! << 24) | (m[25]! << 16) | (m[26]! << 8) | m[27]!) >>> 0;
  }

  // resetLocked starts the window at txid with meta (an owner's open, or a compaction — which also
  // bumps the file generation so the worker reopens its read handle).
  resetLocked(txid: bigint, meta: Uint8Array, compacted: boolean): void {
    this.setMeta(txid, meta);
    this.big[1] = txid;
    this.header[WINDOW_ENTRIES] = 0;
    this.header[ARENA_USED] = 0;
    if (compacted) Atomics.add(this.header, FILE_GENERATION, 1);
  }

  // publishLocked records a main-database commit: its txid, meta, and written indices (README §4).
  publishLocked(txid: bigint, meta: Uint8Array, written: number[]): void {
    this.setMeta(txid, meta);
    const indices = [...new Set(written)].sort((a, b) => a - b);
    if (indices.length > WINDOW_ARENA) {
      this.big[1] = txid; // nothing older is answerable exactly any more
      this.header[WINDOW_ENTRIES] = 0;
      this.header[ARENA_USED] = 0;
      return;
    }
    // Drop the oldest commits until both the entry ring and the arena have room.
    let entries = this.header[WINDOW_ENTRIES]!;
    let used = this.header[ARENA_USED]!;
    let drop = 0;
    let freed = 0;
    while (
      drop < entries &&
      (entries - drop >= PS_INVALIDATION_WINDOW_COMMITS ||
        used - freed + indices.length > WINDOW_ARENA)
    ) {
      freed += this.entryCount[drop]!;
      drop++;
    }
    if (drop > 0) {
      this.big[1] = this.entryTxid[drop - 1]!;
      this.arena.copyWithin(0, freed, used);
      for (let i = drop; i < entries; i++) {
        this.entryTxid[i - drop] = this.entryTxid[i]!;
        this.entryStart[i - drop] = this.entryStart[i]! - freed;
        this.entryCount[i - drop] = this.entryCount[i]!;
      }
      entries -= drop;
      used -= freed;
    }
    this.arena.set(indices, used);
    this.entryTxid[entries] = txid;
    this.entryStart[entries] = used;
    this.entryCount[entries] = indices.length;
    this.header[WINDOW_ENTRIES] = entries + 1;
    this.header[ARENA_USED] = used + indices.length;
  }

  // invalidationLocked answers begin_read for a cache from (sameIncarnation, lastTxid) when the pinned
  // snapshot is txid (README §3.2).
  invalidationLocked(sameIncarnation: boolean, lastTxid: bigint, txid: bigint): Invalidation {
    if (!sameIncarnation || lastTxid === 0n || lastTxid > txid || lastTxid < this.big[1]!) {
      return { all: true, indices: [] };
    }
    const seen = new Set<number>();
    const entries = this.header[WINDOW_ENTRIES]!;
    for (let e = 0; e < entries; e++) {
      if (this.entryTxid[e]! <= lastTxid) continue;
      const start = this.entryStart[e]!;
      const end = start + this.entryCount[e]!;
      for (let i = start; i < end; i++) seen.add(this.arena[i]!);
      if (seen.size > PS_MAX_INVALIDATION_ENTRIES) return { all: true, indices: [] };
    }
    return { all: false, indices: [...seen].sort((a, b) => a - b) };
  }

  // pinLocked adds one pin on txid; false when the table has no free slot.
  pinLocked(txid: bigint): boolean {
    let free = -1;
    for (let i = 0; i < PIN_SLOTS; i++) {
      if (this.pinCount[i]! > 0 && this.pinTxid[i] === txid) {
        this.pinCount[i]!++;
        return true;
      }
      if (free < 0 && this.pinCount[i] === 0) free = i;
    }
    if (free < 0) return false;
    this.pinTxid[free] = txid;
    this.pinCount[free] = 1;
    return true;
  }

  // unpinLocked releases n pins on txid.
  unpinLocked(txid: bigint, n: number): void {
    for (let i = 0; i < PIN_SLOTS; i++) {
      if (this.pinCount[i]! > 0 && this.pinTxid[i] === txid) {
        this.pinCount[i] = Math.max(0, this.pinCount[i]! - n);
        return;
      }
    }
  }

  // oldestPinnedLocked is the oldest txid any connection pins, or null when none does.
  oldestPinnedLocked(): bigint | null {
    let oldest: bigint | null = null;
    for (let i = 0; i < PIN_SLOTS; i++) {
      if (this.pinCount[i]! > 0 && (oldest === null || this.pinTxid[i]! < oldest)) {
        oldest = this.pinTxid[i]!;
      }
    }
    return oldest;
  }

  private setMeta(txid: bigint, meta: Uint8Array): void {
    this.big[0] = txid;
    this.metaBytes.set(meta);
    this.header[META_LENGTH] = meta.length;
  }
}
