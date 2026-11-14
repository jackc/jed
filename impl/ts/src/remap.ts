// PROTOTYPE: deferred interior pages (spec/design/deferred-interior.md §9), behind
// JED_DEFER_INTERIOR=on and without a version bump, mirroring the Go core's remap.go and the Rust
// core's remap.rs function for function so all three write byte-identical files.
//
// A commit writes only the nodes whose own content changed. An interior whose only change is which
// page a child lives at keeps its page, and the meta records "in interior page P, the child pointer in
// slot i now names page C" in a remap list. Readers apply it as they read an interior's child pointers.
// When the list would overflow the commit folds: every stale interior and its ancestors are written
// fresh and the list empties. An entry dies in the commit that orphans its parent, so no page is held
// off the free list (§9.5).
import { NO_TAG } from "./cap.ts";
import { engineError } from "./errors.ts";
import { type Child, newFreshInterior, originOf, type PNode, residentRef } from "./pmap.ts";
import type { Snapshot } from "./snapshot.ts";
import { deferInterior } from "./v53.ts";

export type PageRef = { page: number; tag: Uint8Array };

// entrySize is one entry's bytes (§9.2): parent u32, slot u16, page u32, and an encrypted file's tag.
export const entrySize = (encrypted: boolean): number => (encrypted ? 10 + 16 : 10);

// area is the meta's manifest area [lo, hi): after the header (and an encrypted meta's tags), before
// an encrypted meta's reserve.
export const area = (ps: number, encrypted: boolean): [number, number] =>
  encrypted ? [96, ps - 40] : [64, ps];

// capacity is the entries a meta carries: one third of the manifest area at every page size (§9.2).
export function capacity(ps: number, encrypted: boolean): number {
  const [lo, hi] = area(ps, encrypted);
  return Math.floor(Math.floor((hi - lo) / 3) / entrySize(encrypted));
}

// deferOn reports whether a file of page size ps uses the deferred-interior layout. The layout is
// fixed by the format, not the meta: there is no marker, and a slot with no entries is a zero region.
export function deferOn(ps: number, encrypted: boolean): boolean {
  return deferInterior() && capacity(ps, encrypted) > 0;
}

// reserve is the bytes the region takes from the end of the manifest area: 0 without the layout.
export function reserve(ps: number, encrypted: boolean): number {
  return deferOn(ps, encrypted) ? entrySize(encrypted) * capacity(ps, encrypted) : 0;
}

const key = (parent: number, slot: number): number => parent * 65536 + slot;

// RemapState is the remap list, keyed by (parent page, slot).
export class RemapState {
  private readonly entries = new Map<number, PageRef>();

  get size(): number {
    return this.entries.size;
  }

  clone(): RemapState {
    const out = new RemapState();
    for (const [k, v] of this.entries) out.entries.set(k, v);
    return out;
  }

  // set records that slot `slot` of the retained interior `parent` now resolves to cur; an existing
  // entry is overwritten, so a child that moves repeatedly under one parent occupies one entry forever.
  set(parent: number, slot: number, cur: PageRef): void {
    this.entries.set(key(parent, slot), cur);
  }

  has(parent: number, slot: number): boolean {
    return this.entries.has(key(parent, slot));
  }

  // dropParent forgets every entry `parent` keys: the commit retires that parent (§9.5).
  dropParent(parent: number): void {
    for (const k of [...this.entries.keys()]) {
      if (Math.floor(k / 65536) === parent) this.entries.delete(k);
    }
  }

  // keys reports whether interior page `parent` keys any entry -- is stale.
  keys(parent: number): boolean {
    for (const k of this.entries.keys()) if (Math.floor(k / 65536) === parent) return true;
    return false;
  }

  // parents is the pages that key an entry.
  parents(): Set<number> {
    const out = new Set<number>();
    for (const k of this.entries.keys()) out.add(Math.floor(k / 65536));
    return out;
  }

  // sorted is every entry in (parent, slot) order, the order they are stored.
  sorted(): { parent: number; slot: number; to: PageRef }[] {
    return [...this.entries.entries()]
      .sort((a, b) => a[0] - b[0])
      .map(([k, to]) => ({ parent: Math.floor(k / 65536), slot: k % 65536, to }));
  }

  // resolve redirects the n + 1 child pointers just read from interior page `parent` (§9.1: one
  // lookup per slot, never transitive). An entry naming a slot past the parent's item count is XX001.
  resolve(parent: number, children: PageRef[]): void {
    if (this.entries.size === 0) return;
    for (const [k, to] of this.entries) {
      if (Math.floor(k / 65536) !== parent) continue;
      const slot = k % 65536;
      if (slot >= children.length) {
        throw engineError(
          "data_corrupted",
          "remap entry names a slot past its parent's item count",
        );
      }
      children[slot] = to;
    }
  }

  // encode writes the list into meta page m (§9.2): remap_entries at offset 60, the entries ascending
  // from the start of the region, the rest of the region zero. Before the meta's checksum is computed
  // and before an encrypted meta is sealed (the seal covers the region).
  encode(m: Uint8Array): void {
    const encrypted = m[6] !== 0;
    const ps = m.length;
    const [, hi] = area(ps, encrypted);
    const size = entrySize(encrypted);
    let at = hi - reserve(ps, encrypted);
    m.fill(0, at, hi);
    const dv = new DataView(m.buffer, m.byteOffset, m.byteLength);
    dv.setUint32(60, this.entries.size, false);
    for (const { parent, slot, to } of this.sorted()) {
      dv.setUint32(at, parent, false);
      dv.setUint16(at + 4, slot, false);
      dv.setUint32(at + 6, to.page, false);
      if (encrypted) m.set(to.tag, at + 10);
      at += size;
    }
  }

  // decode reads and validates meta page m's list (§9.3), first and pageCount being the meta's body
  // bounds: null when the slot is structurally invalid. Without the layout offset 60 is reserved zero,
  // as through v52.
  static decode(m: Uint8Array, first: number, pageCount: number): RemapState | null {
    const dv = new DataView(m.buffer, m.byteOffset, m.byteLength);
    const count = dv.getUint32(60, false);
    const encrypted = m[6] !== 0;
    const ps = m.length;
    if (!deferOn(ps, encrypted)) return count === 0 ? new RemapState() : null;
    if (count > capacity(ps, encrypted)) return null;
    const [, hi] = area(ps, encrypted);
    const size = entrySize(encrypted);
    let at = hi - reserve(ps, encrypted);
    const out = new RemapState();
    const targets = new Set<number>();
    let prev = -1;
    for (let i = 0; i < count; i++) {
      const parent = dv.getUint32(at, false);
      const slot = dv.getUint16(at + 4, false);
      const page = dv.getUint32(at + 6, false);
      const tag = encrypted ? m.slice(at + 10, at + size) : NO_TAG;
      const k = key(parent, slot);
      if (
        k <= prev ||
        parent < first ||
        parent >= pageCount ||
        page < first ||
        page >= pageCount ||
        page === parent ||
        targets.has(page)
      )
        return null;
      prev = k;
      targets.add(page);
      out.entries.set(k, { page, tag });
      at += size;
    }
    for (let i = at; i < hi; i++) if (m[i] !== 0) return null;
    return out;
  }
}

// CatState is the committed catalog chain, kept so a commit whose catalog bytes are unchanged does not
// rewrite it (§9.1 scope).
export type CatState = { payloads: Uint8Array[]; root: PageRef };

// DeferPlan is one commit's deferral decision, taken before allocation (§9.4): the entry set minus
// every retired parent, the interiors that keep their page, and the migration page.
export type DeferPlan = {
  remap: RemapState;
  retain: Set<number>;
  migrate: number;
  capacity: number;
};

// DeferPass is one serialization pass's working copy of a plan. A TS pass may be undone and repeated
// (an overflow chain or a seal it must await), so each pass starts from the plan and records the
// interiors it retained, which an undo must return to dirty.
export type DeferPass = {
  remap: RemapState;
  retain: Set<number>;
  capacity: number;
  retained: number;
  retainedIds: number[];
};

export function beginPass(plan: DeferPlan): DeferPass {
  return {
    remap: plan.remap.clone(),
    retain: plan.retain,
    capacity: plan.capacity,
    retained: 0,
    retainedIds: [],
  };
}

// migrateHint is §9.6: the top page of the file when it is not free and some lower page is free.
export function migrateHint(free: number[], high: number): number {
  if (high < 3) return 0;
  const top = high - 1;
  let lower = false;
  for (const p of free) {
    if (p === top) return 0; // already free: the last commit's trailing run will take it
    lower = lower || p < top;
  }
  return lower ? top : 0;
}

// retainable is §9.4 step 2 for a dirty interior: it replaces a persisted page one for one (a), its
// separators are still that page's (b), and its page is not the migration page (c).
function retainable(n: PNode, migrate: number): boolean {
  const page = n.origin?.page ?? 0;
  return page !== 0 && n.sameKeys === true && page !== migrate;
}

// planNode decides retention under n without allocating, recording what serialization will: the clean
// pages it keeps, the interiors that keep their page, and the (parent, slot) of each moved child of a
// retained interior. It reports whether n gets a fresh page.
function planNode(
  n: PNode,
  migrate: number,
  kept: Set<number>,
  retain: Set<number>,
  adds: [number, number][],
): boolean {
  if (n.page !== 0) {
    kept.add(n.page);
    return false;
  }
  if (n.children.length === 0) return true; // a dirty leaf is always written fresh
  const moved: number[] = [];
  n.children.forEach((c: Child, i: number) => {
    if (c.node === null) kept.add(c.page);
    else if (planNode(c.node, migrate, kept, retain, adds)) moved.push(i);
  });
  if (!retainable(n, migrate)) return true;
  const origin = n.origin!.page;
  retain.add(origin);
  for (const i of moved) adds.push([origin, i]);
  return false;
}

// visitRetired walks the previous snapshot's subtree under n as the live-page delta's orphan walk does,
// skipping every subtree the new snapshot keeps, and records each interior it reaches that keys an
// entry: the parents this commit retires or retains.
function visitRetired(n: PNode | null, kept: Set<number>, parents: Set<number>, out: Set<number>) {
  if (n === null || n.page === 0 || kept.has(n.page) || n.children.length === 0) return;
  if (parents.has(n.page)) out.add(n.page);
  for (const c of n.children) if (c.node !== null) visitRetired(c.node, kept, parents, out);
}

// eachCommittedTree calls f with the root of every tree a commit serializes, in serialization order:
// each table's data tree, then its non-GiST index trees.
export function eachCommittedTree(snap: Snapshot, f: (root: PNode) => void): void {
  const keys = [...snap.tables.keys()].sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
  for (const k of keys) {
    const root = snap.stores.get(k)!.treeRoot();
    if (root !== null) f(root);
    for (const idx of snap.tables.get(k)!.indexes) {
      if (idx.kind === "gist") continue;
      const iroot = snap.indexStore(idx.name).treeRoot();
      if (iroot !== null) f(iroot);
    }
  }
}

// planDeferral is §9.4 before allocation: retention, the entry set and the fold test. e0 is the
// committed list and prev the committed snapshot it describes. null when the commit must fold. The drop
// rule (§9.5) comes from the walk the live-page delta makes: a committed parent it reaches that the
// commit does not retain is retired in this commit, the one that orphans its page.
export function planDeferral(
  snap: Snapshot,
  prev: Snapshot | null,
  e0: RemapState,
  migrate: number,
  cap: number,
): DeferPlan | null {
  const kept = new Set<number>();
  const retain = new Set<number>();
  const adds: [number, number][] = [];
  eachCommittedTree(snap, (root) => {
    planNode(root, migrate, kept, retain, adds);
  });
  const e = e0.clone();
  if (e.size > 0) {
    if (prev === null) return null; // without the committed snapshot the drop rule cannot run: fold
    const parents = e0.parents();
    const reached = new Set<number>();
    for (const st of prev.stores.values()) visitRetired(st.treeRoot(), kept, parents, reached);
    for (const st of prev.indexStores.values()) visitRetired(st.treeRoot(), kept, parents, reached);
    for (const p of reached) if (!retain.has(p)) e.dropParent(p);
  }
  let size = e.size;
  for (const [p, s] of adds) if (!e.has(p, s)) size++;
  if (size > cap) return null;
  return { remap: e, retain, migrate, capacity: cap };
}

// unstale is a copy of n's subtree in which every stale interior -- one whose page keys an entry in
// parents -- and every ancestor of one is a dirty node the commit writes fresh; null when nothing under
// n is stale. The fold (§9.4).
export function unstale(n: PNode | null, parents: Set<number>): PNode | null {
  if (n === null || n.children.length === 0) return null;
  let children: Child[] | null = null;
  n.children.forEach((c, i) => {
    if (c.node === null) return;
    const r = unstale(c.node, parents);
    if (r === null) return;
    children ??= n.children.slice();
    children[i] = residentRef(r);
  });
  const stale = n.page !== 0 && parents.has(n.page);
  if (children === null && !stale) return null;
  if (n.page === 0 && children === null) return null;
  return newFreshInterior(n.keys.slice(), children ?? n.children.slice());
}

// foldSnapshot is the fold over every tree of snap: each stale interior -- one whose page keys an entry
// in parents -- and each of its ancestors becomes a dirty node the commit writes fresh, so the commit can
// publish an empty list.
export function foldSnapshot(snap: Snapshot, parents: Set<number>): void {
  if (parents.size === 0) return;
  const fold = (root: PNode | null): PNode | null => unstale(root, parents);
  for (const st of snap.stores.values()) st.unstale(fold);
  for (const st of snap.indexStores.values()) st.unstale(fold);
}

// hasDirtyNode reports whether any store -- a GiST leaf-key store included, whose nodes are never
// persisted -- has a dirty tree root: the Go core's dirtyNodeCount() !== 0.
export function hasDirtyNode(snap: Snapshot): boolean {
  for (const st of [...snap.stores.values(), ...snap.indexStores.values()]) {
    const r = st.treeRoot();
    if (r !== null && r.page === 0) return true;
  }
  return false;
}

export { originOf };
