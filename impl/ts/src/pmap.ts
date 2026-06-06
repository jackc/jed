// Persistent (copy-on-write) ordered map — the in-memory store primitive (decision B1,
// spec/design/transactions.md §3).
//
// Keyed by the encoded key bytes (compared lexicographically = memcmp = the order-preserving
// key encoding's contract, spec/design/encoding.md). Every mutation returns a new map that
// shares structure with the old one; nodes are immutable by convention (never mutated after
// construction), so `clone()` (which shares the root) is an O(1) independent snapshot — mutating
// the clone leaves the original untouched. That cheap, structurally-shared snapshot carries the
// §3 staging-buffer / transaction model (transactions.md §2). The concrete shape is a
// copy-on-write B-tree: the in-memory precursor of the Phase-6 on-disk B-tree.
//
// Only the iteration order is a cross-core contract this slice; the in-RAM node shape (fan-out,
// split points) is private (transactions.md §3). Delete rebalances (Cormen's algorithm) so
// leaves stay non-empty.

import type { Row } from "./storage.ts";

// Minimum degree t: a node holds between t-1 and 2t-1 keys (the root may hold fewer) and
// overflows at 2t. Private tuning — it changes only the in-RAM shape, never order.
const T = 16;
const MAX_KEYS = 2 * T - 1;
const MIN_KEYS = T - 1;

// One B-tree node. `children` is empty for a leaf; otherwise children.length === keys.length+1.
// keys.length === vals.length always. Nodes are never mutated after construction.
type PNode = {
  keys: Uint8Array[];
  vals: Row[];
  children: PNode[];
};

function isLeaf(n: PNode): boolean {
  return n.children.length === 0;
}

function compareBytes(a: Uint8Array, b: Uint8Array): number {
  const n = a.length < b.length ? a.length : b.length;
  for (let i = 0; i < n; i++) {
    if (a[i] !== b[i]) return a[i] < b[i] ? -1 : 1;
  }
  return a.length === b.length ? 0 : a.length < b.length ? -1 : 1;
}

// search returns the index and whether key is present: found ⇒ keys[index] === key, else index
// is the child/insertion slot.
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

  // insert inserts or overwrites key. Returns the previous row if key was present (an
  // overwrite, leaving the size unchanged), else undefined (a new insert, which grows the size).
  insert(key: Uint8Array, val: Row): Row | undefined {
    if (this.root === null) {
      this.root = { keys: [key], vals: [val], children: [] };
      this.length++;
      return undefined;
    }
    const ctx: InsCtx = { old: undefined, replaced: false };
    const out = nodeInsert(this.root, key, val, ctx);
    this.root =
      out.whole !== null
        ? out.whole
        : { keys: [out.midK], vals: [out.midV], children: [out.left, out.right] };
    if (!ctx.replaced) this.length++;
    return ctx.old;
  }

  // remove deletes key. Returns the removed row, or undefined if absent (then the map is
  // unchanged).
  remove(key: Uint8Array): Row | undefined {
    if (this.root === null) return undefined;
    const res = nodeRemove(this.root, key);
    if (!res.ok) return undefined;
    const newRoot = res.node;
    // The root may have drained to zero keys: an empty leaf becomes the empty map; an empty
    // internal node (one child) hands the root down a level (height shrinks).
    if (newRoot.keys.length === 0) {
      this.root = isLeaf(newRoot) ? null : newRoot.children[0];
    } else {
      this.root = newRoot;
    }
    this.length--;
    return res.removed;
  }

  // inorder returns all (key, row) pairs in ascending key order. Eager (the cost contract
  // charges per row in the executor loop, not here — spec/design/cost.md), so laziness is
  // unobservable.
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

type InsCtx = { old: Row | undefined; replaced: boolean };
// The result of inserting into a subtree: a whole rebuilt node, or a split.
type InsOut =
  | { whole: PNode }
  | { whole: null; left: PNode; midK: Uint8Array; midV: Row; right: PNode };

// nodeInsert is the recursive insert. On overwrite it sets ctx and rebuilds the path with the
// value replaced (no split). On a new key it inserts into the leaf and splits overflowing nodes
// back up the path.
function nodeInsert(n: PNode, key: Uint8Array, val: Row, ctx: InsCtx): InsOut {
  const { index, found } = search(n, key);
  if (found) {
    const vals = n.vals.slice();
    ctx.old = vals[index];
    ctx.replaced = true;
    vals[index] = val;
    return { whole: { keys: n.keys.slice(), vals, children: n.children.slice() } };
  }
  if (isLeaf(n)) {
    return splitIfNeeded(insertAt(n.keys, index, key), insertAt(n.vals, index, val), []);
  }
  const child = nodeInsert(n.children[index], key, val, ctx);
  if (child.whole !== null) {
    const children = n.children.slice();
    children[index] = child.whole;
    return { whole: { keys: n.keys.slice(), vals: n.vals.slice(), children } };
  }
  const keys = insertAt(n.keys, index, child.midK);
  const vals = insertAt(n.vals, index, child.midV);
  let children = n.children.slice();
  children[index] = child.left;
  children = insertAt(children, index + 1, child.right);
  return splitIfNeeded(keys, vals, children);
}

// splitIfNeeded builds a node from the parts; if it overflows (> 2t-1 keys) it splits at the
// midpoint and promotes the median. children empty ⇒ leaf. The split point is deterministic and
// (being in-RAM only) free to choose (transactions.md §3).
function splitIfNeeded(keys: Uint8Array[], vals: Row[], children: PNode[]): InsOut {
  if (keys.length <= MAX_KEYS) {
    return { whole: { keys, vals, children } };
  }
  const mid = keys.length >> 1;
  const leaf = children.length === 0;
  return {
    whole: null,
    left: {
      keys: keys.slice(0, mid),
      vals: vals.slice(0, mid),
      children: leaf ? [] : children.slice(0, mid + 1),
    },
    midK: keys[mid],
    midV: vals[mid],
    right: {
      keys: keys.slice(mid + 1),
      vals: vals.slice(mid + 1),
      children: leaf ? [] : children.slice(mid + 1),
    },
  };
}

function canSpare(n: PNode): boolean {
  return n.keys.length > MIN_KEYS;
}

// minKV is the leftmost (smallest) entry of a subtree — its in-order successor.
function minKV(n: PNode): { key: Uint8Array; val: Row } {
  while (!isLeaf(n)) n = n.children[0];
  return { key: n.keys[0], val: n.vals[0] };
}

// maxKV is the rightmost (largest) entry of a subtree — its in-order predecessor.
function maxKV(n: PNode): { key: Uint8Array; val: Row } {
  while (!isLeaf(n)) n = n.children[n.children.length - 1];
  return { key: n.keys[n.keys.length - 1], val: n.vals[n.vals.length - 1] };
}

type RemOut = { ok: boolean; node: PNode; removed: Row | undefined };

// nodeRemove is the recursive delete (Cormen's B-tree deletion, copy-on-write). It maintains the
// invariant that any node it descends into holds at least t keys, so a delete cannot underflow
// it — a key in an internal node is replaced by a predecessor/successor drawn from a child that
// can spare one (else the two children and the separator are merged first). That rebalancing
// keeps every leaf non-empty, so minKV/maxKV are always well-defined.
function nodeRemove(n: PNode, key: Uint8Array): RemOut {
  const { index, found } = search(n, key);
  if (found) {
    if (isLeaf(n)) {
      const removed = n.vals[index];
      return { ok: true, node: { keys: removeAt(n.keys, index), vals: removeAt(n.vals, index), children: [] }, removed };
    }
    const removed = n.vals[index];
    if (canSpare(n.children[index])) {
      const pred = maxKV(n.children[index]);
      const child = nodeRemove(n.children[index], pred.key).node;
      const keys = n.keys.slice();
      const vals = n.vals.slice();
      const children = n.children.slice();
      keys[index] = pred.key;
      vals[index] = pred.val;
      children[index] = child;
      return { ok: true, node: { keys, vals, children }, removed };
    }
    if (canSpare(n.children[index + 1])) {
      const succ = minKV(n.children[index + 1]);
      const child = nodeRemove(n.children[index + 1], succ.key).node;
      const keys = n.keys.slice();
      const vals = n.vals.slice();
      const children = n.children.slice();
      keys[index] = succ.key;
      vals[index] = succ.val;
      children[index + 1] = child;
      return { ok: true, node: { keys, vals, children }, removed };
    }
    const merged = mergeAt(n, index);
    const res = finishDescend(merged, index, key);
    return { ok: true, node: res.node, removed };
  }
  if (isLeaf(n)) {
    return { ok: false, node: n, removed: undefined };
  }
  return descendRemove(n, index, key);
}

// descendRemove descends into child i to delete key, first ensuring that child holds at least t
// keys — borrow from a sibling that can spare it, else merge with a sibling.
function descendRemove(n: PNode, i: number, key: Uint8Array): RemOut {
  if (n.children[i].keys.length >= T) {
    return finishDescend(n, i, key);
  }
  if (i > 0 && canSpare(n.children[i - 1])) {
    return finishDescend(borrowFromLeft(n, i), i, key);
  }
  if (i + 1 < n.children.length && canSpare(n.children[i + 1])) {
    return finishDescend(borrowFromRight(n, i), i, key);
  }
  if (i > 0) {
    return finishDescend(mergeAt(n, i - 1), i - 1, key);
  }
  return finishDescend(mergeAt(n, i), i, key);
}

// finishDescend recurses into child i (now guaranteed >= t keys) and splices the result back in.
function finishDescend(n: PNode, i: number, key: Uint8Array): RemOut {
  const res = nodeRemove(n.children[i], key);
  if (!res.ok) return { ok: false, node: n, removed: undefined };
  const children = n.children.slice();
  children[i] = res.node;
  return { ok: true, node: { keys: n.keys.slice(), vals: n.vals.slice(), children }, removed: res.removed };
}

// borrowFromLeft: child i borrows a key from its left sibling, rotating through separator i-1.
function borrowFromLeft(n: PNode, i: number): PNode {
  const left = n.children[i - 1];
  const cur = n.children[i];

  const upKey = left.keys[left.keys.length - 1];
  const upVal = left.vals[left.vals.length - 1];
  const newLeft: PNode = {
    keys: left.keys.slice(0, -1),
    vals: left.vals.slice(0, -1),
    children: isLeaf(left) ? [] : left.children.slice(0, -1),
  };
  const newCur: PNode = {
    keys: insertAt(cur.keys, 0, n.keys[i - 1]),
    vals: insertAt(cur.vals, 0, n.vals[i - 1]),
    children: isLeaf(left) ? [] : insertAt(cur.children, 0, left.children[left.children.length - 1]),
  };

  const keys = n.keys.slice();
  const vals = n.vals.slice();
  const children = n.children.slice();
  keys[i - 1] = upKey;
  vals[i - 1] = upVal;
  children[i - 1] = newLeft;
  children[i] = newCur;
  return { keys, vals, children };
}

// borrowFromRight: child i borrows a key from its right sibling, rotating through separator i.
function borrowFromRight(n: PNode, i: number): PNode {
  const cur = n.children[i];
  const right = n.children[i + 1];

  const upKey = right.keys[0];
  const upVal = right.vals[0];
  const newRight: PNode = {
    keys: right.keys.slice(1),
    vals: right.vals.slice(1),
    children: isLeaf(right) ? [] : right.children.slice(1),
  };
  const newCur: PNode = {
    keys: insertAt(cur.keys, cur.keys.length, n.keys[i]),
    vals: insertAt(cur.vals, cur.vals.length, n.vals[i]),
    children: isLeaf(right) ? [] : insertAt(cur.children, cur.children.length, right.children[0]),
  };

  const keys = n.keys.slice();
  const vals = n.vals.slice();
  const children = n.children.slice();
  keys[i] = upKey;
  vals[i] = upVal;
  children[i] = newCur;
  children[i + 1] = newRight;
  return { keys, vals, children };
}

// mergeAt merges children[i], separator i, and children[i+1] into one node (2t-1 keys), and
// removes the separator and the absorbed right child from this node.
function mergeAt(n: PNode, i: number): PNode {
  const left = n.children[i];
  const right = n.children[i + 1];
  const merged: PNode = {
    keys: [...left.keys, n.keys[i], ...right.keys],
    vals: [...left.vals, n.vals[i], ...right.vals],
    children: isLeaf(left) ? [] : [...left.children, ...right.children],
  };
  const children = n.children.slice();
  children[i] = merged;
  children.splice(i + 1, 1);
  return { keys: removeAt(n.keys, i), vals: removeAt(n.vals, i), children };
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
