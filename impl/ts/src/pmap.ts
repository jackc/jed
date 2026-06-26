// Persistent (copy-on-write) ordered map — the page-backed B-tree (decision B1,
// spec/design/transactions.md §3; spec/fileformat/format.md "The per-table data B-tree").
//
// Keyed by the encoded key bytes (compared lexicographically = memcmp = the order-preserving
// key encoding's contract, spec/design/encoding.md). Every mutation returns a new map that shares
// structure with the old one; nodes are immutable by convention, so `clone()` (which shares the
// root) is an O(1) independent snapshot. That cheap, structurally-shared snapshot carries the §3
// staging-buffer / transaction model (transactions.md §2).
//
// Since Phase 6 (P6.1) this IS the on-disk B-tree, node-for-page: its fan-out is size-driven — a
// node holds as many entries as fit a page payload cap (= page_size − 16) and splits when it would
// overflow, so the node boundaries (and serialized bytes) are a §8 byte contract (format.md). The
// caller supplies each entry's on-disk weight (record size); cap is passed per call (held by the
// TableStore). Each node also carries a set-once on-disk page id (0 = dirty) for the incremental
// commit (P6.1 part B). Delete rebalances by merge-then-maybe-split (no borrow — format.md "Delete").

import type { Row } from "./storage.ts";

// One B-tree node. `children` is empty for a leaf; otherwise children.length === keys.length+1.
// keys.length === vals.length === weights.length always. weights[i] is entry i's on-disk record
// size, for the size-driven split/merge. page is the on-disk page index (0 when dirty), set once at
// the commit that first persists this node. Exported so the serializer (format.ts) can read/build it.
export type PNode = {
  keys: Uint8Array[];
  vals: Row[];
  weights: number[];
  children: Child[];
  page: number;
};

// A B-tree node's reference to one child. Under demand paging (P6.4b, spec/design/pager.md §4) a
// clean leaf need not be resident: an interior node keeps an OnDisk page id for such a child and the
// read path faults it through the buffer pool on access. node !== null ⇒ resident (a dirty node, a
// resident interior skeleton node, or a materialized leaf); node === null ⇒ OnDisk(page) — always a
// leaf, since only leaves page, which is what lets nodeCount avoid loading them. An in-memory database
// constructs only resident refs.
export type Child = { node: PNode | null; page: number };

export function residentRef(n: PNode): Child {
  return { node: n, page: 0 };
}
export function onDiskRef(page: number): Child {
  return { node: null, page };
}

// LeafSource faults a clean leaf page to a resident node on demand (pager.md §4) — the buffer pool,
// behind the table's column types. Defined here so the B-tree traversal can fault without importing
// the storage/format layers (they implement it); a fully-resident in-memory database passes null and
// never faults.
export interface LeafSource {
  loadLeaf(page: number): PNode;
}

// resolveChild resolves c to a resident node, faulting an OnDisk leaf through src. A resident ref
// returns its node directly; an OnDisk leaf with no source is an internal wiring bug (an in-memory
// tree builds no OnDisk ref, and every file-backed traversal supplies a source), so it throws.
function resolveChild(c: Child, src: LeafSource | null): PNode {
  if (c.node !== null) return c.node;
  if (src === null)
    throw new Error(`demand-paged leaf ${c.page} reached with no buffer-pool source`);
  return src.loadLeaf(c.page);
}

function isLeaf(n: PNode): boolean {
  return n.children.length === 0;
}

// payload is this node's serialized size (format.md): Σ weights plus, for an interior node,
// 4·(N+1) for its child pointers.
function payload(n: PNode): number {
  let total = 0;
  for (const w of n.weights) total += w;
  if (!isLeaf(n)) total += 4 * n.children.length;
  return total;
}

export function compareBytes(a: Uint8Array, b: Uint8Array): number {
  const n = a.length < b.length ? a.length : b.length;
  for (let i = 0; i < n; i++) {
    if (a[i] !== b[i]) return a[i] < b[i] ? -1 : 1;
  }
  return a.length === b.length ? 0 : a.length < b.length ? -1 : 1;
}

// KeyBound is a contiguous range of encoded keys — the form a primary-key predicate pushes down to a
// bounded B-tree scan (spec/design/cost.md §3 "bounded scan / point lookup", encoding.md). lo/hi are
// encoded key bytes; null is open on that side (−∞ / +∞), and the *Inc flags say whether the endpoint
// key itself is included. Because the key encoding is order-preserving (compareBytes = value order), a
// byte range is a value range. A bounded scan visits exactly the nodes whose key span intersects this
// bound, so its page_read cost is proportional to what it touches, not the whole tree — and the
// unbounded bound (−∞..+∞) degenerates to the full scan, so existing full-scan costs do not move.
export type KeyBound = {
  lo: Uint8Array | null;
  loInc: boolean;
  hi: Uint8Array | null;
  hiInc: boolean;
};

// unboundedBound is the full-table bound (−∞..+∞): every node overlaps it, reproducing the full scan.
export function unboundedBound(): KeyBound {
  return { lo: null, loInc: false, hi: null, hiInc: false };
}

// lowerBoundGT / lowerBoundGE: the first index whose key is > / ≥ `key` — the binary-search
// backbone of the window helpers below. Written as two direct loops (no predicate closure): the
// windows run per node on every descent, and a per-call closure allocation is measurable there.
function lowerBoundGT(keys: Uint8Array[], key: Uint8Array): number {
  let lo = 0;
  let hi = keys.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (compareBytes(keys[mid]!, key) <= 0) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

function lowerBoundGE(keys: Uint8Array[], key: Uint8Array): number {
  let lo = 0;
  let hi = keys.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (compareBytes(keys[mid]!, key) < 0) lo = mid + 1;
    else hi = mid;
  }
  return lo;
}

// childWindow: the contiguous window [first, last] of n's child indices whose separator span can
// overlap the bound — child i spans the OPEN interval (keys[i-1], keys[i]), so it is pruned iff
// keys[i] ≤ lo (entirely at/below lo) or keys[i-1] ≥ hi (entirely at/above hi). The keys are sorted,
// so the surviving children are contiguous and both edges binary-search: first = the first child not
// below lo, last = the last child not above hi. The strict comparisons are exact regardless of
// endpoint inclusivity — the separators are entries in this node (covered by entryWindow), never in a
// child. The node's own outer brackets need no test: the parent descended here only because this
// subtree overlaps. rangeEntries (descends) and overlapNodeCount (counts) window identically, so they
// visit the SAME node set — the §8 determinism page_read depends on — decided from resident
// separators WITHOUT faulting an OnDisk leaf. A bound admitting only a separator entry in this node
// yields first > last (every child pruned): an empty child window, still a valid entry window.
function childWindow(b: KeyBound, n: PNode): [number, number] {
  const first = b.lo === null ? 0 : lowerBoundGT(n.keys, b.lo);
  const last = b.hi === null ? n.keys.length : lowerBoundGE(n.keys, b.hi);
  return [first, last];
}

// entryWindow: the contiguous half-open window [first, last) of n's own entry indices whose keys lie
// within the bound — the binary-searched equivalent of testing containment per key, honoring the
// endpoint inclusivity flags. On a leaf this is the admitted row range; on an interior node it is the
// admitted separator entries (a B-tree stores records in interior nodes too).
function entryWindow(b: KeyBound, n: PNode): [number, number] {
  const first =
    b.lo === null ? 0 : b.loInc ? lowerBoundGE(n.keys, b.lo) : lowerBoundGT(n.keys, b.lo);
  let last =
    b.hi === null
      ? n.keys.length
      : b.hiInc
        ? lowerBoundGT(n.keys, b.hi)
        : lowerBoundGE(n.keys, b.hi);
  if (last < first) last = first;
  return [first, last];
}

// search returns the index and whether key is present: found ⇒ keys[index] === key, else index is
// the child/insertion slot.
function search(n: PNode, key: Uint8Array): { index: number; found: boolean } {
  let lo = 0;
  let hi = n.keys.length;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    const c = compareBytes(n.keys[mid], key);
    if (c === 0) return { index: mid, found: true };
    if (c < 0) lo = mid + 1;
    else hi = mid;
  }
  return { index: lo, found: false };
}

// PMap is a persistent ordered map from encoded key to Row. `clone()` is an O(1) independent
// snapshot (the root is shared; nodes are immutable).
export class PMap {
  private root: PNode | null;
  private length: number;

  constructor(root: PNode | null = null, length = 0) {
    this.root = root;
    this.length = length;
  }

  clone(): PMap {
    return new PMap(this.root, this.length);
  }

  get size(): number {
    return this.length;
  }

  // rootNode exposes the root node to the serializer (format.ts). null for an empty map.
  rootNode(): PNode | null {
    return this.root;
  }

  // get looks up the row at key. src faults an OnDisk leaf on the descent (null for a fully-resident
  // in-memory tree); an I/O error propagates as a thrown EngineError.
  get(key: Uint8Array, src: LeafSource | null): Row | undefined {
    let n = this.root;
    while (n !== null) {
      const { index, found } = search(n, key);
      if (found) return n.vals[index];
      if (isLeaf(n)) return undefined;
      n = resolveChild(n.children[index], src);
    }
    return undefined;
  }

  // insert inserts or overwrites key with val (on-disk record size weight); cap is the page payload
  // capacity. Returns the previous row if key was present (an overwrite, size unchanged), else
  // undefined (a new insert, which grows the size). An overwrite can change the weight, so it too
  // may overflow and split.
  insert(
    key: Uint8Array,
    val: Row,
    weight: number,
    cap: number,
    src: LeafSource | null,
  ): Row | undefined {
    if (this.root === null) {
      this.root = { keys: [key], vals: [val], weights: [weight], children: [], page: 0 };
      this.length++;
      return undefined;
    }
    const ctx: InsCtx = { old: undefined, replaced: false };
    const out = nodeInsert(this.root, key, val, weight, ctx, src, cap);
    this.root =
      out.whole !== null
        ? out.whole
        : {
            keys: [out.midK],
            vals: [out.midV],
            weights: [out.midW],
            children: [residentRef(out.left), residentRef(out.right)],
            page: 0,
          };
    if (!ctx.replaced) this.length++;
    return ctx.old;
  }

  // remove deletes key. Returns the removed row, or undefined if absent (then the map is unchanged).
  // cap is the page payload capacity (the rebalance threshold). src faults OnDisk leaves the delete
  // descends into / rebalances against (spec/design/pager.md §4).
  remove(key: Uint8Array, cap: number, src: LeafSource | null): Row | undefined {
    if (this.root === null) return undefined;
    const res = nodeRemove(this.root, key, src, cap);
    if (!res.ok) return undefined;
    const newRoot = res.node;
    // The root may have drained to zero keys: an empty leaf becomes the empty map; an empty internal
    // node (one child) hands the root down a level (height shrinks). The root is exempt from the
    // underfull rule, so no rebalance here.
    if (newRoot.keys.length === 0) {
      // The lone surviving child becomes the new root — fault it if it is an OnDisk leaf (a tree of
      // height 2 can collapse to its single bottom child).
      this.root = isLeaf(newRoot) ? null : resolveChild(newRoot.children[0], src);
    } else {
      this.root = newRoot;
    }
    this.length--;
    return res.removed;
  }

  // inorder returns all (key, row) pairs in ascending key order. Eager (the cost contract charges
  // per row in the executor loop, not here — spec/design/cost.md), so laziness is unobservable.
  // inorder returns all (key, row) pairs in ascending key order. Eager; src faults each OnDisk leaf
  // through the pool, and the faulted node is dropped (GC) once its rows are collected, so the
  // resident leaf set stays bounded by the pool, not the tree (pager.md §4).
  inorder(src: LeafSource | null): { keys: Uint8Array[]; vals: Row[] } {
    const keys: Uint8Array[] = [];
    const vals: Row[] = [];
    const walk = (n: PNode | null): void => {
      if (n === null) return;
      if (isLeaf(n)) {
        for (let i = 0; i < n.keys.length; i++) {
          keys.push(n.keys[i]);
          vals.push(n.vals[i]);
        }
        return;
      }
      for (let i = 0; i < n.keys.length; i++) {
        walk(resolveChild(n.children[i], src));
        keys.push(n.keys[i]);
        vals.push(n.vals[i]);
      }
      walk(resolveChild(n.children[n.keys.length], src));
    };
    walk(this.root);
    return { keys, vals };
  }

  // nodeCount is the number of B-tree nodes (pages) in this tree — the page_read count a full
  // scan charges (spec/design/cost.md §3 "page_read"). A scan walks every node, so this is the
  // structural node count (interior + leaf); 0 for an empty map. Deterministic and byte-identical
  // across cores (the node boundaries are a §8 byte contract — format.md).
  nodeCount(): number {
    const count = (n: PNode | null): number => {
      if (n === null) return 0;
      let total = 1;
      // A resident child is counted recursively; an OnDisk child is a clean leaf (only leaves page —
      // pager.md §1/§4), counted as one node WITHOUT loading it — the resident-interior-skeleton
      // dividend that keeps cost identical to P6.3 (pager.md §5).
      for (const c of n.children) total += c.node !== null ? count(c.node) : 1;
      return total;
    };
    return count(this.root);
  }

  // residentRecordBytes is the total on-disk record bytes stored in this tree — the sum of every
  // entry's weight over every node (this is a B-tree: records live in interior nodes too, not only
  // leaves). The deterministic, cross-core-identical measure of a temp table's storage footprint
  // (spec/design/temp-tables.md §7; weight is the on-disk record_size, byte-identical across cores —
  // §8). The tree is fully resident for a temp store (temp data never pages), so this never faults; an
  // OnDisk child contributes 0 (defensive — temp stores have none).
  residentRecordBytes(): number {
    const walk = (n: PNode | null): number => {
      if (n === null) return 0;
      let here = 0;
      for (const w of n.weights) here += w;
      for (const c of n.children) if (c.node !== null) here += walk(c.node);
      return here;
    };
    return walk(this.root);
  }

  // rangeEntries returns the (key, row) pairs whose key lies within the bound, in ascending key order
  // — a bounded in-order traversal that binary-searches each node's child window (the children whose
  // separator span can overlap the bound — childWindow) and in-bound entry window (entryWindow), then
  // walks only those, so only overlapping leaves fault through src. The unbounded bound walks the
  // whole tree (identical to inorder). One asymmetric edge: a separator entry equal to an INCLUSIVE lo
  // is in bound while both its adjacent children are pruned, so the entry window can start one slot
  // before the child window — emitted before the descent loop.
  rangeEntries(b: KeyBound, src: LeafSource | null): { keys: Uint8Array[]; vals: Row[] } {
    const { keys, vals } = this.rangeEntriesCounted(b, src);
    return { keys, vals };
  }

  // rangeEntriesCounted is rangeEntries plus the number of B-tree nodes the bounded traversal
  // visits — the page_read count overlapNodeCount would return, observed during the ONE windowed
  // walk instead of a second counting descent (the visited sets are identical by construction:
  // both window with childWindow).
  rangeEntriesCounted(
    b: KeyBound,
    src: LeafSource | null,
  ): { keys: Uint8Array[]; vals: Row[]; nodes: number } {
    const keys: Uint8Array[] = [];
    const vals: Row[] = [];
    let nodes = 0;
    const walk = (n: PNode): void => {
      nodes++;
      const [ef, el] = entryWindow(b, n);
      if (isLeaf(n)) {
        for (let i = ef; i < el; i++) {
          keys.push(n.keys[i]);
          vals.push(n.vals[i]);
        }
        return;
      }
      const [cf, cl] = childWindow(b, n);
      if (ef < cf) {
        keys.push(n.keys[ef]);
        vals.push(n.vals[ef]);
      }
      for (let i = cf; i <= cl; i++) {
        walk(resolveChild(n.children[i], src));
        if (i >= ef && i < el) {
          keys.push(n.keys[i]);
          vals.push(n.vals[i]);
        }
      }
    };
    if (this.root !== null) walk(this.root);
    return { keys, vals, nodes };
  }

  // overlapNodeCount is the number of B-tree nodes a bounded scan over b visits — the page_read it
  // charges (cost.md §3). Mirrors rangeEntries' traversal exactly (same childWindow prune, root
  // always visited), counting an OnDisk leaf as one node WITHOUT faulting it (pager.md §5). The
  // unbounded bound returns nodeCount(), so a full scan's cost is unchanged.
  overlapNodeCount(b: KeyBound): number {
    const count = (n: PNode): number => {
      if (isLeaf(n)) return 1;
      let total = 1;
      const [cf, cl] = childWindow(b, n);
      for (let i = cf; i <= cl; i++) {
        const ch = n.children[i];
        total += ch.node !== null ? count(ch.node) : 1;
      }
      return total;
    };
    return this.root === null ? 0 : count(this.root);
  }

  // scanRange visits the (key, row) pairs within the bound, in ascending key order, calling visit per
  // in-bound row. visit returns false to STOP the traversal — and because a leaf is faulted only when
  // descended into, leaves past the stop point are never faulted (the genuine LIMIT short-circuit —
  // spec/design/cost.md §3 "LIMIT short-circuit"). Streams one row at a time (no array), so a bounded
  // result holds ~one leaf resident. An eval error propagates as a thrown exception. Windowed like
  // rangeEntries, including the separator-equal-to-an-inclusive-lo edge emitted before the descent.
  scanRange(
    b: KeyBound,
    src: LeafSource | null,
    visit: (key: Uint8Array, row: Row) => boolean,
  ): void {
    const walk = (n: PNode): boolean => {
      const [ef, el] = entryWindow(b, n);
      if (isLeaf(n)) {
        for (let i = ef; i < el; i++) {
          if (!visit(n.keys[i], n.vals[i])) return false;
        }
        return true;
      }
      const [cf, cl] = childWindow(b, n);
      if (ef < cf && !visit(n.keys[ef], n.vals[ef])) return false;
      for (let i = cf; i <= cl; i++) {
        if (!walk(resolveChild(n.children[i], src))) return false;
        if (i >= ef && i < el && !visit(n.keys[i], n.vals[i])) return false;
      }
      return true;
    };
    if (this.root !== null) walk(this.root);
  }

  // scanRangeRev is scanRange in reverse: it visits the in-bound (key, row) pairs in DESCENDING key
  // order — the exact reverse of scanRange's row sequence — for a DESC reverse scan (cost.md §3
  // "ORDER BY satisfied by primary-key order"). It windows with the same childWindow/entryWindow
  // prune (so the visited-node set and pageRead cost match), and stops the moment visit returns
  // false without faulting leaves past the stop point (a reverse top-N faults from the high end).
  // For an interior node it walks children from cl down to cf, emitting the in-window separator
  // BEFORE descending its child, and the asymmetric inclusive-lo separator key[ef] (when ef<cf) LAST.
  scanRangeRev(
    b: KeyBound,
    src: LeafSource | null,
    visit: (key: Uint8Array, row: Row) => boolean,
  ): void {
    const walk = (n: PNode): boolean => {
      const [ef, el] = entryWindow(b, n);
      if (isLeaf(n)) {
        for (let i = el - 1; i >= ef; i--) {
          if (!visit(n.keys[i], n.vals[i])) return false;
        }
        return true;
      }
      const [cf, cl] = childWindow(b, n);
      for (let i = cl; i >= cf; i--) {
        if (i >= ef && i < el && !visit(n.keys[i], n.vals[i])) return false;
        if (!walk(resolveChild(n.children[i], src))) return false;
      }
      if (ef < cf && !visit(n.keys[ef], n.vals[ef])) return false;
      return true;
    };
    if (this.root !== null) walk(this.root);
  }
}

// pmapFromLoaded reconstructs a map from a loaded root (format.ts loadDatabase).
export function pmapFromLoaded(root: PNode | null, length: number): PMap {
  return new PMap(root, length);
}

type InsCtx = { old: Row | undefined; replaced: boolean };
// The result of inserting into a subtree: a whole rebuilt node, or a split.
type InsOut =
  | { whole: PNode }
  | { whole: null; left: PNode; midK: Uint8Array; midV: Row; midW: number; right: PNode };

// build constructs a node from the parts; if its payload overflows cap it splits 2-way and promotes
// one median. The split point m = min(largest m in [1,N-1] with leftpayload(m) ≤ cap, N-2) always
// yields two non-empty, fitting halves under RECORD_MAX = (cap-12)/2 (format.md). The < 3 guard is
// defensive against an oversized record — the oversize surfaces as 0A000 at serialize (format.ts).
// build constructs a node from the parts; if its payload overflows cap it splits 2-way and promotes
// one median (format.md "Split point"). rightEdge says the just-edited record (the inserted/replaced
// one, or the separator a child split promoted) is the node's LAST: then the split is the append
// rule m = min(m_append, N-2) with m_append = largest m in [1,N-1] with leftpayload(m) <= cap —
// sequential ascending loads pack left nodes ~full. Anywhere else (and the delete path's
// merge-overflow, which has no edited position) splits BALANCED: m = min(m_balanced, m_append, N-2)
// with m_balanced = smallest m with 2*leftpayload(m) >= payload — without it, largest-left
// degenerates to [N-2 | 1] splinters and random-order inserts converge on a few-percent fill
// (benchmarks.md finding). Either m yields two non-empty, fitting halves under the
// RECORD_MAX = (cap-12)/2 cap (format.md).
function build(
  keys: Uint8Array[],
  vals: Row[],
  weights: number[],
  children: Child[],
  cap: number,
  rightEdge: boolean,
): InsOut {
  const interior = children.length > 0;
  let total = 0;
  for (const w of weights) total += w;
  if (interior) total += 4 * children.length;
  if (total <= cap || keys.length < 3) {
    return { whole: { keys, vals, weights, children, page: 0 } };
  }

  const n = keys.length;
  let best = 1;
  let balanced = 0;
  let prefix = 0;
  for (let m = 1; m < n; m++) {
    prefix += weights[m - 1];
    const lp = (interior ? 4 * (m + 1) : 0) + prefix;
    if (lp <= cap) best = m;
    if (balanced === 0 && 2 * lp >= total) balanced = m;
  }
  if (!rightEdge && balanced !== 0 && balanced < best) best = balanced;
  let m = Math.min(best, n - 2);
  if (m < 1) m = 1;

  return {
    whole: null,
    left: {
      keys: keys.slice(0, m),
      vals: vals.slice(0, m),
      weights: weights.slice(0, m),
      children: interior ? children.slice(0, m + 1) : [],
      page: 0,
    },
    midK: keys[m],
    midV: vals[m],
    midW: weights[m],
    right: {
      keys: keys.slice(m + 1),
      vals: vals.slice(m + 1),
      weights: weights.slice(m + 1),
      children: interior ? children.slice(m + 1) : [],
      page: 0,
    },
  };
}

// nodeInsert is the recursive insert. On overwrite it sets ctx and rebuilds the path with the
// value+weight replaced (which may now overflow). On a new key it inserts into the leaf and splits
// overflowing nodes back up the path.
function nodeInsert(
  n: PNode,
  key: Uint8Array,
  val: Row,
  weight: number,
  ctx: InsCtx,
  src: LeafSource | null,
  cap: number,
): InsOut {
  const { index, found } = search(n, key);
  if (found) {
    const vals = n.vals.slice();
    const weights = n.weights.slice();
    ctx.old = vals[index];
    ctx.replaced = true;
    vals[index] = val;
    weights[index] = weight;
    return build(
      n.keys.slice(),
      vals,
      weights,
      n.children.slice(),
      cap,
      index === n.keys.length - 1,
    );
  }
  if (isLeaf(n)) {
    return build(
      insertAt(n.keys, index, key),
      insertAt(n.vals, index, val),
      insertAt(n.weights, index, weight),
      [],
      cap,
      index === n.keys.length,
    );
  }
  // Fault the target child (a resident interior, or an OnDisk leaf brought in for mutation — it
  // becomes a dirty resident node on the rebuilt path).
  const sub = nodeInsert(resolveChild(n.children[index], src), key, val, weight, ctx, src, cap);
  if (sub.whole !== null) {
    const children = n.children.slice();
    children[index] = residentRef(sub.whole);
    return {
      whole: {
        keys: n.keys.slice(),
        vals: n.vals.slice(),
        weights: n.weights.slice(),
        children,
        page: 0,
      },
    };
  }
  const keys = insertAt(n.keys, index, sub.midK);
  const vals = insertAt(n.vals, index, sub.midV);
  const weights = insertAt(n.weights, index, sub.midW);
  let children = n.children.slice();
  children[index] = residentRef(sub.left);
  children = insertAt(children, index + 1, residentRef(sub.right));
  return build(keys, vals, weights, children, cap, index === n.keys.length);
}

// maxKV is the rightmost (largest) entry of a subtree — its in-order predecessor. Faults the rightmost
// leaf through src if it is OnDisk.
function maxKV(n: PNode, src: LeafSource | null): { key: Uint8Array; val: Row; weight: number } {
  while (!isLeaf(n)) n = resolveChild(n.children[n.children.length - 1], src);
  const i = n.keys.length - 1;
  return { key: n.keys[i], val: n.vals[i], weight: n.weights[i] };
}

type RemOut = { ok: boolean; node: PNode; removed: Row | undefined };

// nodeRemove is the recursive delete (copy-on-write). Returns the rebuilt subtree (possibly
// underfull — the caller rebalances it) and the removed row. A separator found in an interior node
// is replaced by its in-order predecessor (drawn from the left subtree), which is then deleted from
// that subtree; the touched child is rebalanced via rebalanceChild.
function nodeRemove(n: PNode, key: Uint8Array, src: LeafSource | null, cap: number): RemOut {
  const { index, found } = search(n, key);
  if (found) {
    if (isLeaf(n)) {
      const removed = n.vals[index];
      return {
        ok: true,
        node: {
          keys: removeAt(n.keys, index),
          vals: removeAt(n.vals, index),
          weights: removeAt(n.weights, index),
          children: [],
          page: 0,
        },
        removed,
      };
    }
    const removed = n.vals[index];
    // Fault the left subtree once; both the predecessor lookup and its deletion descend it.
    const leftChild = resolveChild(n.children[index], src);
    const pred = maxKV(leftChild, src);
    const child = nodeRemove(leftChild, pred.key, src, cap).node;
    const keys = n.keys.slice();
    const vals = n.vals.slice();
    const weights = n.weights.slice();
    const children = n.children.slice();
    keys[index] = pred.key;
    vals[index] = pred.val;
    weights[index] = pred.weight;
    children[index] = residentRef(child);
    const rebuilt: PNode = { keys, vals, weights, children, page: 0 };
    return { ok: true, node: rebalanceChild(rebuilt, index, src, cap), removed };
  }
  if (isLeaf(n)) {
    return { ok: false, node: n, removed: undefined };
  }
  const res = nodeRemove(resolveChild(n.children[index], src), key, src, cap);
  if (!res.ok) return { ok: false, node: n, removed: undefined };
  const children = n.children.slice();
  children[index] = residentRef(res.node);
  const rebuilt: PNode = {
    keys: n.keys.slice(),
    vals: n.vals.slice(),
    weights: n.weights.slice(),
    children,
    page: 0,
  };
  return { ok: true, node: rebalanceChild(rebuilt, index, src, cap), removed: res.removed };
}

// rebalanceChild: if children[i] is underfull (payload < cap/2), merge it with an adjacent sibling
// (prefer the right one), then split the merged node back if it overflows — the unified rebalance
// (no borrow). The returned parent may itself have lost a key and become underfull; its own parent
// handles that as the recursion unwinds.
function rebalanceChild(n: PNode, i: number, src: LeafSource | null, cap: number): PNode {
  // children[i] was just rebuilt resident by nodeRemove, so inspecting it faults nothing.
  if (payload(resolveChild(n.children[i], src)) >= cap / 2) return n;
  const j = i + 1 < n.children.length ? i : i - 1;
  return mergeAt(n, j, src, cap);
}

// mergeAt merges children[j], separator j, and children[j+1] into one node M. If M fits, it replaces
// the pair and the parent loses separator j and child j+1. If M overflows, it is split 2-way and the
// two halves + the new separator replace the pair (the parent's key count is unchanged). M < 2·cap
// always (format.md), so a single split restores fit.
function mergeAt(n: PNode, j: number, src: LeafSource | null, cap: number): PNode {
  // Fault both children — the underfull child (just rebuilt resident) and its sibling, which may
  // still be an OnDisk leaf the delete never touched.
  const left = resolveChild(n.children[j], src);
  const right = resolveChild(n.children[j + 1], src);
  const mkeys = [...left.keys, n.keys[j], ...right.keys];
  const mvals = [...left.vals, n.vals[j], ...right.vals];
  const mweights = [...left.weights, n.weights[j], ...right.weights];
  const mchildren: Child[] = isLeaf(left) ? [] : [...left.children, ...right.children];

  const keys = n.keys.slice();
  const vals = n.vals.slice();
  const weights = n.weights.slice();
  const children = n.children.slice();

  // Merge-overflow: balanced split (format.md — no edited position exists here).
  const out = build(mkeys, mvals, mweights, mchildren, cap, false);
  if (out.whole !== null) {
    keys.splice(j, 1);
    vals.splice(j, 1);
    weights.splice(j, 1);
    children[j] = residentRef(out.whole);
    children.splice(j + 1, 1);
    return { keys, vals, weights, children, page: 0 };
  }
  keys[j] = out.midK;
  vals[j] = out.midV;
  weights[j] = out.midW;
  children[j] = residentRef(out.left);
  children[j + 1] = residentRef(out.right);
  return { keys, vals, weights, children, page: 0 };
}

// --- immutable array helpers (each returns a fresh array, leaving the input untouched) -------

function insertAt<T>(a: T[], i: number, x: T): T[] {
  const out = a.slice();
  out.splice(i, 0, x);
  return out;
}

function removeAt<T>(a: T[], i: number): T[] {
  const out = a.slice();
  out.splice(i, 1);
  return out;
}
