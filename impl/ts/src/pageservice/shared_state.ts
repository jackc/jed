// The serving owner's shared state (spec/design/page-service.md §14 "Native TS on Node"): the state the
// engine thread and the service worker both touch, in one SharedArrayBuffer guarded by a small
// Atomics lock. It holds the committed txid and the meta page that published it, the written-index
// window and the resolution window (spec/pageservice/README.md §4), the pin table (every client
// connection's pins, counted per txid — part of the owner's reader watermark, transactions.md §8), a
// file generation the engine thread bumps when compaction replaces the file, so the worker reopens its
// read handle, and the writer gate (README §4, §6.4) with the facts the worker needs to grant free-page
// reuse and to decide a maintenance commit: the free list's generation, the live count at the last
// reclamation, and the oldest version a local reader of the engine thread pinned at its last commit.
//
// The writer gate is one Int32 slot: GATE_FREE, or held by the engine thread (an owner write
// transaction, compaction, a maintenance commit), by the worker (a client write transaction), or handed
// by the worker to the engine thread for a maintenance commit (GATE_HANDED). The engine thread waits
// for it with Atomics.wait; the worker never blocks, it waits with Atomics.waitAsync. EPOCH changes
// whenever the worker publishes a client commit or hands over the gate, so an engine-thread transaction
// begin notices either with one atomic load.
//
// Browser-safe: no node: imports, so it is unit-testable on its own; only the Node owner uses it.
//
// Batched durability (spec/design/validated-cow.md §4) is mirrored here too: the floor (its slot and
// txid) and the pending set's entries (page index and CRC-64 digest), which the worker names to a
// relaxed client's begin_write and adds that client's commit to (spec/pageservice/README.md §3.5). The
// entries live in a fixed region the size of the largest inline manifest; a pending set is below the
// batch cap at every begin_write (a commit reaching it is flushed before the gate moves on), so only a
// commit in progress can overflow it, which marks the set unnamed until the flush that follows. A
// generation counter changes with every update, so the engine thread re-reads the set only then.
//
// The window is a bounded ring of at most PS_INVALIDATION_WINDOW_COMMITS commits whose indices share a
// fixed arena. When a commit's indices do not fit, the oldest commits are dropped (their txids then
// answer "invalidate all", which README §3.2 permits for anything older than the retained window); a
// single commit larger than the arena empties the window.

import {
  type ManifestEntry,
  manifestInlineCapacity,
  resolutionDigest,
} from "../commit_manifest.ts";
import type { BatchState } from "../pager.ts";
import {
  PS_COMMIT_STATUS_COMMITTED,
  PS_COMMIT_STATUS_NOT_COMMITTED,
  PS_COMMIT_STATUS_UNKNOWN,
  PS_INVALIDATION_WINDOW_COMMITS,
  PS_MAX_INVALIDATION_ENTRIES,
} from "./codec.ts";

// The pin table holds this many distinct pinned txids at once; a begin_read that needs one more slot
// is refused (54000), so the table never grows past its buffer.
export const PIN_SLOTS = 4096;
// The window's index arena, in u32 entries (1 MiB).
export const WINDOW_ARENA = 1 << 18;
const META_MAX = 65536;
// The resolution window holds the (txid, digest) pairs of this many published commits (README §4).
export const PS_RESOLUTION_WINDOW_COMMITS = PS_INVALIDATION_WINDOW_COMMITS;
// The pending-entries region holds the largest inline manifest of any supported page size.
export const PENDING_SLOTS = manifestInlineCapacity(65536);

// The owner's handoff states (spec/pageservice/README.md §6.5): serving; yielding to a requester
// (draining); closing (a graceful close's drain); releasing (the drain ended, ownership is being
// released — a requester's disconnect no longer cancels anything). While not serving, client begins
// are parked unanswered and the engine thread's own begins wait.
export const OWNER_SERVING = 0;
export const OWNER_YIELDING = 1;
export const OWNER_CLOSING = 2;
export const OWNER_RELEASING = 3;

// The commit-acknowledgement test hooks (README §6.4, §6.5): drop the connection, or end the process,
// instead of acknowledging the next client commit after it is durable.
export const ACK_NORMAL = 0;
export const ACK_HANG_UP = 1;
export const ACK_CRASH = 2;

// The writer gate's states.
export const GATE_FREE = 0;
export const GATE_ENGINE = 1;
export const GATE_WORKER = 2;
export const GATE_HANDED = 3;

// Int32 header slots.
const LOCK = 0;
const FILE_GENERATION = 1;
const WINDOW_ENTRIES = 2;
const ARENA_USED = 3;
const META_LENGTH = 4;
const GATE = 5;
const EPOCH = 6;
const POISONED = 7;
const RESOLUTION_COUNT = 8;
const RESOLUTION_NEXT = 9;
const ACK_HOOK = 10;
const OWNER_STATE = 11;
// Batched durability (spec/design/validated-cow.md §4): the meta slot holding the newest durable meta,
// and the pending set's entry count (PENDING_OVERFLOW when it outgrew the region). The engine thread
// publishes the batch after each own commit and flush; the worker flushes it before granting a
// synchronous client's begin_write, names it to a relaxed one, and records each client commit, always
// holding the writer gate. BATCH_LOCK guards the batch slots and region; BATCH_GENERATION changes with
// every update.
const FLOOR_SLOT = 12;
const PENDING_COUNT = 13;
// KEY_STATE is whether the database is encrypted (spec/fileformat/encrypted.md §14): 1 when KEY holds
// its data key, which the worker checks and reseals a client commit's meta with, and opens free-list
// pages with. The engine thread sets it at start and after a rekey.
const KEY_STATE = 14;
const BATCH_LOCK = 15;
const BATCH_GENERATION = 16;
const HEADER_I32 = 20;
const PENDING_OVERFLOW = -1;

// BigInt64 slots.
const COMMITTED_TXID = 0;
const WINDOW_START = 1;
const FREE_GEN_TXID = 2;
const LIVE_AT_COMPACTION = 3;
// ENGINE_OLDEST is the oldest version a local reader of the engine thread pinned when it last
// published, or -1 when none did. Readers that register later pin the committed version, which is never
// older than the free list's generation, so a stale (older) value only withholds a grant.
const ENGINE_OLDEST = 4;
// REQUESTER is the connection id a yielding owner yields to (0: none).
const REQUESTER = 5;
// FLOOR_TXID is the txid of the newest durable meta, in FLOOR_SLOT.
const FLOOR_TXID = 6;
const BIG_SLOTS = 8;

// Byte offsets of the other regions (each 8-byte aligned).
const BIG_OFFSET = HEADER_I32 * 4;
const ENTRY_TXID_OFFSET = BIG_OFFSET + BIG_SLOTS * 8;
const ENTRY_START_OFFSET = ENTRY_TXID_OFFSET + PS_INVALIDATION_WINDOW_COMMITS * 8;
const ENTRY_COUNT_OFFSET = ENTRY_START_OFFSET + PS_INVALIDATION_WINDOW_COMMITS * 4;
const PIN_TXID_OFFSET = ENTRY_COUNT_OFFSET + PS_INVALIDATION_WINDOW_COMMITS * 4;
const PIN_COUNT_OFFSET = PIN_TXID_OFFSET + PIN_SLOTS * 8;
const ARENA_OFFSET = PIN_COUNT_OFFSET + PIN_SLOTS * 4;
const META_OFFSET = ARENA_OFFSET + WINDOW_ARENA * 4;
const RESOLUTION_OFFSET = META_OFFSET + META_MAX;
const KEY_OFFSET = RESOLUTION_OFFSET + PS_RESOLUTION_WINDOW_COMMITS * 16;
const KEY_LEN = 32;
const PENDING_DIGEST_OFFSET = KEY_OFFSET + KEY_LEN;
const PENDING_INDEX_OFFSET = PENDING_DIGEST_OFFSET + PENDING_SLOTS * 8;
const TOTAL_BYTES = PENDING_INDEX_OFFSET + PENDING_SLOTS * 4;

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
  // resolution is the ring of (txid, digest) pairs: slot i at [2i, 2i + 1].
  private readonly resolution: BigInt64Array;
  // The pending set's entries, in the batch's order of addition: digests (as signed 64-bit) and indices.
  private readonly pendingDigest: BigInt64Array;
  private readonly pendingIndex: Uint32Array;

  // constructor makes fresh state, or (given buffer) another view of existing state.
  constructor(shared?: SharedArrayBuffer) {
    const buffer = shared ?? new SharedArrayBuffer(TOTAL_BYTES);
    this.buffer = buffer;
    this.header = new Int32Array(buffer, 0, HEADER_I32);
    this.big = new BigInt64Array(buffer, BIG_OFFSET, BIG_SLOTS);
    this.entryTxid = new BigInt64Array(buffer, ENTRY_TXID_OFFSET, PS_INVALIDATION_WINDOW_COMMITS);
    this.entryStart = new Int32Array(buffer, ENTRY_START_OFFSET, PS_INVALIDATION_WINDOW_COMMITS);
    this.entryCount = new Int32Array(buffer, ENTRY_COUNT_OFFSET, PS_INVALIDATION_WINDOW_COMMITS);
    this.pinTxid = new BigInt64Array(buffer, PIN_TXID_OFFSET, PIN_SLOTS);
    this.pinCount = new Int32Array(buffer, PIN_COUNT_OFFSET, PIN_SLOTS);
    this.arena = new Uint32Array(buffer, ARENA_OFFSET, WINDOW_ARENA);
    this.metaBytes = new Uint8Array(buffer, META_OFFSET, META_MAX);
    this.resolution = new BigInt64Array(
      buffer,
      RESOLUTION_OFFSET,
      PS_RESOLUTION_WINDOW_COMMITS * 2,
    );
    this.pendingDigest = new BigInt64Array(buffer, PENDING_DIGEST_OFFSET, PENDING_SLOTS);
    this.pendingIndex = new Uint32Array(buffer, PENDING_INDEX_OFFSET, PENDING_SLOTS);
    if (shared === undefined) this.big[ENGINE_OLDEST] = -1n;
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

  // epoch changes whenever the worker publishes a client commit or hands over the gate; read without
  // the lock (the engine thread's one-load check at each transaction begin).
  epoch(): number {
    return Atomics.load(this.header, EPOCH);
  }

  bumpEpoch(): void {
    Atomics.add(this.header, EPOCH, 1);
  }

  // --- the writer gate, lock-free ---

  gate(): number {
    return Atomics.load(this.header, GATE);
  }

  // tryGate takes a free gate for holder (GATE_ENGINE or GATE_WORKER).
  tryGate(holder: number): boolean {
    return Atomics.compareExchange(this.header, GATE, GATE_FREE, holder) === GATE_FREE;
  }

  // takeHanded takes a gate the worker handed to the engine thread.
  takeHanded(): boolean {
    return Atomics.compareExchange(this.header, GATE, GATE_HANDED, GATE_ENGINE) === GATE_HANDED;
  }

  // handGate passes the worker's gate to the engine thread (a maintenance commit, README §6.4).
  handGate(): void {
    Atomics.store(this.header, GATE, GATE_HANDED);
    this.bumpEpoch();
    Atomics.notify(this.header, GATE);
  }

  releaseGate(): void {
    Atomics.store(this.header, GATE, GATE_FREE);
    Atomics.notify(this.header, GATE);
  }

  // waitGate blocks the calling thread while the gate is `seen`, at most timeoutMs (engine thread).
  waitGate(seen: number, timeoutMs: number): void {
    Atomics.wait(this.header, GATE, seen, timeoutMs);
  }

  // waitGateAsync resolves once the gate may no longer be `seen`, or after timeoutMs (worker).
  waitGateAsync(seen: number, timeoutMs: number): Promise<unknown> {
    const result = Atomics.waitAsync(this.header, GATE, seen, timeoutMs);
    return result.async ? result.value : Promise.resolve(result.value);
  }

  // poisoned is set when a client commit's write or sync failed: the owner stops serving and its own
  // handle refuses commits until reopened (README §3.5).
  poisoned(): boolean {
    return Atomics.load(this.header, POISONED) !== 0;
  }

  poison(): void {
    Atomics.store(this.header, POISONED, 1);
  }

  // armAckHook arms a commit-acknowledgement test hook (ACK_HANG_UP or ACK_CRASH); takeAckHook
  // consumes it (worker).
  armAckHook(hook: number): void {
    Atomics.store(this.header, ACK_HOOK, hook);
  }

  takeAckHook(): number {
    return Atomics.exchange(this.header, ACK_HOOK, ACK_NORMAL);
  }

  // --- batched durability (validated-cow.md §4): the pager's BatchMirror, plus the worker's side ---

  // The batch lock is separate from the state lock, so a mirror update never nests inside it (the
  // engine thread publishes its batch while holding the state lock at a compaction).
  private lockBatch(): void {
    while (Atomics.compareExchange(this.header, BATCH_LOCK, 0, 1) !== 0) {
      Atomics.wait(this.header, BATCH_LOCK, 1);
    }
  }

  private unlockBatch(): void {
    Atomics.store(this.header, BATCH_LOCK, 0);
    Atomics.notify(this.header, BATCH_LOCK, 1);
  }

  // bumpBatch records an update of the batch; the caller holds the batch lock.
  private bumpBatch(): number {
    return Atomics.add(this.header, BATCH_GENERATION, 1) + 1;
  }

  // setEntries replaces the pending set; the caller holds the batch lock.
  private setEntries(entries: ManifestEntry[]): void {
    if (entries.length > PENDING_SLOTS) {
      Atomics.store(this.header, PENDING_COUNT, PENDING_OVERFLOW);
      return;
    }
    for (let i = 0; i < entries.length; i++) {
      this.pendingIndex[i] = entries[i]!.index;
      this.pendingDigest[i] = BigInt.asIntN(64, entries[i]!.checksum);
    }
    Atomics.store(this.header, PENDING_COUNT, entries.length);
  }

  // entries is the pending set in page order, or null when it outgrew the region; the caller holds the
  // batch lock.
  private entries(): ManifestEntry[] | null {
    const count = Atomics.load(this.header, PENDING_COUNT);
    if (count === PENDING_OVERFLOW) return null;
    const out: ManifestEntry[] = [];
    for (let i = 0; i < count; i++) {
      out.push({
        index: this.pendingIndex[i]!,
        checksum: BigInt.asUintN(64, this.pendingDigest[i]!),
      });
    }
    return out.sort((a, b) => a.index - b.index);
  }

  floorSlot(): number {
    return Atomics.load(this.header, FLOOR_SLOT);
  }

  floorTxid(): bigint {
    return Atomics.load(this.big, FLOOR_TXID);
  }

  // batchPending reports whether anything is pending a flush.
  batchPending(): boolean {
    return Atomics.load(this.header, PENDING_COUNT) !== 0;
  }

  // pendingCount is the pending set's size; an overflowed set counts as unbounded.
  pendingCount(): number {
    const count = Atomics.load(this.header, PENDING_COUNT);
    return count === PENDING_OVERFLOW ? Number.MAX_SAFE_INTEGER : count;
  }

  // pendingEntries is the pending set in page order, or null when it outgrew the region (the worker
  // then flushes rather than name it).
  pendingEntries(): ManifestEntry[] | null {
    this.lockBatch();
    try {
      return this.entries();
    } finally {
      this.unlockBatch();
    }
  }

  batchGeneration(): number {
    return Atomics.load(this.header, BATCH_GENERATION);
  }

  // batchState reads the whole batch at once (the pager's reconciliation).
  batchState(): BatchState {
    this.lockBatch();
    try {
      return {
        generation: Atomics.load(this.header, BATCH_GENERATION),
        floorSlot: Atomics.load(this.header, FLOOR_SLOT),
        floorTxid: Atomics.load(this.big, FLOOR_TXID),
        entries: this.entries(),
      };
    } finally {
      this.unlockBatch();
    }
  }

  // publishBatch replaces the batch with the engine thread's, returning the new generation.
  publishBatch(floorSlot: number, floorTxid: bigint, entries: ManifestEntry[]): number {
    this.lockBatch();
    try {
      Atomics.store(this.header, FLOOR_SLOT, floorSlot);
      Atomics.store(this.big, FLOOR_TXID, floorTxid);
      this.setEntries(entries);
      return this.bumpBatch();
    } finally {
      this.unlockBatch();
    }
  }

  // floorAt records a durable client commit: its meta, in slot, is the floor, and nothing is pending.
  floorAt(slot: number, txid: bigint): void {
    this.publishBatch(slot, txid, []);
  }

  // addPending records a relaxed client commit (README §3.5): its own body pages join the pending set.
  addPending(own: ManifestEntry[]): void {
    this.lockBatch();
    try {
      const count = Atomics.load(this.header, PENDING_COUNT);
      if (count === PENDING_OVERFLOW || count + own.length > PENDING_SLOTS) {
        Atomics.store(this.header, PENDING_COUNT, PENDING_OVERFLOW);
      } else {
        for (let i = 0; i < own.length; i++) {
          this.pendingIndex[count + i] = own[i]!.index;
          this.pendingDigest[count + i] = BigInt.asIntN(64, own[i]!.checksum);
        }
        Atomics.store(this.header, PENDING_COUNT, count + own.length);
      }
      this.bumpBatch();
    } finally {
      this.unlockBatch();
    }
  }

  // batchFlushed records a flush of a pending batch: the newest meta, in the non-floor slot, is the
  // floor now. newest is the flusher's newest commit (the engine thread's own may not be published
  // yet); the published one counts too (a client commit the engine thread has not adopted). Called
  // holding the writer gate, so no commit moves it meanwhile.
  batchFlushed(newest = 0n): void {
    this.lockBatch();
    try {
      const committed = Atomics.load(this.big, COMMITTED_TXID);
      Atomics.store(this.header, FLOOR_SLOT, Atomics.load(this.header, FLOOR_SLOT) ^ 1);
      Atomics.store(this.big, FLOOR_TXID, newest > committed ? newest : committed);
      Atomics.store(this.header, PENDING_COUNT, 0);
      this.bumpBatch();
    } finally {
      this.unlockBatch();
    }
  }

  // --- handoff (README §6.5) ---

  // ownerState is the owner's handoff state (OWNER_*); read without the lock, changed with it.
  ownerState(): number {
    return Atomics.load(this.header, OWNER_STATE);
  }

  setOwnerStateLocked(state: number): void {
    Atomics.store(this.header, OWNER_STATE, state);
    Atomics.notify(this.header, OWNER_STATE);
  }

  // requesterLocked is the connection id a yield goes to (0: none).
  requesterLocked(): bigint {
    return this.big[REQUESTER]!;
  }

  setRequesterLocked(connectionId: bigint): void {
    this.big[REQUESTER] = connectionId;
  }

  // --- the rest is called with the lock held ---

  committedTxidLocked(): bigint {
    return this.big[COMMITTED_TXID]!;
  }

  freeGenTxidLocked(): bigint {
    return this.big[FREE_GEN_TXID]!;
  }

  liveAtCompactionLocked(): number {
    return Number(this.big[LIVE_AT_COMPACTION]!);
  }

  // engineOldestLocked is the oldest version the engine thread's local readers pinned at its last
  // publication, or null.
  engineOldestLocked(): bigint | null {
    const v = this.big[ENGINE_OLDEST]!;
    return v < 0n ? null : v;
  }

  // setEngineFactsLocked records the engine thread's page accounting facts with a publication: the free
  // list's generation, the live count at the last reclamation, and its oldest local pin.
  setEngineFactsLocked(freeGenTxid: bigint, liveAtCompaction: number, oldest: bigint | null): void {
    this.big[FREE_GEN_TXID] = freeGenTxid;
    this.big[LIVE_AT_COMPACTION] = BigInt(liveAtCompaction);
    this.big[ENGINE_OLDEST] = oldest ?? -1n;
  }

  clearEngineOldestLocked(): void {
    this.big[ENGINE_OLDEST] = -1n;
  }

  // setKeyLocked records the database's data key (null: plaintext) for the worker
  // (spec/fileformat/encrypted.md §14).
  setKeyLocked(dek: Uint8Array | null): void {
    const key = new Uint8Array(this.buffer, KEY_OFFSET, KEY_LEN);
    if (dek === null) {
      key.fill(0);
      this.header[KEY_STATE] = 0;
    } else {
      key.set(dek);
      this.header[KEY_STATE] = 1;
    }
  }

  // keyLocked copies the database's data key, or null for a plaintext database.
  keyLocked(): Uint8Array | null {
    if (this.header[KEY_STATE] === 0) return null;
    return new Uint8Array(this.buffer, KEY_OFFSET, KEY_LEN).slice();
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
    this.big[WINDOW_START] = txid;
    this.header[WINDOW_ENTRIES] = 0;
    this.header[ARENA_USED] = 0;
    this.recordResolutionLocked(txid, meta);
    if (compacted) Atomics.add(this.header, FILE_GENERATION, 1);
  }

  // publishLocked records a main-database commit: its txid, meta, and written indices (README §4).
  publishLocked(txid: bigint, meta: Uint8Array, written: number[]): void {
    this.setMeta(txid, meta);
    this.recordResolutionLocked(txid, meta);
    const indices = [...new Set(written)].sort((a, b) => a - b);
    if (indices.length > WINDOW_ARENA) {
      this.big[WINDOW_START] = txid; // nothing older is answerable exactly any more
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
      this.big[WINDOW_START] = this.entryTxid[drop - 1]!;
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
    if (
      !sameIncarnation ||
      lastTxid === 0n ||
      lastTxid > txid ||
      lastTxid < this.big[WINDOW_START]!
    ) {
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

  // recordResolutionLocked adds a published commit's (txid, digest) pair: the digest is the resolution
  // digest of the meta page that published it (README §3.5, resolutionDigest).
  recordResolutionLocked(txid: bigint, meta: Uint8Array): void {
    const next = this.header[RESOLUTION_NEXT]!;
    this.resolution[next * 2] = txid;
    this.resolution[next * 2 + 1] = BigInt.asIntN(64, resolutionDigest(meta));
    this.header[RESOLUTION_NEXT] = (next + 1) % PS_RESOLUTION_WINDOW_COMMITS;
    this.header[RESOLUTION_COUNT] = Math.min(
      this.header[RESOLUTION_COUNT]! + 1,
      PS_RESOLUTION_WINDOW_COMMITS,
    );
  }

  // resolveLocked answers commit_status (page-service.md §8.4): committed when the window holds txid
  // with this digest, not committed when it holds it with another or txid is above the committed one,
  // unknown otherwise.
  resolveLocked(txid: bigint, digest: bigint): number {
    if (txid > this.big[COMMITTED_TXID]!) return PS_COMMIT_STATUS_NOT_COMMITTED;
    const want = BigInt.asIntN(64, digest);
    const count = this.header[RESOLUTION_COUNT]!;
    for (let i = 0; i < count; i++) {
      if (this.resolution[i * 2] === txid) {
        return this.resolution[i * 2 + 1] === want
          ? PS_COMMIT_STATUS_COMMITTED
          : PS_COMMIT_STATUS_NOT_COMMITTED;
      }
    }
    return PS_COMMIT_STATUS_UNKNOWN;
  }

  // resolutionTxidsLocked lists the window's txids, oldest first (tests).
  resolutionTxidsLocked(): bigint[] {
    const count = this.header[RESOLUTION_COUNT]!;
    const start = count < PS_RESOLUTION_WINDOW_COMMITS ? 0 : this.header[RESOLUTION_NEXT]!;
    const out: bigint[] = [];
    for (let i = 0; i < count; i++) {
      out.push(this.resolution[((start + i) % PS_RESOLUTION_WINDOW_COMMITS) * 2]!);
    }
    return out;
  }

  private setMeta(txid: bigint, meta: Uint8Array): void {
    this.big[COMMITTED_TXID] = txid;
    this.metaBytes.set(meta);
    this.header[META_LENGTH] = meta.length;
  }
}
