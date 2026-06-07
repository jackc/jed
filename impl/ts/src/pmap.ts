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
// node holds as many entries as fit a page payload cap (= page_size − 12) and splits when it would
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
  children: PNode[];
  page: number;
};

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

function compareBytes(a: Uint8Array, b: Uint8Array): number {
  const n = a.length < b.length ? a.length : b.length;
  for (let i = 0; i < n; i++) {
    if (a[i] !== b[i]) return a[i] < b[i] ? -1 : 1;
  }
  return a.length === b.length ? 0 : a.length < b.length ? -1 : 1;
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

  // get looks up the row at key.
  get(key: Uint8Array): Row | undefined {
    let n = this.root;
    while (n !== null) {
      const { index, found } = search(n, key);
      if (found) return n.vals[index];
      if (isLeaf(n)) return undefined;
      n = n.children[index];
    }
    return undefined;
  }

  // insert inserts or overwrites key with val (on-disk record size weight); cap is the page payload
  // capacity. Returns the previous row if key was present (an overwrite, size unchanged), else
  // undefined (a new insert, which grows the size). An overwrite can change the weight, so it too
  // may overflow and split.
  insert(key: Uint8Array, val: Row, weight: number, cap: number): Row | undefined {
    if (this.root === null) {
      this.root = { keys: [key], vals: [val], weights: [weight], children: [], page: 0 };
      this.length++;
      return undefined;
    }
    const ctx: InsCtx = { old: undefined, replaced: false };
    const out = nodeInsert(this.root, key, val, weight, ctx, cap);
    this.root =
      out.whole !== null
        ? out.whole
        : { keys: [out.midK], vals: [out.midV], weights: [out.midW], children: [out.left, out.right], page: 0 };
    if (!ctx.replaced) this.length++;
    return ctx.old;
  }

  // remove deletes key. Returns the removed row, or undefined if absent (then the map is unchanged).
  // cap is the page payload capacity (the rebalance threshold).
  remove(key: Uint8Array, cap: number): Row | undefined {
    if (this.root === null) return undefined;
    const res = nodeRemove(this.root, key, cap);
    if (!res.ok) return undefined;
    const newRoot = res.node;
    // The root may have drained to zero keys: an empty leaf becomes the empty map; an empty internal
    // node (one child) hands the root down a level (height shrinks). The root is exempt from the
    // underfull rule, so no rebalance here.
    if (newRoot.keys.length === 0) {
      this.root = isLeaf(newRoot) ? null : newRoot.children[0];
    } else {
      this.root = newRoot;
    }
    this.length--;
    return res.removed;
  }

  // inorder returns all (key, row) pairs in ascending key order. Eager (the cost contract charges
  // per row in the executor loop, not here — spec/design/cost.md), so laziness is unobservable.
  inorder(): { keys: Uint8Array[]; vals: Row[] } {
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
        walk(n.children[i]);
        keys.push(n.keys[i]);
        vals.push(n.vals[i]);
      }
      walk(n.children[n.keys.length]);
    };
    walk(this.root);
    return { keys, vals };
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
function build(keys: Uint8Array[], vals: Row[], weights: number[], children: PNode[], cap: number): InsOut {
  const interior = children.length > 0;
  let total = 0;
  for (const w of weights) total += w;
  if (interior) total += 4 * children.length;
  if (total <= cap || keys.length < 3) {
    return { whole: { keys, vals, weights, children, page: 0 } };
  }

  const n = keys.length;
  let best = 1;
  let prefix = 0;
  for (let m = 1; m < n; m++) {
    prefix += weights[m - 1];
    const lp = (interior ? 4 * (m + 1) : 0) + prefix;
    if (lp <= cap) best = m;
  }
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
function nodeInsert(n: PNode, key: Uint8Array, val: Row, weight: number, ctx: InsCtx, cap: number): InsOut {
  const { index, found } = search(n, key);
  if (found) {
    const vals = n.vals.slice();
    const weights = n.weights.slice();
    ctx.old = vals[index];
    ctx.replaced = true;
    vals[index] = val;
    weights[index] = weight;
    return build(n.keys.slice(), vals, weights, n.children.slice(), cap);
  }
  if (isLeaf(n)) {
    return build(insertAt(n.keys, index, key), insertAt(n.vals, index, val), insertAt(n.weights, index, weight), [], cap);
  }
  const child = nodeInsert(n.children[index], key, val, weight, ctx, cap);
  if (child.whole !== null) {
    const children = n.children.slice();
    children[index] = child.whole;
    return { whole: { keys: n.keys.slice(), vals: n.vals.slice(), weights: n.weights.slice(), children, page: 0 } };
  }
  const keys = insertAt(n.keys, index, child.midK);
  const vals = insertAt(n.vals, index, child.midV);
  const weights = insertAt(n.weights, index, child.midW);
  let children = n.children.slice();
  children[index] = child.left;
  children = insertAt(children, index + 1, child.right);
  return build(keys, vals, weights, children, cap);
}

// maxKV is the rightmost (largest) entry of a subtree — its in-order predecessor.
function maxKV(n: PNode): { key: Uint8Array; val: Row; weight: number } {
  while (!isLeaf(n)) n = n.children[n.children.length - 1];
  const i = n.keys.length - 1;
  return { key: n.keys[i], val: n.vals[i], weight: n.weights[i] };
}

type RemOut = { ok: boolean; node: PNode; removed: Row | undefined };

// nodeRemove is the recursive delete (copy-on-write). Returns the rebuilt subtree (possibly
// underfull — the caller rebalances it) and the removed row. A separator found in an interior node
// is replaced by its in-order predecessor (drawn from the left subtree), which is then deleted from
// that subtree; the touched child is rebalanced via rebalanceChild.
function nodeRemove(n: PNode, key: Uint8Array, cap: number): RemOut {
  const { index, found } = search(n, key);
  if (found) {
    if (isLeaf(n)) {
      const removed = n.vals[index];
      return {
        ok: true,
        node: { keys: removeAt(n.keys, index), vals: removeAt(n.vals, index), weights: removeAt(n.weights, index), children: [], page: 0 },
        removed,
      };
    }
    const removed = n.vals[index];
    const pred = maxKV(n.children[index]);
    const child = nodeRemove(n.children[index], pred.key, cap).node;
    const keys = n.keys.slice();
    const vals = n.vals.slice();
    const weights = n.weights.slice();
    const children = n.children.slice();
    keys[index] = pred.key;
    vals[index] = pred.val;
    weights[index] = pred.weight;
    children[index] = child;
    const rebuilt: PNode = { keys, vals, weights, children, page: 0 };
    return { ok: true, node: rebalanceChild(rebuilt, index, cap), removed };
  }
  if (isLeaf(n)) {
    return { ok: false, node: n, removed: undefined };
  }
  const res = nodeRemove(n.children[index], key, cap);
  if (!res.ok) return { ok: false, node: n, removed: undefined };
  const children = n.children.slice();
  children[index] = res.node;
  const rebuilt: PNode = { keys: n.keys.slice(), vals: n.vals.slice(), weights: n.weights.slice(), children, page: 0 };
  return { ok: true, node: rebalanceChild(rebuilt, index, cap), removed: res.removed };
}

// rebalanceChild: if children[i] is underfull (payload < cap/2), merge it with an adjacent sibling
// (prefer the right one), then split the merged node back if it overflows — the unified rebalance
// (no borrow). The returned parent may itself have lost a key and become underfull; its own parent
// handles that as the recursion unwinds.
function rebalanceChild(n: PNode, i: number, cap: number): PNode {
  if (payload(n.children[i]) >= cap / 2) return n;
  const j = i + 1 < n.children.length ? i : i - 1;
  return mergeAt(n, j, cap);
}

// mergeAt merges children[j], separator j, and children[j+1] into one node M. If M fits, it replaces
// the pair and the parent loses separator j and child j+1. If M overflows, it is split 2-way and the
// two halves + the new separator replace the pair (the parent's key count is unchanged). M < 2·cap
// always (format.md), so a single split restores fit.
function mergeAt(n: PNode, j: number, cap: number): PNode {
  const left = n.children[j];
  const right = n.children[j + 1];
  const mkeys = [...left.keys, n.keys[j], ...right.keys];
  const mvals = [...left.vals, n.vals[j], ...right.vals];
  const mweights = [...left.weights, n.weights[j], ...right.weights];
  const mchildren = isLeaf(left) ? [] : [...left.children, ...right.children];

  const keys = n.keys.slice();
  const vals = n.vals.slice();
  const weights = n.weights.slice();
  const children = n.children.slice();

  const out = build(mkeys, mvals, mweights, mchildren, cap);
  if (out.whole !== null) {
    keys.splice(j, 1);
    vals.splice(j, 1);
    weights.splice(j, 1);
    children[j] = out.whole;
    children.splice(j + 1, 1);
    return { keys, vals, weights, children, page: 0 };
  }
  keys[j] = out.midK;
  vals[j] = out.midV;
  weights[j] = out.midW;
  children[j] = out.left;
  children[j + 1] = out.right;
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
