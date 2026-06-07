// Bounded buffer pool — a CLOCK (second-chance) cache of decoded leaf nodes keyed by on-disk page id
// (spec/design/pager.md §3). The demand-paging read path (P6.4b) faults a leaf through getOrLoad; the
// pool bounds how many leaves are resident at once, evicting under CLOCK when full.
//
// No pins. Eviction only drops the cache entry — any node still referenced by a live tree or an
// in-flight read stays alive via GC, and a clean node is immutable so a re-load is a harmless
// duplicate (pager.md §4). A traversal holds at most a root→leaf path, a bound of tree height.
//
// Not a §8 byte contract (pager.md §3): the pool changes WHEN a page is resident, never WHAT a query
// observes (results and cost are invariant to it), so each core may implement it idiomatically — like
// P5.3's per-core concurrency.

import type { PNode } from "./pmap.ts";

// One resident page: its id, the cached node, and the CLOCK reference bit (set on access, cleared by
// the sweeping hand to grant a second chance).
type Slot = { page: number; node: PNode; referenced: boolean };

// A bounded CLOCK cache from page id to a decoded leaf node.
export class BufferPool {
  private capacity: number;
  private slots: Slot[] = [];
  private index = new Map<number, number>();
  private hand = 0;

  constructor(capacity: number) {
    this.capacity = Math.max(1, capacity);
  }

  // getOrLoad returns the decoded node for page: a cache hit returns the cached node (setting its
  // reference bit), a miss calls load (read + decode the page), caches it — evicting one page under
  // CLOCK if at capacity — and returns it.
  getOrLoad(page: number, load: () => PNode): PNode {
    const i = this.index.get(page);
    if (i !== undefined) {
      this.slots[i].referenced = true;
      return this.slots[i].node;
    }
    const node = load();
    this.insertSlot(page, node);
    return node;
  }

  // insertSlot adds a freshly-loaded page, evicting one under CLOCK if at capacity.
  private insertSlot(page: number, node: PNode): void {
    if (this.slots.length < this.capacity) {
      this.index.set(page, this.slots.length);
      this.slots.push({ page, node, referenced: false });
      return;
    }
    const victim = this.evictSlot();
    this.index.delete(this.slots[victim].page);
    this.index.set(page, victim);
    this.slots[victim] = { page, node, referenced: false };
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

  // resident is the number of pages currently resident — the bound the pool enforces (≤ capacity).
  resident(): number {
    return this.slots.length;
  }
}
