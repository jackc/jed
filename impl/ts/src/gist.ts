// GiST access method — the operation-deterministic R-tree (spec/design/gist.md).
//
// Two opclasses share one tree core (gist.md §2 — the type-specific part is the *only* part that
// differs): range_ops (GX1) over a range column accelerating && and @>, and the scalar `=` opclass
// (GX2, the in-core btree_gist equivalent) over a fixed-width keyable scalar column accelerating =.
// A range_ops bound is the row's exact range (leaf) / covering union (interior) via encodeRangeBody;
// a scalar `=` bound is [min,max] over the ORDER-PRESERVING KEY ENCODING (gist.md §6) — the executor
// encodes a value to its key bytes and the tree only ever COMPARES those bytes (no decode, no
// per-type comparator, no collation; the fixed-width set). This module is the self-contained core —
// the in-memory R-tree (build / penalty / median split), the on-disk node codec (the §4.1 byte
// layout, page types 5/6), and the consistent-descent search.
//
// Determinism (gist.md §3): every operation is a pure function of its inputs, so the identical
// mutation sequence every core replays builds the byte-identical tree. Within a node, entries are
// ordered canonically (boundTotalCmp, ties by storage key / subtree-min key), so a node's bytes are
// a pure function of its entry set; pages are assigned in a canonical post-order walk. This is the
// lockstep port of impl/rust/src/gist.rs (CLAUDE.md §2) — byte-identical by construction.

import { type ColType } from "./catalog.ts";
import { engineError } from "./errors.ts";
import { encodeRangeBody, readRangeBody } from "./format.ts";
import { rangeContains, rangeOverlaps, rangeTotalCmp, rangeUnion } from "./range.ts";
import type { Value } from "./value.ts";

// GIST_FANOUT is the maximum entries per GiST node (gist.md §4.1); the (N+1)-th triggers a median
// picksplit. A pinned cross-core constant.
export const GIST_FANOUT = 4;

// GiST page types (gist.md §4.1, format.md *Page header*).
export const PAGE_GIST_LEAF = 5;
export const PAGE_GIST_INTERIOR = 6;

// The query operator a GiST opclass serves. range_ops accelerates "overlaps" (&&) and "contains"
// (@>); the scalar `=` opclass accelerates "equal" (=).
export type GistStrategy = "overlaps" | "contains" | "equal";

// The operator class — the only type-specific part (gist.md §2). Range is range_ops over a range
// column whose element ColType is elem; Scalar is the `=` opclass over a fixed-width keyable scalar
// (whose bound is opaque key bytes the executor produces — no element type).
export type GistOpclass = { scalar: false; elem: ColType } | { scalar: true };

export const GIST_SCALAR_OPCLASS: GistOpclass = { scalar: true };
export function gistRangeOpclass(elem: ColType): GistOpclass {
  return { scalar: false, elem };
}

type RangeVal = Value & { kind: "range" };

// A bounding key: a range value (range_ops) or a [min,max] pair over the order-preserving key
// encoding (scalar `=`). A leaf's scalar bound is the degenerate [v,v]. Narrowed by `"rng" in b`.
type GistBound = { rng: RangeVal } | { smin: Uint8Array; smax: Uint8Array };

// A search query operand: a range constant (rng) for &&/@>, or a scalar equality constant's
// order-preserving KEY bytes (skey) for =.
export type GistQuery = { rng: RangeVal } | { skey: Uint8Array };

type GistLeafEntry = { bound: GistBound; skey: Uint8Array };
type GistChildEntry = { bound: GistBound; node: GistNode };
type GistNode =
  | { leaf: true; entries: GistLeafEntry[] }
  | { leaf: false; children: GistChildEntry[] };

// GistTree is an operation-deterministic GiST R-tree over a single column (range or scalar opclass).
export type GistTree = { root: GistNode; len: number };

// GistPage is one serialized GiST node page: page number, type (leaf 5 / interior 6), entry count
// (the header item_count), and payload bytes after the 16-byte header.
export type GistPage = {
  pageNo: number;
  pageType: number;
  itemCount: number;
  payload: Uint8Array;
};

export function newGistTree(): GistTree {
  return { root: { leaf: true, entries: [] }, len: 0 };
}

function asRange(v: Value): RangeVal {
  if (v.kind !== "range") throw engineError("data_corrupted", "gist: expected a range value");
  return v;
}

function mustUnion(a: RangeVal, b: RangeVal): RangeVal {
  return asRange(rangeUnion(a, b, false)); // strict=false (the convex hull) never errors
}

function cmpBytes(a: Uint8Array, b: Uint8Array): number {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) {
    if (a[i]! !== b[i]!) return a[i]! < b[i]! ? -1 : 1;
  }
  return a.length - b.length;
}

// encodeBound serializes a bounding key to its self-delimiting bytes (no outer length prefix — the
// node codec adds the bound_len framing; the leaf-store key relies on this being self-delimiting to
// split off the trailing storage key).
function encodeBound(op: GistOpclass, b: GistBound): Uint8Array {
  if (!op.scalar) return encodeRangeBody(op.elem, (b as { rng: RangeVal }).rng);
  const s = b as { smin: Uint8Array; smax: Uint8Array };
  return joinBytes([be16(s.smin.length), s.smin, be16(s.smax.length), s.smax]);
}

// readBound reads one self-delimiting bounding key starting at cur.pos, advancing it past the bound.
function readBound(op: GistOpclass, buf: Uint8Array, cur: { pos: number }): GistBound {
  if (!op.scalar) {
    const v = readRangeBody(op.elem, buf, cur);
    if (v.kind !== "range") throw engineError("data_corrupted", "gist: bound is not a range");
    return { rng: v };
  }
  const mlen = readU16(buf, cur);
  const smin = takeBytes(buf, cur, mlen);
  const xlen = readU16(buf, cur);
  const smax = takeBytes(buf, cur, xlen);
  return { smin, smax };
}

// boundTotalCmp is the canonical total order over bounding keys (gist.md §3): rangeTotalCmp for
// ranges; the [min,max] key bytes lexicographically for scalars (the order-preserving key encoding
// makes raw byte order reproduce value order). Dispatched on the bound kind.
function boundTotalCmp(a: GistBound, b: GistBound): number {
  if ("rng" in a) return rangeTotalCmp(a.rng, (b as { rng: RangeVal }).rng);
  const bs = b as { smin: Uint8Array; smax: Uint8Array };
  const c = cmpBytes(a.smin, bs.smin);
  return c !== 0 ? c : cmpBytes(a.smax, bs.smax);
}

// boundUnion is the covering union of two bounding keys — the convex-hull merge for ranges; the
// componentwise [min(min), max(max)] (byte-wise, the order-preserving key order) for scalars.
function boundUnion(a: GistBound, b: GistBound): GistBound {
  if ("rng" in a) return { rng: mustUnion(a.rng, (b as { rng: RangeVal }).rng) };
  const bs = b as { smin: Uint8Array; smax: Uint8Array };
  return {
    smin: cmpBytes(bs.smin, a.smin) < 0 ? bs.smin : a.smin,
    smax: cmpBytes(bs.smax, a.smax) > 0 ? bs.smax : a.smax,
  };
}

// gistInsert one row's (bounding key, storage key) into the tree under op.
export function gistInsert(
  tree: GistTree,
  op: GistOpclass,
  bound: GistBound,
  skey: Uint8Array,
): void {
  const sib = insertNode(tree.root, op, bound, skey);
  if (sib !== null) {
    // The root split: grow a new interior root over the old root (left) + the sibling.
    const left = tree.root;
    const children: GistChildEntry[] = [{ bound: nodeUnion(left), node: left }, sib];
    sortChildren(children);
    tree.root = { leaf: false, children };
  }
  tree.len++;
}

// gistSearch is the consistent-descent search: every storage key whose row satisfies the query under
// strat. The interior descend predicate is conservative (no false negatives); the exact operator is
// applied at the leaf. Returns { keys, nodes, interior } — nodes (interior + leaf) is the page_read
// charge, interior the gist_descent charge (spec/design/gist.md §9).
export function gistSearch(
  tree: GistTree,
  query: GistQuery,
  strat: GistStrategy,
): { keys: Uint8Array[]; nodes: number; interior: number } {
  const out: Uint8Array[] = [];
  const counts = { nodes: 0, interior: 0 };
  searchNode(tree.root, query, strat, out, counts);
  return { keys: out, nodes: counts.nodes, interior: counts.interior };
}

// chooseChild picks the child to descend on insert: the one whose union, merged with the new entry,
// has the lexicographically-smallest serialized bound bytes; ties keep the lower slot (penalty).
function chooseChild(children: GistChildEntry[], op: GistOpclass, bound: GistBound): number {
  let best = 0;
  let bestKey: Uint8Array | null = null;
  for (let i = 0; i < children.length; i++) {
    const key = encodeBound(op, boundUnion(children[i]!.bound, bound));
    if (bestKey === null || cmpBytes(key, bestKey) < 0) {
      best = i;
      bestKey = key;
    }
  }
  return best;
}

// insertNode inserts into node, returning a new right-sibling child when the node split.
function insertNode(
  node: GistNode,
  op: GistOpclass,
  bound: GistBound,
  skey: Uint8Array,
): GistChildEntry | null {
  if (node.leaf) {
    node.entries.push({ bound, skey });
    sortLeaf(node.entries);
  } else {
    const i = chooseChild(node.children, op, bound);
    const sib = insertNode(node.children[i]!.node, op, bound, skey);
    // The chosen child's union may have shrunk (after a split below) or grown; recompute it.
    node.children[i]!.bound = nodeUnion(node.children[i]!.node);
    if (sib !== null) node.children.push(sib);
    sortChildren(node.children);
  }
  return splitIfOverflow(node);
}

// splitIfOverflow splits an over-fan-out node at the median (entries already canonical) and returns
// the new right sibling; otherwise null.
function splitIfOverflow(node: GistNode): GistChildEntry | null {
  if (node.leaf) {
    if (node.entries.length <= GIST_FANOUT) return null;
    const mid = Math.ceil(node.entries.length / 2);
    const right: GistNode = { leaf: true, entries: node.entries.splice(mid) };
    return { bound: nodeUnion(right), node: right };
  }
  if (node.children.length <= GIST_FANOUT) return null;
  const mid = Math.ceil(node.children.length / 2);
  const right: GistNode = { leaf: false, children: node.children.splice(mid) };
  return { bound: nodeUnion(right), node: right };
}

// nodeUnion is the covering union of a node's entries (the convex-hull merge — never errors). The
// node must be non-empty (the empty tree's root leaf is never unioned).
function nodeUnion(node: GistNode): GistBound {
  if (node.leaf) {
    let u = node.entries[0]!.bound;
    for (let i = 1; i < node.entries.length; i++) u = boundUnion(u, node.entries[i]!.bound);
    return u;
  }
  let u = node.children[0]!.bound;
  for (let i = 1; i < node.children.length; i++) u = boundUnion(u, node.children[i]!.bound);
  return u;
}

// subtreeMinSkey is the smallest storage key anywhere in the subtree — a deterministic,
// sibling-unique tiebreak for canonical interior ordering.
function subtreeMinSkey(node: GistNode): Uint8Array {
  if (node.leaf) {
    let min = node.entries[0]!.skey;
    for (let i = 1; i < node.entries.length; i++) {
      if (cmpBytes(node.entries[i]!.skey, min) < 0) min = node.entries[i]!.skey;
    }
    return min;
  }
  let min = subtreeMinSkey(node.children[0]!.node);
  for (let i = 1; i < node.children.length; i++) {
    const s = subtreeMinSkey(node.children[i]!.node);
    if (cmpBytes(s, min) < 0) min = s;
  }
  return min;
}

function sortLeaf(entries: GistLeafEntry[]): void {
  entries.sort((a, b) => {
    const c = boundTotalCmp(a.bound, b.bound);
    return c !== 0 ? c : cmpBytes(a.skey, b.skey);
  });
}

function sortChildren(children: GistChildEntry[]): void {
  // Recompute the subtree-min tiebreak inside the comparator (fan-out is tiny) so it tracks the live
  // element under sort's swaps.
  children.sort((a, b) => {
    const c = boundTotalCmp(a.bound, b.bound);
    return c !== 0 ? c : cmpBytes(subtreeMinSkey(a.node), subtreeMinSkey(b.node));
  });
}

// The conservative interior descend predicate (gist.md §5/§6). For && and @>, a matching row must
// overlap the query, and every row is contained in its subtree's union, so a non-overlapping union
// holds no match — overlaps prunes safely. For =, a matching value must lie within the subtree's
// [min,max] key interval, so a query key outside it prunes safely.
function descendPred(union: GistBound, query: GistQuery, strat: GistStrategy): boolean {
  if (strat === "equal") {
    const u = union as { smin: Uint8Array; smax: Uint8Array };
    const q = (query as { skey: Uint8Array }).skey;
    return cmpBytes(u.smin, q) <= 0 && cmpBytes(q, u.smax) <= 0;
  }
  return rangeOverlaps((union as { rng: RangeVal }).rng, (query as { rng: RangeVal }).rng);
}

// leafMatches is the exact operator, applied at the leaf to keep only true matches. A leaf's scalar
// bound is the degenerate [v,v], so equality is min == query key.
function leafMatches(bound: GistBound, query: GistQuery, strat: GistStrategy): boolean {
  if (strat === "equal") {
    return (
      cmpBytes((bound as { smin: Uint8Array }).smin, (query as { skey: Uint8Array }).skey) === 0
    );
  }
  const r = (bound as { rng: RangeVal }).rng;
  const q = (query as { rng: RangeVal }).rng;
  return strat === "overlaps" ? rangeOverlaps(r, q) : rangeContains(r, q);
}

function searchNode(
  node: GistNode,
  query: GistQuery,
  strat: GistStrategy,
  out: Uint8Array[],
  counts: { nodes: number; interior: number },
): void {
  counts.nodes++;
  if (node.leaf) {
    for (const e of node.entries) {
      if (leafMatches(e.bound, query, strat)) out.push(e.skey);
    }
    return;
  }
  counts.interior++;
  for (const c of node.children) {
    if (descendPred(c.bound, query, strat)) searchNode(c.node, query, strat, out, counts);
  }
}

// ---- on-disk node codec (gist.md §4.1) -------------------------------------------------------

function be16(n: number): Uint8Array {
  return Uint8Array.of((n >>> 8) & 0xff, n & 0xff);
}
function be32(n: number): Uint8Array {
  return Uint8Array.of((n >>> 24) & 0xff, (n >>> 16) & 0xff, (n >>> 8) & 0xff, n & 0xff);
}
function joinBytes(parts: Uint8Array[]): Uint8Array {
  let len = 0;
  for (const p of parts) len += p.length;
  const out = new Uint8Array(len);
  let off = 0;
  for (const p of parts) {
    out.set(p, off);
    off += p.length;
  }
  return out;
}

// serializeGistTree serializes the whole tree to its node pages in canonical post-order (children
// before parent, the root last). alloc hands out the next page number. Returns the pages (each with
// its allocated number) and the root page.
export function serializeGistTree(
  tree: GistTree,
  op: GistOpclass,
  alloc: () => number,
): { pages: GistPage[]; root: number } {
  const pages: GistPage[] = [];
  const root = serializeNode(tree.root, op, pages, alloc);
  return { pages, root };
}

function serializeNode(
  node: GistNode,
  op: GistOpclass,
  pages: GistPage[],
  alloc: () => number,
): number {
  if (node.leaf) {
    const parts: Uint8Array[] = [];
    for (const e of node.entries) {
      const b = encodeBound(op, e.bound);
      parts.push(be16(b.length), b, be16(e.skey.length), e.skey);
    }
    const pageNo = alloc();
    pages.push({
      pageNo,
      pageType: PAGE_GIST_LEAF,
      itemCount: node.entries.length,
      payload: joinBytes(parts),
    });
    return pageNo;
  }
  // Children first (post-order), in the node's canonical entry order.
  const childPages = node.children.map((c) => serializeNode(c.node, op, pages, alloc));
  const parts: Uint8Array[] = [];
  for (let i = 0; i < node.children.length; i++) {
    const b = encodeBound(op, node.children[i]!.bound);
    parts.push(be16(b.length), b, be32(childPages[i]!));
  }
  const pageNo = alloc();
  pages.push({
    pageNo,
    pageType: PAGE_GIST_INTERIOR,
    itemCount: node.children.length,
    payload: joinBytes(parts),
  });
  return pageNo;
}

// ---- the leaf-key codec + canonical-order build (the executor/serializer API) -----------------

// rangeGistLeafKey builds a range_ops leaf-store key for one row (the GIN term ‖ skey pattern): the
// row range's self-delimiting encodeRangeBody bytes then its storage key.
export function rangeGistLeafKey(elem: ColType, bound: RangeVal, skey: Uint8Array): Uint8Array {
  return encodeGistLeafKey(gistRangeOpclass(elem), { rng: bound }, skey);
}

// scalarGistLeafKey builds a scalar `=` leaf-store key for one row: the value's order-preserving KEY
// bytes as the degenerate [v,v] bound, then its storage key. valueKey is encodeKeyValue of the row's
// scalar value — the executor computes it (gist.ts never encodes a value, only compares bytes).
export function scalarGistLeafKey(valueKey: Uint8Array, skey: Uint8Array): Uint8Array {
  return encodeGistLeafKey(GIST_SCALAR_OPCLASS, { smin: valueKey, smax: valueKey }, skey);
}

// encodeGistLeafKey is the leaf-store key = the bound's self-delimiting bytes ‖ the storage key.
function encodeGistLeafKey(op: GistOpclass, bound: GistBound, skey: Uint8Array): Uint8Array {
  return joinBytes([encodeBound(op, bound), skey]);
}

function decodeGistLeafKey(
  op: GistOpclass,
  key: Uint8Array,
): { bound: GistBound; skey: Uint8Array } {
  const cur = { pos: 0 };
  const bound = readBound(op, key, cur);
  return { bound, skey: key.subarray(cur.pos) };
}

// buildGistFromLeafKeys builds the persisted R-tree from the index store's leaf keys. The keys are
// decoded and inserted in CANONICAL order (boundTotalCmp, ties by storage key), so the tree is a pure
// function of the leaf SET — content-deterministic, independent of the original mutation order
// (gist.md §3); the cross-core / golden round-trip property the build relies on.
export function buildGistFromLeafKeys(op: GistOpclass, keys: Uint8Array[]): GistTree {
  const entries = keys.map((k) => decodeGistLeafKey(op, k));
  entries.sort((a, b) => {
    const c = boundTotalCmp(a.bound, b.bound);
    return c !== 0 ? c : cmpBytes(a.skey, b.skey);
  });
  const tree = newGistTree();
  for (const e of entries) gistInsert(tree, op, e.bound, e.skey);
  return tree;
}

// readGistLeafKeys walks a persisted GiST R-tree (rooted at root, page types 5/6), marking every node
// page in reached (so the free-list keeps the live tree) and collecting each leaf's leaf key (bound ‖
// skey — the opclass's self-delimiting bound bytes concatenated with the storage key). OPCLASS-
// AGNOSTIC: the bound bytes are copied verbatim (range body or [min,max] key blob), so no element type
// is needed. read returns one page's { pageType, itemCount, payload }.
export function readGistLeafKeys(
  read: (pageNo: number) => { pageType: number; itemCount: number; payload: Uint8Array },
  pageNo: number,
  reached: Set<number>,
  out: Uint8Array[],
): void {
  reached.add(pageNo);
  const { pageType, itemCount, payload } = read(pageNo);
  const cur = { pos: 0 };
  if (pageType === PAGE_GIST_LEAF) {
    for (let i = 0; i < itemCount; i++) {
      const blen = readU16(payload, cur);
      const bound = takeBytes(payload, cur, blen);
      const slen = readU16(payload, cur);
      const skey = takeBytes(payload, cur, slen);
      out.push(joinBytes([bound, skey]));
    }
    return;
  }
  if (pageType === PAGE_GIST_INTERIOR) {
    const children: number[] = [];
    for (let i = 0; i < itemCount; i++) {
      const blen = readU16(payload, cur);
      takeBytes(payload, cur, blen); // skip the union bound
      children.push(readU32(payload, cur));
    }
    for (const cp of children) readGistLeafKeys(read, cp, reached, out);
    return;
  }
  throw engineError("data_corrupted", "expected a GiST node page");
}

function readU16(buf: Uint8Array, cur: { pos: number }): number {
  if (cur.pos + 2 > buf.length) throw engineError("data_corrupted", "gist: truncated u16");
  const v = (buf[cur.pos]! << 8) | buf[cur.pos + 1]!;
  cur.pos += 2;
  return v;
}
function readU32(buf: Uint8Array, cur: { pos: number }): number {
  if (cur.pos + 4 > buf.length) throw engineError("data_corrupted", "gist: truncated u32");
  const v =
    ((buf[cur.pos]! << 24) |
      (buf[cur.pos + 1]! << 16) |
      (buf[cur.pos + 2]! << 8) |
      buf[cur.pos + 3]!) >>>
    0;
  cur.pos += 4;
  return v;
}
function takeBytes(buf: Uint8Array, cur: { pos: number }, n: number): Uint8Array {
  if (cur.pos + n > buf.length) throw engineError("data_corrupted", "gist: truncated bytes");
  const v = buf.subarray(cur.pos, cur.pos + n);
  cur.pos += n;
  return v;
}
