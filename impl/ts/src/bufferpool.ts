// Bounded buffer pool — a CLOCK (second-chance) cache of decoded leaf nodes keyed by on-disk page id
// (spec/design/pager.md §3). The demand-paging read path (P6.4b) faults a leaf through getOrLoad; the
// pool bounds the bytes resident leaves hold (cache_bytes), each leaf charged what this core holds for
// it (nodeBytes), evicting under CLOCK until a new leaf fits. At least one leaf stays resident whatever
// the budget.
//
// No pins. Eviction only drops the cache entry — any node still referenced by a live tree or an
// in-flight read stays alive via GC, and a clean node is immutable so a re-load is a harmless
// duplicate (pager.md §4). A traversal holds at most a root→leaf path, a bound of tree height.
//
// Not a §8 byte contract (pager.md §3): the pool changes WHEN a page is resident, never WHAT a query
// observes (results and cost are invariant to it), so each core may implement it idiomatically — like
// P5.3's per-core concurrency.

import type { PNode } from "./pmap.ts";
import { ioSteps, runSync, type Steps } from "./execution.ts";

// One resident page: its id, the cached node, the bytes it is charged, and the CLOCK reference bit (set
// on access, cleared by the sweeping hand to grant a second chance).
type Slot = { page: number; node: PNode; bytes: number; referenced: boolean };

// nodeBytes is what the pool charges for a node: a faulted leaf's held bytes (PackedLeaf.held). The
// pool caches only faulted leaves; any other node is charged a page.
export function nodeBytes(node: PNode): number {
  return node.packed?.held ?? 8192;
}

// A CLOCK cache from page id to a decoded leaf node, bounded in bytes.
export class BufferPool {
  // capacity is the byte budget the resident leaves are charged against together (carried over to the
  // replacement pool when compaction reopens the database, spec/design/api.md §2.6). Re-bounded only
  // through setCapacity. used is what they are charged now.
  capacity: number;
  private used = 0;
  private slots: Slot[] = [];
  private index = new Map<number, number>();
  private hand = 0;
  private generation = 0;

  constructor(capacity: number) {
    this.capacity = Math.max(1, capacity);
  }

  // getOrLoad returns the decoded node for page: a cache hit returns the cached node (setting its
  // reference bit), a miss calls load (read + decode the page), caches it — evicting one page under
  // CLOCK if at capacity — and returns it.
  getOrLoad(page: number, load: () => PNode): PNode {
    return runSync(this.getOrLoadSteps(page, () => ioSteps(load)));
  }

  *getOrLoadSteps(page: number, load: () => Steps<PNode>): Steps<PNode> {
    const i = this.index.get(page);
    if (i !== undefined) {
      this.slots[i].referenced = true;
      return this.slots[i].node;
    }
    const generation = this.generation;
    const node = yield* load();
    // A failed or canceled load changes no cache state. An invalidation during a suspended
    // load prevents its old decode from being installed into the newer cache generation.
    if (generation === this.generation && !this.index.has(page)) this.insertSlot(page, node);
    return node;
  }

  // contains reports whether page is resident.
  contains(page: number): boolean {
    return this.index.has(page);
  }

  // insertSlot adds a freshly-loaded page, evicting under CLOCK until it fits the budget or nothing is
  // left to evict: a leaf larger than the whole budget is still admitted, alone.
  private insertSlot(page: number, node: PNode): void {
    const bytes = nodeBytes(node);
    while (this.slots.length > 0 && this.used + bytes > this.capacity) {
      this.drop(this.evictSlot());
    }
    this.index.set(page, this.slots.length);
    this.slots.push({ page, node, bytes, referenced: false });
    this.used += bytes;
  }

  // evictSlot advances the CLOCK hand, clearing the reference bit of each page it passes (a second
  // chance), and returns the index of the first unreferenced page to evict. Terminates within two
  // sweeps (every page's bit is cleared on the first pass).
  private evictSlot(): number {
    for (;;) {
      const i = this.hand;
      this.hand = (this.hand + 1) % this.slots.length;
      if (this.slots[i].referenced) this.slots[i].referenced = false;
      else return i;
    }
  }

  // setCapacity re-bounds the budget (clamped to ≥ 1), evicting under CLOCK until the resident set
  // fits, keeping at least one leaf — a spilled temp domain's pool tightens from pinned to
  // temp_buffers (spec/design/temp-tables.md §6).
  setCapacity(capacity: number): void {
    this.capacity = Math.max(1, capacity);
    while (this.slots.length > 1 && this.used > this.capacity) {
      const victim = this.evictSlot();
      this.invalidate(this.slots[victim].page);
    }
  }

  // resident is the number of pages currently resident.
  resident(): number {
    return this.slots.length;
  }

  // residentBytes is what the resident leaves are charged together — the bound the pool enforces
  // (≤ capacity, or one leaf).
  residentBytes(): number {
    return this.used;
  }

  // invalidateAll drops every cached page — a page client told by the owner that its whole cache may be
  // stale (spec/pageservice/README.md §3.2).
  invalidateAll(): void {
    this.generation++;
    this.slots = [];
    this.used = 0;
    this.index = new Map();
    this.hand = 0;
  }

  // invalidate drops any cached entry for page — required when a commit REWRITES a page in place, which
  // happens when within-session compaction (a reclaim domain — temp, or an in-memory database with
  // reclamation on) hands a freed page id back to a new node: the pool caches by page id, so the stale
  // decode of the page's PRIOR content must be evicted or a later fault returns old rows. A no-op when the
  // page is not resident — the common case, since a copy-on-write commit without reuse only ever writes
  // fresh, never-cached high-water pages (so the main file path pays only a map lookup).
  invalidate(page: number): void {
    this.generation++;
    const i = this.index.get(page);
    if (i === undefined) return;
    this.drop(i);
  }

  // drop removes slot i: swap the last slot into the hole so the array stays dense (the CLOCK hand
  // stays well-formed), then shrink. An eviction is not an invalidation, so it leaves the generation.
  private drop(i: number): void {
    this.index.delete(this.slots[i]!.page);
    this.used -= this.slots[i]!.bytes;
    const last = this.slots.length - 1;
    if (i !== last) {
      const moved = this.slots[last]!;
      this.slots[i] = moved;
      this.index.set(moved.page, i);
    }
    this.slots.pop();
    if (this.slots.length === 0) this.hand = 0;
    else this.hand %= this.slots.length;
  }
}
