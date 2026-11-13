// Incremental free-list reclamation (spec/design/incremental-reclaim.md; format.md *Reclamation*).
// Each file commit lists the pages it orphans in its own persisted free list, so dead pages do not wait
// for a reachability rebuild.
//
// The rules (design doc §3):
//   - commit T lists its orphans in its own free list, but never reuses them itself: they are
//     reachable from T's fallback meta (T−1);
//   - from T+1 they are reusable, subject to the reader watermark: in memory each listed set keeps its
//     generation (T) until no reader pins a version older than T (IncrState.gens);
//   - under synchronous = off an orphan that was written inside the pending batch is in the floor's
//     manifest, so it is held in memory and listed only by the first commit planned with nothing
//     pending (IncrState.held); the batch reuse rule (only pages free at the floor) does the rest;
//   - a rare rebuild still runs, triggered by the exact count of unlisted dead pages (incrLeaked).
//
// Browser-clean: host-agnostic core only.

import { capFirstBody } from "./cap.ts";
import type { ManifestEntry } from "./commit_manifest.ts";
import { COMPACT_MIN_PAGES, RECLAIM_LEAK_DEN } from "./costs.ts";
import type { Engine } from "./executor.ts";
import type { FreeListPlan, IncrementalWrite } from "./format.ts";

// FreeGen is the free pages one commit listed, sorted, tagged with the version from which they are
// dead (the commit's txid).
export type FreeGen = { txid: bigint; pages: number[] };

// IncrState is a file storage's incremental-reclamation state. Nothing in it is persisted: it is
// rebuilt at open from what the load reads anyway, and its loss (a role change, a foreign commit) only
// leaks pages until the leak-triggered rebuild.
export type IncrState = {
  // known is whether catalog, gist and manifest name the committed state's catalog chain, GiST R-tree
  // pages and manifest-overflow pages. A page client's commit leaves them unknown (the owner did not
  // plan it); the next owner commit then leaks them.
  known: boolean;
  catalog: number[];
  gist: number[];
  manifest: number[];
  // manifestCount is how many manifest-overflow pages the committed meta names (known or not).
  manifestCount: number;
  // gens is the free pages listed since the base generation (Engine.freeGenTxid), oldest first.
  gens: FreeGen[];
  // held is the orphans written inside the pending relaxed batch (validated-cow.md §4), sorted.
  held: number[];
  // tolerated is the unlisted dead pages the state started with (a reopen loses the held pages and a
  // one-entry list; a page client's commit leaks its orphans). The simulator's growth oracle allows
  // them.
  tolerated: number;
  unknownBefore: boolean;
  // tree is the B-tree / overflow orphans of the commit being planned (admitFile computes them with
  // the live-page delta).
  tree: number[];
};

// IncrPlan is what planFreeList needs of an incremental commit: the orphans to list (never reused by
// this commit) and whether the leak trigger fires.
export type IncrPlan = { listed: number[]; due: boolean };

export function newIncrState(): IncrState {
  return {
    known: false,
    catalog: [],
    gist: [],
    manifest: [],
    manifestCount: 0,
    gens: [],
    held: [],
    tolerated: 0,
    unknownBefore: false,
    tree: [],
  };
}

// freshImageIncr is the state of a just-created file: every body page of a fresh image is a catalog
// page, which the first commit orphans.
export function freshImageIncr(firstBody: number, pageCount: number): IncrState {
  const inc = newIncrState();
  inc.known = true;
  for (let p = firstBody; p < pageCount; p++) inc.catalog.push(p);
  return inc;
}

// incrActive is whether db's commits reclaim incrementally: a durable file's owner (or alone process),
// on the Node file host and the browser OPFS host alike. A page client's commit is planned against its
// owner's free list and leaves orphans unlisted, like a rebuild-mode commit; in-memory and temp domains
// keep the post-commit rebuild (persist.ts maybeCompact).
export function incrActive(db: Engine): boolean {
  if (db.persistHook === null || db.paging === null || db.paging.trustsMeta()) return false;
  if (db.incr === null) db.incr = newIncrState();
  return true;
}

// incrMerge folds every generation no reader can still observe into the base generation.
export function incrMerge(db: Engine, oldest: bigint): void {
  const inc = db.incr!;
  let n = 0;
  while (n < inc.gens.length && inc.gens[n]!.txid <= oldest) {
    if (inc.gens[n]!.txid > db.freeGenTxid) db.freeGenTxid = inc.gens[n]!.txid;
    n++;
  }
  if (n > 0) inc.gens = inc.gens.slice(n);
}

// incrNewer is the listed pages a reader may still observe: those of the unmerged generations.
export function incrNewer(db: Engine): Set<number> | null {
  const inc = db.incr!;
  if (inc.gens.length === 0) return null;
  const newer = new Set<number>();
  for (const g of inc.gens) for (const p of g.pages) newer.add(p);
  return newer;
}

// excludeNewer narrows a reuse restriction (null: every free page) by the newer generations.
export function excludeNewer(
  allowed: Set<number> | null,
  newer: Set<number>,
  free: number[],
): Set<number> {
  const out = new Set<number>();
  for (const p of free) if ((allowed === null || allowed.has(p)) && !newer.has(p)) out.add(p);
  return out;
}

// reuseGeneration is the generation every listed free page is dead from, for a page-service owner's
// reuse grant (incremental-reclaim.md §3.3): a client cannot tell the generations apart, so any
// unmerged generation withholds the grant. The grant passes when every pin is at or after it.
export function reuseGeneration(db: Engine): bigint {
  const gens = db.incr?.gens ?? [];
  const newest = gens.length > 0 ? gens[gens.length - 1]!.txid : 0n;
  return newest > db.freeGenTxid ? newest : db.freeGenTxid;
}

// incrLeaked is the exact count of dead pages no list holds, in the committed state: every page below
// the high-water is a meta or key-block page, live, free, a free-list chain page, a manifest-overflow
// page of the committed meta, held, or leaked.
export function incrLeaked(db: Engine): number {
  const inc = db.incr!;
  return (
    db.pageCount -
    capFirstBody(db.paging!.cap()) -
    db.freePages.length -
    db.live.live -
    db.freeChain.length -
    inc.manifestCount -
    inc.held.length
  );
}

// incrDue is the incremental mode's rebuild trigger (incremental-reclaim.md §3.5): the unlisted dead
// pages exceed compact_min_pages and live / reclaim_leak_den.
export function incrDue(db: Engine): boolean {
  const leaked = incrLeaked(db);
  return leaked > COMPACT_MIN_PAGES && leaked * RECLAIM_LEAK_DEN > db.live.live;
}

// incrOrphans is what the commit being planned orphans, split into the pages its free list may list now
// and those written inside the pending batch, which are held (pending is the batch's pending set). A
// commit planned with nothing pending also lists the held pages. Every TS commit rewrites the catalog
// chain (there is no kept-chain prototype), so a known catalog chain is always orphaned.
export function incrOrphans(
  db: Engine,
  pending: ManifestEntry[],
): { listable: number[]; held: number[] } {
  const inc = db.incr!;
  const all = inc.tree.slice();
  if (inc.known) {
    all.push(...inc.catalog, ...inc.gist, ...inc.manifest);
  }
  all.push(...db.freeChain);
  if (pending.length === 0) all.push(...inc.held);
  all.sort((a, b) => a - b);
  for (let i = 1; i < all.length; i++) {
    if (all[i] === all[i - 1]) {
      throw new Error(`incremental reclaim: page ${all[i]} orphaned twice`);
    }
  }
  if (pending.length === 0) return { listable: all, held: [] };
  const inBatch = new Set<number>();
  for (const e of pending) inBatch.add(e.index);
  const listable: number[] = [];
  const held: number[] = [];
  for (const p of all) {
    if (inBatch.has(p)) held.push(p);
    else listable.push(p);
  }
  return { listable, held };
}

// incrAfter records a published incremental commit. Called before db's page accounting takes the
// plan's (db.pageCount is still the previous high-water).
export function incrAfter(
  db: Engine,
  txid: bigint,
  write: IncrementalWrite,
  plan: FreeListPlan,
  listed: number[],
  held: number[],
  pending: ManifestEntry[],
): void {
  const inc = db.incr!;
  inc.unknownBefore = !inc.known;
  if (plan.rebuilt) {
    inc.tolerated = 0;
    // The rebuild listed every dead page except the batch's: only those stay held.
    inc.gens = [];
    const inBatch = new Set<number>();
    for (const e of pending) inBatch.add(e.index);
    inc.held = mergeSorted(inc.held, held).filter((p) => inBatch.has(p));
  } else {
    if (listed.length > 0) inc.gens.push({ txid, pages: listed });
    inc.held = pending.length === 0 ? [] : mergeSorted(inc.held, held);
  }
  if (plan.newPageCount < db.pageCount) inc.gens = trimGens(inc.gens, plan.newPageCount);
  inc.catalog = write.catalogIds;
  inc.gist = write.gistIds;
  inc.manifest = plan.manifestIds.slice();
  inc.manifestCount = inc.manifest.length;
  inc.known = true;
  inc.tree = [];
}

// incrForeign records a commit this storage did not plan (a page client's, adopted by its owner): the
// listed pages that survive keep their generations; the wholesale pages of the new state are unknown.
export function incrForeign(db: Engine, free: number[], manifestPages: number): void {
  const inc = db.incr;
  if (inc === null) return;
  const listed = new Set(free);
  const gens: FreeGen[] = [];
  for (const g of inc.gens) {
    const keep = g.pages.filter((p) => listed.has(p));
    if (keep.length > 0) gens.push({ txid: g.txid, pages: keep });
  }
  inc.gens = gens;
  inc.known = false;
  inc.catalog = [];
  inc.gist = [];
  inc.manifest = [];
  inc.manifestCount = manifestPages;
  inc.tree = [];
}

function trimGens(gens: FreeGen[], pageCount: number): FreeGen[] {
  const out: FreeGen[] = [];
  for (const g of gens) {
    const keep = g.pages.filter((p) => p < pageCount);
    if (keep.length > 0) out.push({ txid: g.txid, pages: keep });
  }
  return out;
}

// mergeSorted merges two ascending lists (no duplicates across them).
export function mergeSorted(a: number[], b: number[]): number[] {
  if (b.length === 0) return a;
  if (a.length === 0) return b;
  const out: number[] = [];
  let i = 0;
  let j = 0;
  while (i < a.length && j < b.length) {
    if (a[i]! < b[j]!) out.push(a[i++]!);
    else if (a[i]! > b[j]!) out.push(b[j++]!);
    else throw new Error(`incremental reclaim: page ${a[i]} listed twice`);
  }
  while (i < a.length) out.push(a[i++]!);
  while (j < b.length) out.push(b[j++]!);
  return out;
}
