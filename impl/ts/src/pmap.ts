// Persistent (copy-on-write) ordered map — the page-backed B+tree (decision B1,
// spec/design/bplus-reshape.md; spec/design/transactions.md §3; spec/fileformat/format.md "The
// per-table data B+tree").
//
// Keyed by the encoded key bytes (compared lexicographically = memcmp = the order-preserving
// key encoding's contract, spec/design/encoding.md). Every mutation returns a new map that shares
// structure with the old one; nodes are immutable by convention, so `clone()` (which shares the
// root) is an O(1) independent snapshot. That cheap, structurally-shared snapshot carries the §3
// staging-buffer / transaction model (transactions.md §2).
//
// This IS the on-disk B+tree, node-for-page (v24). Records live ONLY in leaves; an interior node
// is a record-free routing skeleton — separator keys + child pointers. A separator is a COPY of a
// boundary key (a leaf split copies the right half's first key up; an interior split pushes its
// median separator up) and may go stale after deletes — it keeps routing (left < sep ≤ right holds
// forever). Fan-out is size-driven: a node holds as many entries as fit a page payload cap
// (= page_size − 16) and splits when it would overflow, so the node boundaries (and serialized
// bytes) are a §8 byte contract (format.md). The caller supplies each leaf entry's on-disk weight
// (record size); cap and the leaf's column-class shape are passed per call (held by the
// TableStore). Delete rebalances by merge-then-maybe-split (no borrow — format.md "Delete"; an
// interior merge whose result cannot 2-way split is ABANDONED).

import type { Row } from "./storage.ts";
import type { Value } from "./value.ts";

// PackedLeaf is the block-backed resident form of a demand-paged clean leaf (packed-leaf.md §5): the
// page block + the parsed PAX directories, reconstructing rows on demand instead of storing a decoded
// row vector. Its methods close over the format-layer value codec (readValueLazy over a
// Uint8Array.subarray of the page block, GC-pinned), so pmap calls them through this interface rather
// than importing the format layer — avoiding a format↔pmap import cycle (like leafOverhead
// below). Built by format.ts decodeLeafNode.
export interface PackedLeaf {
  readonly n: number;
  // Reconstruct the whole value row i — uniformly LAZY (bplus-reshape.md B4): fixed-width columns
  // decode eagerly, variable columns defer as self-resolving unfetched values.
  row(i: number): Row;
  // Reconstruct only column c of row i — the touched-column path (the A2/A3 columnar gather).
  col(i: number, c: number): Value;
}

// One B+tree node. `children` is empty for a leaf; otherwise children.length === keys.length+1 and
// the node is a record-free interior (v24): its keys are the routing separators and its
// vals/weights are EMPTY. For a leaf keys.length === vals.length === weights.length (or a `packed`
// block in place of vals); weights[i] is record i's on-disk size, for the size-driven split/merge.
// page is the on-disk page index (0 when dirty), set once at the commit that first persists this
// node. Exported so the serializer (format.ts) can read/build it.
export type PNode = {
  keys: Uint8Array[];
  // The decoded value rows — populated for a Decoded leaf (the writer's transient
  // materialize-mutate-repack scratch, bplus-reshape.md §5; the post-commit residency flip demotes
  // it once persisted, so Decoded survives a commit only in a root leaf, a GiST leaf-key store, or
  // a bare scratch engine), empty for a Packed leaf (which
  // reconstructs on demand from `packed`) and for EVERY interior node (record-free, v24). Read only
  // through the rowAt / colAt / decodedRows seam on leaves, never indexed directly, so the two leaf
  // forms are interchangeable (packed-leaf.md §3/§4).
  vals: Row[];
  weights: number[];
  children: Child[];
  // The block-backed resident form of a demand-paged clean leaf (packed-leaf.md §5); undefined for a
  // Decoded node (in-memory/loaded leaves, any dirty leaf — mutation materializes Packed→Decoded
  // first, §7 — and every interior node). A Packed leaf is always clean (page != 0), so it is never
  // serialized.
  packed?: PackedLeaf;
  page: number;
};

// rowAt reconstructs value row i — the value-read seam (packed-leaf.md §4), on a LEAF. A Decoded
// leaf returns the stored row (read-only by convention); a Packed leaf reconstructs it from the
// retained PAX directories. Throws on a corrupt touched inline body (XX001). The old two-form
// masked/unmasked reconstruction seam (rowAtMasked / rowAtMaybeMasked) is COLLAPSED
// (bplus-reshape.md B4): a Packed leaf's reconstruction is uniformly lazy (fixed-width columns
// decode eagerly, variable columns defer as self-resolving unfetched values), so a reconstruction
// mask no longer exists — the query's touched set survives as the cost basis + the scan layer's
// resolve prefetch, and a missed value resolves on touch (the demand-fault backstop).
export function rowAt(n: PNode, i: number): Row {
  return n.packed ? n.packed.row(i) : n.vals[i]!;
}

// colAt reconstructs ONLY column c of row i — the touched-column path (packed-leaf.md §4/§6, the
// OP_Column model).
export function colAt(n: PNode, i: number, c: number): Value {
  return n.packed ? n.packed.col(i, c) : n.vals[i]![c]!;
}

// decodedRows returns every value row of a LEAF — the mutation-descent materialization
// (packed-leaf.md §7). A Decoded leaf clones vals; a Packed leaf reconstructs every row so the
// rebuilt node is Decoded (buildLeaf / nodeInsert / nodeRemove / mergeAt then run unchanged).
export function decodedRows(n: PNode): Row[] {
  if (!n.packed) return n.vals.slice();
  const rows: Row[] = new Array(n.packed.n);
  for (let i = 0; i < n.packed.n; i++) rows[i] = n.packed.row(i);
  return rows;
}

// A B+tree node's reference to one child. Under demand paging (P6.4b, spec/design/pager.md §4) a
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
// behind the table's column types. Defined here so the B+tree traversal can fault without importing
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

// LeafShape is a leaf's column-class shape — the two counts leafOverhead needs beyond N (the fixed
// and variable column counts, fixed + var = K). Computed once per store from its column types
// (format.ts leafShape) and threaded through the B+tree's size arithmetic, which never sees the
// types themselves. An index tree (zero value columns) is { fixed: 0, var: 0 }.
export type LeafShape = { fixed: number; var: number };

// leafOverhead is the bytes a v24 leaf's payload carries beyond Σ record_size (format.md "Leaf
// node"): the key directory (4·N), the column directory (4·(K+1)), and per region a flags byte plus
// — fixed-width — the null bitmap (ceil(N/8)) or — variable-width — the value directory (4·N).
// Defined here (not format.ts) to avoid a format↔pmap import cycle; the cross-core goldens catch
// any drift. Interior nodes do not use this (their payload is 8·N + 4 + Σ sep_len).
//   leafOverhead(N, cols) = 4·N + 4·(K+1) + F·(1 + ceil(N/8)) + V·(1 + 4·N)
export function leafOverhead(n: number, shape: LeafShape): number {
  const k = shape.fixed + shape.var;
  return 4 * n + 4 * (k + 1) + shape.fixed * (1 + Math.ceil(n / 8)) + shape.var * (1 + 4 * n);
}

// payload is this node's serialized size (format.md): a leaf is Σ weights + leafOverhead(N, shape);
// an interior node is 8·N + 4 + Σ sep_len (child pointers + separator directory + key blob —
// record-free, v24).
function payload(n: PNode, shape: LeafShape): number {
  if (isLeaf(n)) {
    let total = 0;
    for (const w of n.weights) total += w;
    return total + leafOverhead(n.keys.length, shape);
  }
  let seps = 0;
  for (const k of n.keys) seps += k.length;
  return 8 * n.keys.length + 4 + seps;
}

export function compareBytes(a: Uint8Array, b: Uint8Array): number {
  const n = a.length < b.length ? a.length : b.length;
  for (let i = 0; i < n; i++) {
    if (a[i] !== b[i]) return a[i] < b[i] ? -1 : 1;
  }
  return a.length === b.length ? 0 : a.length < b.length ? -1 : 1;
}

// KeyBound is a contiguous range of encoded keys — the form a primary-key predicate pushes down to a
// bounded B+tree scan (spec/design/cost.md §3 "bounded scan / point lookup", encoding.md). lo/hi are
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

// childWindow: the contiguous window [first, last] of an INTERIOR node's child indices whose key
// span can overlap the bound. Child i spans [sep[i-1], sep[i]) (v24 — a key equal to a separator
// lies right), so child i is pruned iff sep[i] ≤ lo (entirely at/below lo) or sep[i-1] is at/above
// hi — > hi for an inclusive hi (a child whose low separator equals hi can still hold hi itself),
// ≥ hi for an exclusive one. The separators are sorted, so the surviving children are contiguous
// and both edges binary-search. rangeEntries (descends) and overlapNodeCount (counts) window
// identically, so they visit the SAME node set — the §8 determinism page_read depends on — decided
// from resident separators WITHOUT faulting an OnDisk leaf.
function childWindow(b: KeyBound, n: PNode): [number, number] {
  const first = b.lo === null ? 0 : lowerBoundGT(n.keys, b.lo);
  const last =
    b.hi === null
      ? n.keys.length
      : b.hiInc
        ? lowerBoundGT(n.keys, b.hi)
        : lowerBoundGE(n.keys, b.hi);
  return [first, last < first ? first : last];
}

// entryWindow: the contiguous half-open window [first, last) of a LEAF's record indices whose keys
// lie within the bound — the binary-searched equivalent of testing containment per key, honoring
// the endpoint inclusivity flags. Applies only at leaves (interior nodes carry no records, v24).
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

// search binary-searches a LEAF's keys: found ⇒ keys[index] === key, else index is the insertion slot.
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

// childSlot is the child an INTERIOR descent takes for key: partition_point(sep ≤ key) — a key
// equal to a separator lies in the RIGHT subtree (the copy-up separator is the right half's first
// key; format.md "Interior node").
function childSlot(n: PNode, key: Uint8Array): number {
  return lowerBoundGT(n.keys, key);
}

// newLeaf / newInterior build a fresh DIRTY node (page 0) — every copy-on-write rebuild goes
// through here. An interior node carries no records (empty vals/weights, v24).
function newLeaf(keys: Uint8Array[], vals: Row[], weights: number[]): PNode {
  return { keys, vals, weights, children: [], page: 0 };
}

function newInterior(keys: Uint8Array[], children: Child[]): PNode {
  return { keys, vals: [], weights: [], children, page: 0 };
}

// PMap is a persistent ordered map from encoded key to Row. `clone()` is an O(1) independent
// snapshot (the root is shared; nodes are immutable).
//
// rowCount is the exact nonnegative row count when known. Table maps are built from empty or
// restored with their v28 catalog count and maintain it across insert/remove. Index maps loaded
// from a disk skeleton carry null because index cardinality is not persisted. isEmpty never
// consults it: it derives emptiness from the root, exact and O(1) whether or not the count is known.
export class PMap {
  private root: PNode | null;
  private rowCount: bigint | null;

  constructor(root: PNode | null = null, rowCount: bigint | null = 0n) {
    this.root = root;
    this.rowCount = rowCount;
  }

  clone(): PMap {
    return new PMap(this.root, this.rowCount);
  }

  // getCount returns the exact nonnegative row count, or null for an index skeleton whose count is
  // not persisted.
  getCount(): bigint | null {
    return this.rowCount;
  }

  isEmpty(): boolean {
    return this.root === null;
  }

  // rootNode exposes the root node to the serializer (format.ts). null for an empty map.
  rootNode(): PNode | null {
    return this.root;
  }

  // get looks up the row at key — a root→leaf descent (interior nodes only route, v24). src faults
  // an OnDisk leaf on the descent (null for a fully-resident in-memory tree); an I/O error
  // propagates as a thrown EngineError.
  get(key: Uint8Array, src: LeafSource | null): Row | undefined {
    let n = this.root;
    if (n === null) return undefined;
    while (!isLeaf(n)) {
      n = resolveChild(n.children[childSlot(n, key)], src);
    }
    const { index, found } = search(n, key);
    return found ? rowAt(n, index) : undefined;
  }

  // insert inserts or overwrites key with val (on-disk record size weight); cap is the page payload
  // capacity and shape the leaf's column-class shape. Returns the previous row if key was present
  // (an overwrite, size unchanged), else undefined (a new insert, which grows the size). An
  // overwrite can change the weight, so it too may overflow and split.
  insert(
    key: Uint8Array,
    val: Row,
    weight: number,
    cap: number,
    shape: LeafShape,
    src: LeafSource | null,
  ): Row | undefined {
    if (this.root === null) {
      this.root = newLeaf([key], [val], [weight]);
      if (this.rowCount !== null) this.rowCount += 1n;
      return undefined;
    }
    const ctx: InsCtx = { old: undefined, replaced: false };
    const out = nodeInsert(this.root, key, val, weight, ctx, src, cap, shape);
    this.root =
      out.whole !== null
        ? out.whole
        : newInterior([out.sep], [residentRef(out.left), residentRef(out.right)]);
    if (!ctx.replaced && this.rowCount !== null) this.rowCount += 1n;
    return ctx.old;
  }

  // remove deletes key. Returns the removed row, or undefined if absent (then the map is unchanged).
  // cap is the page payload capacity and shape the leaf's column-class shape (the rebalance
  // threshold). src faults OnDisk leaves the delete descends into / rebalances against
  // (spec/design/pager.md §4).
  remove(key: Uint8Array, cap: number, shape: LeafShape, src: LeafSource | null): Row | undefined {
    if (this.root === null) return undefined;
    const res = nodeRemove(this.root, key, src, cap, shape);
    if (!res.ok) return undefined;
    const newRoot = res.node;
    // The root may have drained: an empty leaf becomes the empty map; a 0-key interior root hands
    // the root down a level (height shrinks). The root is exempt from the underfull rule, so no
    // rebalance here.
    if (newRoot.keys.length === 0) {
      // The lone surviving child becomes the new root — fault it if it is an OnDisk leaf (a tree of
      // height 2 can collapse to its single bottom child).
      this.root = isLeaf(newRoot) ? null : resolveChild(newRoot.children[0], src);
    } else {
      this.root = newRoot;
    }
    if (this.rowCount !== null) this.rowCount -= 1n;
    return res.removed;
  }

  // inorder returns all (key, row) pairs in ascending key order — a leaf walk (records are
  // leaf-only, v24). Eager (the cost contract charges per row in the executor loop, not here —
  // spec/design/cost.md); src faults each OnDisk leaf through the pool, and the faulted node is
  // dropped (GC) once its rows are collected, so the resident leaf set stays bounded by the pool,
  // not the tree (pager.md §4).
  inorder(src: LeafSource | null): { keys: Uint8Array[]; vals: Row[] } {
    const keys: Uint8Array[] = [];
    const vals: Row[] = [];
    const walk = (n: PNode): void => {
      if (isLeaf(n)) {
        for (let i = 0; i < n.keys.length; i++) {
          keys.push(n.keys[i]);
          vals.push(rowAt(n, i));
        }
        return;
      }
      for (const c of n.children) walk(resolveChild(c, src));
    };
    if (this.root !== null) walk(this.root);
    return { keys, vals };
  }

  // nodeCount is the number of B+tree nodes (pages) in this tree — the page_read count a full
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
  // leaf entry's weight (records live only in leaves, v24). The deterministic, cross-core-identical
  // measure of a temp table's storage footprint (spec/design/temp-tables.md §7; weight is the
  // on-disk record_size, byte-identical across cores — §8). The tree is fully resident for a temp
  // store (temp data never pages), so this never faults; an OnDisk child contributes 0 (defensive —
  // temp stores have none).
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
  // — a bounded in-order traversal that binary-searches each interior node's child window (the
  // children whose separator span can overlap the bound — childWindow) and each leaf's in-bound
  // entry window (entryWindow), then walks only those, so only overlapping leaves fault through src.
  // The unbounded bound walks the whole tree (identical to inorder).
  rangeEntries(b: KeyBound, src: LeafSource | null): { keys: Uint8Array[]; vals: Row[] } {
    const { keys, vals } = this.rangeEntriesCounted(b, src);
    return { keys, vals };
  }

  // rangeEntriesCounted is rangeEntries plus the number of B+tree nodes the bounded traversal
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
      if (isLeaf(n)) {
        const [ef, el] = entryWindow(b, n);
        for (let i = ef; i < el; i++) {
          keys.push(n.keys[i]);
          vals.push(rowAt(n, i));
        }
        return;
      }
      const [cf, cl] = childWindow(b, n);
      for (let i = cf; i <= cl; i++) {
        walk(resolveChild(n.children[i], src));
      }
    };
    if (this.root !== null) walk(this.root);
    return { keys, vals, nodes };
  }

  // columnarScan walks the bounded scan gathering ONLY the columns mask selects into dense per-column
  // lanes (cols[c] of length rowCount for each selected c, empty otherwise), never building a full-width
  // Row — the A2/A3 columnar-gather feed (packed-leaf.md §11 Track A2, the allocation dividend A1 leaves
  // on the table). It mirrors rangeEntriesCounted's traversal EXACTLY (same node visits ⇒ the same
  // page_read count; same in-order record sequence — leaf-only, v24), but reads each admitted row's
  // selected columns via colAt — an O(1) PAX column span on a Packed leaf, vals[i][c] on a Decoded
  // leaf — so a wide-table single-column scan never materializes the untouched columns NOR a
  // full-width row. Each cols[c] is in scan order, so it equals the column-c stride of the row feed.
  // rowCount is the admitted record count.
  columnarScan(
    b: KeyBound,
    src: LeafSource | null,
    mask: boolean[],
  ): { cols: Value[][]; rowCount: number; nodes: number } {
    const k = mask.length;
    const cols: Value[][] = Array.from({ length: k }, () => []);
    let rowCount = 0;
    let nodes = 0;
    const gather = (n: PNode, i: number): void => {
      for (let c = 0; c < k; c++) {
        if (mask[c]) cols[c]!.push(colAt(n, i, c));
      }
      rowCount++;
    };
    const walk = (n: PNode): void => {
      nodes++;
      if (isLeaf(n)) {
        const [ef, el] = entryWindow(b, n);
        for (let i = ef; i < el; i++) gather(n, i);
        return;
      }
      const [cf, cl] = childWindow(b, n);
      for (let i = cf; i <= cl; i++) walk(resolveChild(n.children[i], src));
    };
    if (this.root !== null) walk(this.root);
    return { cols, rowCount, nodes };
  }

  // foldScan is the fold-during-walk twin of columnarScan (packed-leaf.md §11): the identical windowed
  // walk (so the visited-node set — and page_read — is identical), but calls visit(n, i) per admitted
  // leaf record instead of gathering its columns into lanes. visit reads the record's touched columns
  // via colAt and folds them straight into an accumulator, so a whole-table / single-int-key
  // aggregate is O(1) memory instead of O(rows). Returns the same { rowCount, nodes } as
  // columnarScan, so the caller charges the same page_read / storage_row_read.
  foldScan(
    b: KeyBound,
    src: LeafSource | null,
    visit: (n: PNode, i: number) => void,
  ): { rowCount: number; nodes: number } {
    let rowCount = 0;
    let nodes = 0;
    const walk = (n: PNode): void => {
      nodes++;
      if (isLeaf(n)) {
        const [ef, el] = entryWindow(b, n);
        for (let i = ef; i < el; i++) {
          visit(n, i);
          rowCount++;
        }
        return;
      }
      const [cf, cl] = childWindow(b, n);
      for (let i = cf; i <= cl; i++) walk(resolveChild(n.children[i], src));
    };
    if (this.root !== null) walk(this.root);
    return { rowCount, nodes };
  }

  // overlapNodeCount is the number of B+tree nodes a bounded scan over b visits — the page_read it
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
  // rangeEntries; only leaves emit (records are leaf-only, v24).
  scanRange(
    b: KeyBound,
    src: LeafSource | null,
    visit: (key: Uint8Array, row: Row) => boolean,
  ): void {
    const walk = (n: PNode): boolean => {
      if (isLeaf(n)) {
        const [ef, el] = entryWindow(b, n);
        for (let i = ef; i < el; i++) {
          if (!visit(n.keys[i], rowAt(n, i))) return false;
        }
        return true;
      }
      const [cf, cl] = childWindow(b, n);
      for (let i = cf; i <= cl; i++) {
        if (!walk(resolveChild(n.children[i], src))) return false;
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
  // An interior node descends its windowed children from cl down to cf.
  scanRangeRev(
    b: KeyBound,
    src: LeafSource | null,
    visit: (key: Uint8Array, row: Row) => boolean,
  ): void {
    const walk = (n: PNode): boolean => {
      if (isLeaf(n)) {
        const [ef, el] = entryWindow(b, n);
        for (let i = el - 1; i >= ef; i--) {
          if (!visit(n.keys[i], rowAt(n, i))) return false;
        }
        return true;
      }
      const [cf, cl] = childWindow(b, n);
      for (let i = cl; i >= cf; i--) {
        if (!walk(resolveChild(n.children[i], src))) return false;
      }
      return true;
    };
    if (this.root !== null) walk(this.root);
  }

  // scanRangeIter is the PULL-model equivalent of scanRange (the S2 pull B+tree scan cursor,
  // spec/design/streaming.md §3/§5): instead of PUSHING each in-bound row to a visit callback, it
  // YIELDS one [key, row] pair per pull, so the CALLER owns the control flow. In TS the natural pull
  // form is a generator (not the explicit frame stack the Rust/Go cores use), but it yields the EXACT
  // same sequence as scanRange, faulting a clean leaf through src only when the traversal descends
  // into it (via walkIter's resolveChild) — so a consumer that stops early (breaks a for-of, or
  // .return()s the generator) faults no leaves past where it stopped (the genuine LIMIT short-
  // circuit, cost.md §3). It yields the stored row reference (like scanRange's callback); the GC keeps
  // a faulted leaf's row alive as long as a pulled row references it, even after the pool evicts the
  // leaf. A faulted-leaf read error in resolveChild propagates as a thrown exception.
  *scanRangeIter(b: KeyBound, src: LeafSource | null): Generator<[Uint8Array, Row]> {
    if (this.root !== null) yield* walkIter(this.root, b, src);
  }

  // scanRangeRevIter is scanRangeIter in reverse — the pull-model equivalent of scanRangeRev,
  // yielding the in-bound pairs in DESCENDING key order (the exact reverse of scanRangeIter).
  *scanRangeRevIter(b: KeyBound, src: LeafSource | null): Generator<[Uint8Array, Row]> {
    if (this.root !== null) yield* walkRevIter(this.root, b, src);
  }

  // demoteCleanLeaves demotes every CLEAN, PERSISTED resident leaf to its OnDisk(page) reference —
  // the post-commit residency flip (bplus-reshape.md B4): after a commit assigns page ids to the
  // dirty nodes it wrote, the committed tree sheds its leaf payloads and becomes the skeletal
  // `interior nodes + OnDisk leaves` shape every load already produces, so reads everywhere go
  // through the one Packed pool path and Decoded survives only inside an uncommitted writer
  // (write-scratch). A ROOT leaf stays resident (the PMap root is always a node — the open/load
  // convention); an unpersisted (page 0) leaf is left alone (defensive — a bare scratch engine that
  // never persists). Rebuilds only the interior spine above changed children; an unchanged subtree
  // keeps its node object (and its set-once page id), so the flip is O(interior nodes) and the
  // flipped tree stays clean for the next incremental commit.
  demoteCleanLeaves(): void {
    const demote = (node: PNode): PNode | null => {
      if (isLeaf(node)) return null; // handled by the parent (a root leaf stays resident)
      let changed = false;
      const children: Child[] = new Array(node.children.length);
      for (let i = 0; i < node.children.length; i++) {
        const c = node.children[i]!;
        if (c.node === null) {
          children[i] = c; // already OnDisk
        } else if (isLeaf(c.node)) {
          if (c.node.page !== 0) {
            changed = true;
            children[i] = onDiskRef(c.node.page);
          } else {
            children[i] = c;
          }
        } else {
          const rebuilt = demote(c.node);
          if (rebuilt !== null) {
            changed = true;
            children[i] = residentRef(rebuilt);
          } else {
            children[i] = c;
          }
        }
      }
      if (!changed) return null;
      // The rebuilt interior keeps its keys AND its page id — its serialized bytes are unchanged
      // (children reference the same pages), so it must stay clean or the next incremental commit
      // would rewrite the whole spine every time.
      return { keys: node.keys, vals: [], weights: [], children, page: node.page };
    };
    if (this.root !== null) {
      const rebuilt = demote(this.root);
      if (rebuilt !== null) this.root = rebuilt;
    }
  }
}

// walkIter mirrors PMap.scanRange's recursive in-order walk, yielding [key, row] instead of calling a
// visit callback — so it is identical by construction (same structure, same windowing, same descent
// order; only leaves yield, v24).
function* walkIter(n: PNode, b: KeyBound, src: LeafSource | null): Generator<[Uint8Array, Row]> {
  if (isLeaf(n)) {
    const [ef, el] = entryWindow(b, n);
    for (let i = ef; i < el; i++) yield [n.keys[i], rowAt(n, i)];
    return;
  }
  const [cf, cl] = childWindow(b, n);
  for (let i = cf; i <= cl; i++) {
    yield* walkIter(resolveChild(n.children[i], src), b, src);
  }
}

// walkRevIter mirrors PMap.scanRangeRev's reverse walk, yielding instead of pushing.
function* walkRevIter(n: PNode, b: KeyBound, src: LeafSource | null): Generator<[Uint8Array, Row]> {
  if (isLeaf(n)) {
    const [ef, el] = entryWindow(b, n);
    for (let i = el - 1; i >= ef; i--) yield [n.keys[i], rowAt(n, i)];
    return;
  }
  const [cf, cl] = childWindow(b, n);
  for (let i = cl; i >= cf; i--) {
    yield* walkRevIter(resolveChild(n.children[i], src), b, src);
  }
}

// pmapFromSkeleton reconstructs a map from a disk-loaded skeleton root (format.ts loadEnginePaged).
// Tables pass their exact v28 catalog count; indexes pass null.
export function pmapFromSkeleton(root: PNode | null, rowCount: bigint | null): PMap {
  return new PMap(root, rowCount);
}

type InsCtx = { old: Row | undefined; replaced: boolean };
// The result of inserting into a subtree: the rebuilt subtree, or a node that overflowed and split
// into left, a SEPARATOR KEY for the parent, and right. A leaf split COPIES the right leaf's first
// key up (no record leaves the leaf level); an interior split PUSHES its median separator up
// (format.md "Fan-out").
type InsOut = { whole: PNode } | { whole: null; left: PNode; sep: Uint8Array; right: PNode };

// splitPoint is the kind-shared split decision (format.md "Split point"): given the per-boundary
// leftpayload/rightpayload functions over m in [mLo, mHi], pick
// m = rightEdge ? m_max : clamp(min(m_balanced, m_max), m_min, m_max), or null when no m in the
// range keeps both sides fitting (the interior merge-abandon case — unreachable on the insert path,
// format.md "Why the record cap"). A rightEdge edit (the just-inserted/replaced entry is the node's
// last) takes the largest-left-fit append split — sequential ascending loads pack left nodes ~full;
// anywhere else (and the delete path's merge-overflow) splits balanced, without which largest-left
// degenerates to [N-2 | 1] splinters under random-order inserts (the benchmarks.md finding).
function splitPoint(
  mLo: number,
  mHi: number,
  total: number,
  cap: number,
  rightEdge: boolean,
  leftpayload: (m: number) => number,
  rightpayload: (m: number) => number,
): number | null {
  // leftpayload is nondecreasing in m and rightpayload nonincreasing, so both bounds scan cleanly;
  // the ranges are tiny (page fan-out), so a linear scan is clearer than a binary search.
  let mMax = -1;
  for (let m = mLo; m <= mHi; m++) {
    if (leftpayload(m) <= cap) mMax = m;
    else break;
  }
  if (mMax < 0) return null;
  let mMin = -1;
  for (let m = mHi; m >= mLo; m--) {
    if (rightpayload(m) <= cap) mMin = m;
    else break;
  }
  if (mMin < 0 || mMin > mMax) return null;
  if (rightEdge) return mMax;
  let mBalanced = mMax;
  for (let m = mLo; m <= mHi; m++) {
    if (2 * leftpayload(m) >= total) {
      mBalanced = m;
      break;
    }
  }
  return Math.max(Math.min(mBalanced, mMax), mMin);
}

// buildLeaf builds a leaf from its parts; if its payload overflows cap, it splits 2-way COPY-UP
// (format.md "Leaf split"): the left leaf keeps records [0, m), the right leaf [m, N), and the
// separator handed up is a COPY of keys[m] (the right leaf's first key). edited is the index of
// the just-inserted/replaced record (null for the delete path's merge-overflow, which splits
// balanced). A leaf with a single over-cap record is left whole (defensive — the oversize surfaces
// as 0A000 when serialized).
function buildLeaf(
  keys: Uint8Array[],
  vals: Row[],
  weights: number[],
  cap: number,
  shape: LeafShape,
  edited: number | null,
): InsOut {
  const n = keys.length;
  let total = 0;
  for (const w of weights) total += w;
  const pay = total + leafOverhead(n, shape);
  if (pay <= cap || n < 2) return { whole: newLeaf(keys, vals, weights) };

  const prefix = new Array<number>(n + 1);
  prefix[0] = 0;
  for (let i = 0; i < n; i++) prefix[i + 1] = prefix[i]! + weights[i]!;
  const leftpayload = (m: number): number => prefix[m]! + leafOverhead(m, shape);
  const rightpayload = (m: number): number => total - prefix[m]! + leafOverhead(n - m, shape);
  const m = splitPoint(1, n - 1, pay, cap, edited === n - 1, leftpayload, rightpayload);
  // Unreachable under the RECORD_MAX cap (a two-record leaf always fits — format.md "Why the
  // record cap"); defensively leave the node whole (0A000 at serialize).
  if (m === null) return { whole: newLeaf(keys, vals, weights) };

  return {
    whole: null,
    left: newLeaf(keys.slice(0, m), vals.slice(0, m), weights.slice(0, m)),
    sep: keys[m]!,
    right: newLeaf(keys.slice(m), vals.slice(m), weights.slice(m)),
  };
}

// buildInterior builds an interior node from its parts; if its payload overflows cap, it splits
// 2-way PUSH-UP (format.md "Interior split"): the left node keeps separators [0, m) + children
// [0, m], separator m moves up, the right node keeps [m+1, N) + children [m+1, N]. With N = 2
// (only reachable with near-cap separators) the split is pinned to m = 1, producing a legal N = 0
// right interior (the degenerate fan-out contract). Returns null when the node overflows and no
// valid split point exists — the caller (only the interior MERGE path can hit it) abandons the merge.
function buildInterior(
  keys: Uint8Array[],
  children: Child[],
  cap: number,
  edited: number | null,
): InsOut | null {
  const n = keys.length;
  let seps = 0;
  for (const k of keys) seps += k.length;
  const pay = 8 * n + 4 + seps;
  if (pay <= cap || n < 2) return { whole: newInterior(keys, children) };

  let m: number;
  if (n === 2) {
    // The degenerate pin (format.md "Interior split"): the left keeps sep[0] (fits, by the
    // minimum-fanout invariant), sep[1] moves up, the right is the legal N = 0 interior.
    m = 1;
  } else {
    const prefix = new Array<number>(n + 1);
    prefix[0] = 0;
    for (let i = 0; i < n; i++) prefix[i + 1] = prefix[i]! + keys[i]!.length;
    const total = prefix[n]!;
    const leftpayload = (mm: number): number => 8 * mm + 4 + prefix[mm]!;
    const rightpayload = (mm: number): number => 8 * (n - 1 - mm) + 4 + (total - prefix[mm + 1]!);
    const got = splitPoint(1, n - 2, pay, cap, edited === n - 1, leftpayload, rightpayload);
    if (got === null) return null;
    m = got;
  }

  return {
    whole: null,
    left: newInterior(keys.slice(0, m), children.slice(0, m + 1)),
    sep: keys[m]!,
    right: newInterior(keys.slice(m + 1), children.slice(m + 1)),
  };
}

// nodeInsert is the recursive insert. It descends to the holding LEAF (interior nodes only route,
// via childSlot); on overwrite it sets ctx and rebuilds with the value+weight replaced (which may
// now overflow). Splits propagate back up: a leaf split copies its boundary key up, an interior
// receiving a separator may push-split in turn.
function nodeInsert(
  n: PNode,
  key: Uint8Array,
  val: Row,
  weight: number,
  ctx: InsCtx,
  src: LeafSource | null,
  cap: number,
  shape: LeafShape,
): InsOut {
  if (isLeaf(n)) {
    const { index, found } = search(n, key);
    if (found) {
      const vals = decodedRows(n);
      const weights = n.weights.slice();
      ctx.old = vals[index];
      ctx.replaced = true;
      vals[index] = val;
      weights[index] = weight;
      return buildLeaf(n.keys.slice(), vals, weights, cap, shape, index);
    }
    return buildLeaf(
      insertAt(n.keys, index, key),
      insertAt(decodedRows(n), index, val),
      insertAt(n.weights, index, weight),
      cap,
      shape,
      index,
    );
  }
  // Fault the target child (a resident interior, or an OnDisk leaf brought in for mutation — it
  // becomes a dirty resident node on the rebuilt path).
  const i = childSlot(n, key);
  const sub = nodeInsert(resolveChild(n.children[i], src), key, val, weight, ctx, src, cap, shape);
  if (sub.whole !== null) {
    // This node's separators are unchanged, so it cannot overflow — rebuild whole.
    const children = n.children.slice();
    children[i] = residentRef(sub.whole);
    return { whole: newInterior(n.keys.slice(), children) };
  }
  const keys = insertAt(n.keys, i, sub.sep);
  let children = n.children.slice();
  children[i] = residentRef(sub.left);
  children = insertAt(children, i + 1, residentRef(sub.right));
  const out = buildInterior(keys, children, cap, i);
  if (out === null) {
    // The cap arithmetic guarantees a valid split point on the insert path (format.md "Why the
    // record cap") — reaching here would be an internal invariant break, not a data condition.
    throw new Error("insert-path interior split always has a valid split point");
  }
  return out;
}

type RemOut = { ok: boolean; node: PNode; removed: Row | undefined };

// nodeRemove is the recursive delete (copy-on-write). It descends to the holding LEAF (a separator
// equal to the key just routes right — it is never itself deleted or replaced; separators may go
// stale, format.md "Delete"). Returns the rebuilt subtree (possibly underfull — the caller
// rebalances it) and the removed row; the touched child is rebalanced via rebalanceChild.
function nodeRemove(
  n: PNode,
  key: Uint8Array,
  src: LeafSource | null,
  cap: number,
  shape: LeafShape,
): RemOut {
  if (isLeaf(n)) {
    const { index, found } = search(n, key);
    if (!found) return { ok: false, node: n, removed: undefined };
    const rows = decodedRows(n);
    const removed = rows[index];
    return {
      ok: true,
      node: newLeaf(removeAt(n.keys, index), removeAt(rows, index), removeAt(n.weights, index)),
      removed,
    };
  }
  const i = childSlot(n, key);
  const res = nodeRemove(resolveChild(n.children[i], src), key, src, cap, shape);
  if (!res.ok) return { ok: false, node: n, removed: undefined };
  const children = n.children.slice();
  children[i] = residentRef(res.node);
  const rebuilt = newInterior(n.keys.slice(), children);
  return { ok: true, node: rebalanceChild(rebuilt, i, src, cap, shape), removed: res.removed };
}

// rebalanceChild: if children[i] is underfull (payload < cap/2), merge it with an adjacent sibling
// (prefer the right one), then split the merged node back if it overflows — the unified rebalance
// (no borrow). The returned parent may itself have lost a key and become underfull; its own parent
// handles that as the recursion unwinds.
function rebalanceChild(
  n: PNode,
  i: number,
  src: LeafSource | null,
  cap: number,
  shape: LeafShape,
): PNode {
  // children[i] was just rebuilt resident by nodeRemove, so inspecting it faults nothing.
  if (payload(resolveChild(n.children[i], src), shape) >= cap / 2) return n;
  if (n.children.length < 2) {
    // A 0-key interior (one child, the degenerate max-separator shape) has no sibling to merge
    // with — its own parent merges IT away; the root case collapses in PMap.remove.
    return n;
  }
  const j = i + 1 < n.children.length ? i : i - 1;
  return mergeAt(n, j, src, cap, shape);
}

// mergeAt merges children[j] and children[j+1] into one node M (format.md "Delete"): a LEAF merge
// concatenates the two record lists and the parent separator j is REMOVED (it was a routing copy —
// nothing comes down); an INTERIOR merge PULLS the separator DOWN between the two key lists (the
// merged children need a routing key between them). If M fits, it replaces the pair (the parent
// loses one key); if it overflows, it is split 2-way by the balanced rule and the halves + the new
// separator replace the pair (the parent's key count is unchanged). An INTERIOR M that overflows
// but admits no valid split (near-cap separators) ABANDONS the merge — the parent is returned
// unchanged (format.md "Delete", the deterministic abandon rule).
function mergeAt(
  n: PNode,
  j: number,
  src: LeafSource | null,
  cap: number,
  shape: LeafShape,
): PNode {
  // Fault both children — the underfull child (just rebuilt resident) and its sibling, which may
  // still be an OnDisk leaf the delete never touched.
  const left = resolveChild(n.children[j], src);
  const right = resolveChild(n.children[j + 1], src);

  let merged: InsOut;
  if (isLeaf(left)) {
    // Materialize both leaves (either may be Packed) before merging — the merged node is Decoded.
    const mkeys = [...left.keys, ...right.keys];
    const mvals = [...decodedRows(left), ...decodedRows(right)];
    const mweights = [...left.weights, ...right.weights];
    merged = buildLeaf(mkeys, mvals, mweights, cap, shape, null);
  } else {
    const mkeys = [...left.keys, n.keys[j]!, ...right.keys];
    const mchildren = [...left.children, ...right.children];
    const out = buildInterior(mkeys, mchildren, cap, null);
    // No valid 2-way split point (near-cap separators): abandon the merge — the two children and
    // the parent separator stay exactly as they were (underfull tolerated).
    if (out === null) return n;
    merged = out;
  }

  const keys = n.keys.slice();
  const children = n.children.slice();
  if (merged.whole !== null) {
    keys.splice(j, 1);
    children[j] = residentRef(merged.whole);
    children.splice(j + 1, 1);
    return newInterior(keys, children);
  }
  keys[j] = merged.sep;
  children[j] = residentRef(merged.left);
  children[j + 1] = residentRef(merged.right);
  return newInterior(keys, children);
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
