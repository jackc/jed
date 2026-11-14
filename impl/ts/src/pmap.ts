// Persistent (copy-on-write) ordered map — the page-backed B+tree (decision B1,
// spec/design/bplus-reshape.md; spec/design/transactions.md §3; spec/fileformat/format.md "The
// per-table data B+tree").
//
// Keyed by the encoded key bytes (compared lexicographically = memcmp = the order-preserving
// key encoding's contract, spec/design/encoding.md). Every mutation preserves prior roots by
// path-copying shared/published nodes. `clone()` is the explicit O(1) alias boundary; it invalidates
// the private mutation-generation token that lets later INSERTs reuse an unaliased dirty suffix
// (transactions.md §3).
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

import { type Cap, capPtr } from "./cap.ts";
import type { ColType } from "./catalog.ts";
import type { CompressMemo } from "./format.ts";
import type { Row } from "./storage.ts";
import type { Value } from "./value.ts";
import { leafPrefixSaving } from "./leaf_compact.ts";
import {
  COOPERATE,
  ioSteps,
  runSync,
  shouldCooperate,
  syncStepIterator,
  type StepIterator,
  type Steps,
} from "./execution.ts";

// TypeScript has no object-uniqueness query, so a private object identity is the ownership proof for
// dirty INSERT transients (transactions.md §3). The module-private symbol makes the node stamp
// unforgeable outside this module and keeps it out of structuredClone/serialization. It is runtime
// state only: never rendered, hashed, persisted, or metered.
type MutationGeneration = { active: boolean };
const mutationOwner: unique symbol = Symbol("jed.pmap.mutationOwner");

function newMutationGeneration(): MutationGeneration {
  return { active: true };
}

// PackedLeaf is the block-backed resident form of a demand-paged clean leaf (packed-leaf.md §5): the
// page block + the parsed PAX directories, reconstructing rows on demand instead of storing a decoded
// row vector. Its methods close over the format-layer value codec (readValueLazy over a
// Uint8Array.subarray of the page block, GC-pinned), so pmap calls them through this interface rather
// than importing the format layer — avoiding a format↔pmap import cycle (like leafOverhead
// below). Built by format.ts decodeLeafNode.
export interface PackedLeaf {
  readonly n: number;
  // The column types the leaf was decoded (and its directories validated) against.
  readonly colTypes: ColType[];
  // Borrow key i directly from the retained page block and derive its record weight lazily.
  key(i: number): Uint8Array;
  weight(i: number): number;
  // Reconstruct the whole value row i — uniformly LAZY (bplus-reshape.md B4): fixed-width columns
  // decode eagerly, variable columns defer as self-resolving unfetched values.
  row(i: number): Row;
  // Reconstruct only column c of row i — the touched-column path (the A2/A3 columnar gather).
  col(i: number, c: number): Value;
  // Column c of row i RESOLVED, for a reader that uses the value now (packed-leaf.md §11 Track A5):
  // an inline body is decoded straight from the page rather than deferred and then resolved.
  // undefined for an out-of-line or compressed value, whose resolve may need a page read (the
  // caller takes the stepping path).
  colNow(i: number, c: number): Value | undefined;
  // The retained page payload (packed-leaf.md §11 Track A4).
  readonly payload: Uint8Array;
  // The payload's content length: the leaf's serialized size, content-dependent savings included
  // (format.md "Leaf payload").
  readonly payloadLen: number;
  // search binary-searches the leaf's keys without rebuilding them — set on a leaf that stores a
  // shared key prefix (format.md "Leaf node"), whose full keys are otherwise rebuilt on first use.
  search?(key: Uint8Array): { index: number; found: boolean };
  // Whether column c of row i is NULL — the null bitmap / zero-length span, no value decode.
  isNull(i: number, c: number): boolean;
  // The stored bytes of a non-NULL column c of row i as out.buf[out.start, out.end): a variable
  // column's tagged bytes, a fixed-width column's untagged slot. out.buf is the page payload, or the
  // buffer a region the page stores in another form was rebuilt into (an elided key column, a
  // base-plus-offsets or packed-array region — format.md "Leaf node"). False for NULL.
  span(i: number, c: number, out: Span): boolean;
  // recordWeights is weight(i) for every record, read column by column from the directories.
  recordWeights(): number[];
  // inlineOnly reports whether every variable-width value of the page is NULL or stored in the
  // inline form a writer gives an all-inline record (format.ts paxInlineOnly). Cached.
  inlineOnly(): boolean;
  // hasChains reports whether any value of the page is stored in an overflow chain.
  hasChains(): boolean;
  // dirs is the format layer's parsed directories of the page (opaque here; format.ts PaxDirs).
  readonly dirs: unknown;
}

// A stored value's bytes: buf[start, end).
export type Span = { buf: Uint8Array; start: number; end: number };

// One B+tree node. `children` is empty for a leaf; otherwise children.length === keys.length+1 and
// the node is a record-free interior (v24): its keys are the routing separators and its
// vals/weights are EMPTY. A decoded leaf has keys.length === vals.length === weights.length; a
// Packed leaf keeps all three in its retained page block and leaves those arrays empty.
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
  // Decoded node (in-memory/loaded leaves, any dirty leaf — a mutation copies a Packed leaf into a
  // dirty one, lazily (lazyBase) or decoded, §7 — and every interior node). A Packed leaf is always
  // clean (page != 0), so it is never serialized.
  packed?: PackedLeaf;
  page: number;
  // tag is the AEAD tag of the page at `page` in an encrypted file (spec/fileformat/encrypted.md §2):
  // set with the page id by the commit that seals the node, so a parent written later binds it.
  // Absent in a plaintext file and while the node is dirty.
  tag?: Uint8Array;
  // sizes caches a dirty leaf's content-dependent size facts (LeafSizer) for its n rows, so an
  // insert, delete or row replacement measures one row instead of the leaf. Kept only on a node its
  // writer owns; a lazy leaf's are read from its page (LeafSizer.measureLazy).
  sizes?: { n: number; facts: unknown };
  // Lazy dirty leaf (setLeafLazy): a dirty leaf copied from a Packed leaf keeps the rows it has not
  // replaced undecoded. lazyBase is that leaf; lazy is parallel to vals: lazy[i] >= 0 says row i is
  // still row lazy[i] of lazyBase and vals[i] is NO_ROW, -1 that vals[i] holds it. Its keys are views
  // of the page (or of the keys the Packed leaf rebuilt from its prefix) until a split or merge
  // copies them. lazyBase undefined: every row is in vals.
  lazyBase?: PackedLeaf;
  lazy?: number[];
  // wsum is the sum of weights once the writer starts editing the leaf in place, kept by those edits.
  wsum?: number;
  // sameRows says the leaf's keys are exactly lazyBase's, in order: rows were only replaced. Then
  // colDirty[c] says some replaced row's stored value in column c differs from the page's.
  // patchOK says every record of the page and every row written since is stored all-inline.
  sameRows?: boolean;
  patchOK?: boolean;
  colDirty?: boolean[];
  // Deferral (spec/design/deferred-interior.md §9.4): origin is the persisted page a dirty interior
  // replaces one for one in its parent's slot (absent: none -- a split or merge product, or a leaf,
  // whose origin no decision reads), and sameKeys says its separators are still that page's, so only
  // its child pointers differ. Inherited through every copy; any separator edit clears sameKeys.
  // Mirrors the Go core's origin / sameKeys site for site, because the commit's retention decision --
  // and so its bytes -- depends on them.
  origin?: { page: number; tag?: Uint8Array };
  sameKeys?: boolean;
  [mutationOwner]?: MutationGeneration;
};

// originOf is the persisted page a copy of n replaces, and whether the copy's separators are still
// that page's: n's own page when it is persisted, else what n inherited (Go's originOf).
export function originOf(n: PNode): { origin?: { page: number; tag?: Uint8Array }; same: boolean } {
  if (n.page !== 0) return { origin: { page: n.page, tag: n.tag }, same: true };
  return { origin: n.origin, same: n.sameKeys === true };
}

// copyInterior is a dirty interior copy of from with these separators and children, inheriting its
// origin; sameKeys is kept only when the separators are from's unchanged.
function copyInterior(
  from: PNode,
  keys: Uint8Array[],
  children: Child[],
  keysUnchanged: boolean,
  owner?: MutationGeneration,
): PNode {
  const out = newInterior(keys, children, owner);
  const { origin, same } = originOf(from);
  if (origin !== undefined) out.origin = origin;
  out.sameKeys = same && keysUnchanged;
  return out;
}

// NO_ROW stands in vals for a row a lazy leaf still reads from its page.
const NO_ROW: Row = Object.freeze([]) as unknown as Row;

// leafLazy turns the lazy dirty-leaf paths on: a leaf edited in place of a Packed leaf keeps its
// untouched rows undecoded, sizes itself from its page, and is encoded as a patch of the page
// (format.ts encodeLazyLeaf). File bytes, results and costs are the same either way.
let leafLazy = true;

// setLeafLazy turns the lazy dirty-leaf paths on or off (tests run a workload under each).
export function setLeafLazy(on: boolean): void {
  leafLazy = on;
}

export function leafLazyOn(): boolean {
  return leafLazy;
}

// storedLeaf returns the Packed leaf holding row i's stored bytes — the node's own page, or a lazy
// node's base page for a row it has not replaced — and storedIndex the row's index there. undefined
// when vals[i] holds the row.
export function storedLeaf(n: PNode, i: number): PackedLeaf | undefined {
  if (n.packed) return n.packed;
  if (n.lazyBase !== undefined && n.lazy![i]! >= 0) return n.lazyBase;
  return undefined;
}

export function storedIndex(n: PNode, i: number): number {
  return n.packed ? i : n.lazy![i]!;
}

// rowAt reconstructs value row i — the value-read seam (packed-leaf.md §4), on a LEAF. A Decoded
// leaf returns the stored row (read-only by convention); a Packed leaf reconstructs it from the
// retained PAX directories. Throws on a corrupt touched inline body (XX001). The old two-form
// masked/unmasked reconstruction seam (rowAtMasked / rowAtMaybeMasked) is COLLAPSED
// (bplus-reshape.md B4): a Packed leaf's reconstruction is uniformly lazy (fixed-width columns
// decode eagerly, variable columns defer as self-resolving unfetched values), so a reconstruction
// mask no longer exists — the query's touched set survives as the cost basis + the scan layer's
// resolve prefetch, and a missed value resolves on touch (the demand-fault backstop).
export function rowAt(n: PNode, i: number): Row {
  if (n.packed) return n.packed.row(i);
  if (n.lazyBase !== undefined) {
    // A lazy row is decoded from the page each time: nothing is cached on a read path.
    const at = n.lazy![i]!;
    if (at >= 0) return n.lazyBase.row(at);
  }
  return n.vals[i]!;
}

// colAt reconstructs ONLY column c of row i — the touched-column path (packed-leaf.md §4/§6, the
// OP_Column model).
export function colAt(n: PNode, i: number, c: number): Value {
  if (n.packed) return n.packed.col(i, c);
  if (n.lazyBase !== undefined) {
    const at = n.lazy![i]!;
    if (at >= 0) return n.lazyBase.col(at, c);
  }
  return n.vals[i]![c]!;
}

// colAtNow is colAt with a deferred value resolved, when that needs no stepping: a Packed leaf's
// inline body decodes straight from its page, a Decoded leaf's resolved value is returned as is.
// undefined when the value is out of line, compressed, or a held deferred handle — the caller then
// resolves through the stepping path (readLeafColumnSteps).
export function colAtNow(n: PNode, i: number, c: number): Value | undefined {
  if (n.packed) return n.packed.colNow(i, c);
  if (n.lazyBase !== undefined) {
    const at = n.lazy![i]!;
    if (at >= 0) return n.lazyBase.colNow(at, c);
  }
  const v = n.vals[i]![c]!;
  return v.kind === "unfetched" ? undefined : v;
}

// colIsNull reports whether column c of row i is NULL without reconstructing the value (packed-leaf.md
// §11 Track A4): the PAX null bitmap / zero-length span on a Packed leaf. A deferred large value is never
// NULL.
export function colIsNull(n: PNode, i: number, c: number): boolean {
  if (n.packed) return n.packed.isNull(i, c);
  if (n.lazyBase !== undefined) {
    const at = n.lazy![i]!;
    if (at >= 0) return n.lazyBase.isNull(at, c);
  }
  return n.vals[i]![c]!.kind === "null";
}

// decodedRows returns every value row of a LEAF — the mutation-descent materialization
// (packed-leaf.md §7). A Decoded leaf clones vals; a Packed leaf reconstructs every row so the
// rebuilt node is Decoded (buildLeaf / nodeInsert / nodeRemove / mergeAt then run unchanged).
export function decodedRows(n: PNode): Row[] {
  if (!n.packed && n.lazyBase === undefined) return n.vals.slice();
  const count = nodeLen(n);
  const rows: Row[] = new Array(count);
  for (let i = 0; i < count; i++) rows[i] = rowAt(n, i);
  return rows;
}

export function nodeLen(n: PNode): number {
  return n.packed?.n ?? n.keys.length;
}

export function keyAt(n: PNode, i: number): Uint8Array {
  return n.packed ? n.packed.key(i) : n.keys[i]!;
}

function weightAt(n: PNode, i: number): number {
  return n.packed ? n.packed.weight(i) : n.weights[i]!;
}

function totalWeight(n: PNode): number {
  if (n.wsum !== undefined) {
    if (leafSizesCheck) {
      let sum = 0;
      for (const w of n.weights) sum += w;
      if (sum !== n.wsum) {
        throw new Error(`BUG: a leaf's running weight is ${n.wsum}, its weights sum to ${sum}`);
      }
    }
    return n.wsum;
  }
  let total = 0;
  for (let i = 0; i < nodeLen(n); i++) total += weightAt(n, i);
  return total;
}

// keyViews constructs only a transient directory of block-backed spans when the full serializer
// needs leaf keys. The resident Packed node owns no per-record key objects.
export function keyViews(n: PNode): Uint8Array[] {
  if (!n.packed) return n.keys;
  const keys = new Array<Uint8Array>(n.packed.n);
  for (let i = 0; i < keys.length; i++) keys[i] = n.packed.key(i);
  return keys;
}

function decodedParts(n: PNode): { keys: Uint8Array[]; vals: Row[]; weights: number[] } {
  if (n.lazyBase !== undefined) {
    return { keys: n.keys.map((k) => k.slice()), vals: decodedRows(n), weights: n.weights.slice() };
  }
  if (!n.packed) return { keys: n.keys.slice(), vals: n.vals.slice(), weights: n.weights.slice() };
  const count = n.packed.n;
  const keys = new Array<Uint8Array>(count);
  const vals = new Array<Row>(count);
  const weights = new Array<number>(count);
  for (let i = 0; i < count; i++) {
    keys[i] = n.packed.key(i).slice();
    vals[i] = n.packed.row(i);
    weights[i] = n.packed.weight(i);
  }
  return { keys, vals, weights };
}

// A B+tree node's reference to one child. Under demand paging (P6.4b, spec/design/pager.md §4) a
// clean leaf need not be resident: an interior node keeps an OnDisk page id for such a child and the
// read path faults it through the buffer pool on access. node !== null ⇒ resident (a dirty node, a
// resident interior skeleton node, or a materialized leaf); node === null ⇒ OnDisk(page) — always a
// leaf, since only leaves page, which is what lets nodeCount avoid loading them. An in-memory database
// constructs only resident refs.
// An OnDisk child also carries the tag its parent binds (spec/fileformat/encrypted.md §2; absent in a
// plaintext file).
export type Child = { node: PNode | null; page: number; tag?: Uint8Array };

// leafParts returns a leaf's keys, rows, and weights for reading only: a Decoded leaf's own arrays,
// which the caller must not modify, or a Packed leaf's freshly decoded parts.
function leafParts(n: PNode): { keys: Uint8Array[]; vals: Row[]; weights: number[] } {
  if (!n.packed && n.lazyBase === undefined) {
    return { keys: n.keys, vals: n.vals, weights: n.weights };
  }
  return decodedParts(n);
}

// concatRange returns a fresh array of elements [lo, hi) of the concatenation a ++ b.
function concatRange<T>(a: T[], b: T[], lo: number, hi: number): T[] {
  const out = new Array<T>(hi - lo);
  for (let i = lo; i < hi; i++) out[i - lo] = i < a.length ? a[i]! : b[i - a.length]!;
  return out;
}

export function residentRef(n: PNode): Child {
  return { node: n, page: 0, tag: undefined };
}
export function onDiskRef(page: number, tag?: Uint8Array): Child {
  return { node: null, page, tag };
}

// LeafSource faults a clean leaf page to a resident node on demand (pager.md §4) — the buffer pool,
// behind the table's column types. Defined here so the B+tree traversal can fault without importing
// the storage/format layers (they implement it); a fully-resident in-memory database passes null and
// never faults.
export interface LeafSource {
  loadLeaf(page: number, tag?: Uint8Array): PNode;
  loadLeafSteps?(page: number, tag?: Uint8Array): Steps<PNode>;
  // prefetches reports whether prefetch does anything: only a store that fetches several pages in one
  // request (a page client over the stream transport, spec/design/page-service.md §5.3) benefits
  // from read-ahead.
  prefetches?(): boolean;
  // prefetch is a hint that the scan will fault these leaf pages next, in this order: a source that
  // batches reads brings the ones not already resident in with one request. Errors are left for the
  // fault that follows. Invisible to cost, which counts logical page reads (cost.md).
  prefetch?(pages: { page: number; tag?: Uint8Array }[]): void;
}

// READ_AHEAD_MAX is the most leaves one read-ahead hint names.
const READ_AHEAD_MAX = 64;

// resolveChild resolves c to a resident node, faulting an OnDisk leaf through src. A resident ref
// returns its node directly; an OnDisk leaf with no source is an internal wiring bug (an in-memory
// tree builds no OnDisk ref, and every file-backed traversal supplies a source), so it throws.
function* resolveChildSteps(c: Child, src: LeafSource | null): Steps<PNode> {
  if (c.node !== null) return c.node;
  if (src === null)
    throw new Error(`demand-paged leaf ${c.page} reached with no buffer-pool source`);
  return yield* src.loadLeafSteps === undefined
    ? ioSteps(() => src.loadLeaf(c.page, c.tag))
    : src.loadLeafSteps(c.page, c.tag);
}

function isLeaf(n: PNode): boolean {
  return n.children.length === 0;
}

// LeafShape is a leaf's shape — what its size arithmetic needs beyond its records (format.md "Page
// model", "Fan-out"): the stored fixed-width and variable-width column counts, the elided key column
// count (fixed + var + elided = K), the tree's fixed key length (0 when keys vary), and the sizer
// measuring its content-dependent region savings (null when no column has one). Computed once per
// store from its column types (format.ts leafShape) and threaded through the B+tree's size
// arithmetic, which never sees the types themselves. An index tree (zero value columns, keys of
// varying length) is { 0, 0, 0, 0, null }.
export type LeafShape = {
  fixed: number;
  var: number;
  elided: number;
  keyLen: number;
  sizer: LeafSizer | null;
  // memo is the writing store's compression memo, set on the copy a store hands its tree for an
  // insert or delete: classifying a row whose record needs a disposition plan then reuses the
  // outcomes the write already paid for. Absent compresses again, to the same result.
  memo?: CompressMemo | null;
  // edits is the format layer's part of a lazy leaf's row edits (LeafEdits). A tree without it
  // never builds a lazy leaf.
  edits?: LeafEdits;
};

// LeafEdits compares a row written into a lazy leaf with the page the leaf was copied from.
// Implemented by the format layer over the tree's column types (format.ts).
export interface LeafEdits {
  // changedCols sets changed[c] to whether the encoder, writing row as record at of base's page,
  // would store column c differently from the page. It returns whether the record is stored
  // all-inline (no disposition plan runs).
  changedCols(
    base: PackedLeaf,
    at: number,
    key: Uint8Array,
    row: Row,
    cap: Cap,
    changed: boolean[],
  ): boolean;
  // rowAllInline reports whether a record is stored all-inline.
  rowAllInline(key: Uint8Array, row: Row, cap: Cap): boolean;
}

// LeafSizer measures the bytes a leaf's content-dependent column regions save — base plus offsets,
// dictionaries, packed arrays (format.md "Leaf node") — from decoded rows. Implemented by the format
// layer over the table's column types (format.ts), so pmap never imports the value codec. Facts are
// opaque here; addRow and removeRow keep them equal to a fresh measurement of the rows they hold.
// memo is the writer's compression memo, or null: it changes no result.
export interface LeafSizer {
  newFacts(): unknown;
  addRow(facts: unknown, key: Uint8Array, row: Row, cap: Cap, memo?: CompressMemo | null): void;
  // removeRow takes out a row addRow was given. False when the facts cannot be kept exact this way:
  // the caller drops them and the leaf is measured again.
  removeRow(
    facts: unknown,
    key: Uint8Array,
    row: Row,
    cap: Cap,
    memo?: CompressMemo | null,
  ): boolean;
  // stale reports whether a removal left a column's value range unknown; refresh rescans those
  // columns from the leaf's rows.
  stale(facts: unknown): boolean;
  refresh(facts: unknown, rows: Row[]): void;
  // The lazy-leaf forms (format.ts, "size facts from the page"). measureLazy builds a lazy leaf's
  // facts from its page and its replaced rows, freezing the columns no replacement changed while the
  // leaf's rows are its page's. refreshLazy is refresh for such a leaf. frozen counts the frozen
  // columns: addRow and removeRow apply only when it is 0. replaceCols follows the replacement of row
  // i (not yet applied to n) in a leaf whose rows are its page's: changed is LeafEdits.changedCols'
  // answer and wasLazy says the replaced row is still the page's; remeasure then runs once the row
  // is in place. cloneFacts copies facts for a new generation's copy of the leaf.
  measureLazy(n: PNode, cap: Cap, memo?: CompressMemo | null): unknown;
  refreshLazy(facts: unknown, n: PNode): void;
  frozen(facts: unknown): number;
  replaceCols(
    facts: unknown,
    n: PNode,
    i: number,
    key: Uint8Array,
    row: Row,
    cap: Cap,
    changed: boolean[],
    wasLazy: boolean,
    memo?: CompressMemo | null,
  ): void;
  remeasure(facts: unknown, n: PNode, cap: Cap, memo?: CompressMemo | null): void;
  cloneFacts(facts: unknown): unknown;
  // One pass over a leaf that does not fit as plain regions (buildLeaf). scan classifies every row
  // and scans the dictionary columns, hashing each value once; scanDups says some dictionary payload
  // repeats. scanFacts is the facts of rows [lo, hi) from the scan: a dictionary column with no
  // repeated payload is kept as distinct, with no map, so removeRow needs no lookup there;
  // factsDistinct says so, and the writer drops such facts before addRow. The scan also carries the facts of one
  // running half of a candidate split: scanReset empties it, scanAdd takes row i in (up: the half
  // grows from the first row; false: from the last) and scanSaving is its region savings at count
  // rows.
  //
  // A leaf scanned with no repeated payload starts a distinct group, a token its facts and those of
  // its split halves carry (scanFacts' group; factsGroup reads it). Rows only leave such leaves, so
  // while two of them hold exact facts of one group their rows repeat no payload: scanDistinct is
  // the scan of such rows, which hashes nothing.
  scan(keys: Uint8Array[], rows: Row[], cap: Cap, memo?: CompressMemo | null): unknown;
  scanDistinct(keys: Uint8Array[], rows: Row[], cap: Cap, memo?: CompressMemo | null): unknown;
  scanDups(scan: unknown): boolean;
  scanFacts(scan: unknown, rows: Row[], lo: number, hi: number, group?: object | null): unknown;
  scanReset(scan: unknown): void;
  scanAdd(scan: unknown, rows: Row[], i: number, up: boolean): void;
  scanSaving(scan: unknown, count: number): number;
  factsDistinct(facts: unknown): boolean;
  factsGroup(facts: unknown): object | null;
  // The region savings of a leaf of n rows with these facts; an upper bound while they are stale.
  saving(facts: unknown, n: number): number;
  // pre[m] is the region savings of rows [0, m) and suf[m] that of rows [m, n), for m in [0, n].
  // scan is the leaf's scan when it has one.
  boundarySavings(
    keys: Uint8Array[],
    rows: Row[],
    cap: Cap,
    memo?: CompressMemo | null,
    scan?: unknown,
  ): { pre: number[]; suf: number[] };
}

// leafOverhead is the bytes a leaf's payload carries beyond Σ record_size, before the
// content-dependent savings (format.md "Page model"): the leaf flags byte, the key directory (2·N, or
// the 2-byte key length of a fixed-key tree), the column directory (2·(K+1)), and per stored region a
// kind byte plus — fixed-width — the null bitmap (ceil(N/8)) or — variable-width — the value
// directory (2·N). Defined here (not format.ts) to avoid a format↔pmap import cycle; the cross-core
// goldens catch any drift. Interior nodes do not use this (their payload is 8·N + 4 + Σ sep_len).
//   leafOverhead(N, cols) = 1 + keyDir(N) + 2·(K+1) + F·(1 + ceil(N/8)) + V·(1 + 2·N)
export function leafOverhead(n: number, shape: LeafShape): number {
  const k = shape.fixed + shape.var + shape.elided;
  const keyDir = shape.keyLen > 0 ? 2 : 2 * n;
  return 1 + keyDir + 2 * (k + 1) + shape.fixed * (1 + Math.ceil(n / 8)) + shape.var * (1 + 2 * n);
}

// payload is this node's serialized size (format.md "Leaf payload"): a leaf is Σ weights +
// leafOverhead(N, shape) − saving(leaf); an interior node is ptr·(N+1) + 4·N + Σ sep_len (child
// pointers + separator directory + key blob — record-free, v24; ptr is 4, or 20 with an encrypted
// file's tags — encrypted.md §2).
function payload(n: PNode, cap: Cap, shape: LeafShape): number {
  if (isLeaf(n)) {
    // A Packed leaf's size is its page's content length: the page was written by these rules.
    if (n.packed) return n.packed.payloadLen;
    const plain = totalWeight(n) + leafOverhead(n.keys.length, shape);
    return plain - contentSaving(n, cap, shape);
  }
  return interiorPayload(cap, n.keys);
}

// leafSizesCheck makes contentSaving compare a leaf's kept size facts — a lazy leaf's frozen and
// page-measured ones included — with a fresh measurement of its decoded rows, and totalWeight compare
// a leaf's running weight with the sum of its weights. buildLeaf then also finds its split point by
// the tabled rule and compares it with the running one, and the format layer compares a row
// classified without its disposition plan with the plan (tests).
let leafSizesCheck = false;

// setLeafSizesCheck turns the kept-size-facts cross-check on or off (tests only).
export function setLeafSizesCheck(on: boolean): void {
  leafSizesCheck = on;
}

// leafSizesCheckOn reports whether the cross-check is on, for the format layer's part of it.
export function leafSizesCheckOn(): boolean {
  return leafSizesCheck;
}

// measureLeaf is the size facts of a Decoded leaf's rows, measured from scratch.
function measureLeaf(n: PNode, cap: Cap, sizer: LeafSizer, memo: CompressMemo | null): unknown {
  const facts = sizer.newFacts();
  const count = n.keys.length;
  for (let i = 0; i < count; i++) sizer.addRow(facts, n.keys[i]!, n.vals[i]!, cap, memo);
  return facts;
}

// contentSaving is the bytes a Decoded leaf's content-dependent encodings save: the shared key prefix
// and the column regions' savings (format.md "Leaf payload").
function contentSaving(n: PNode, cap: Cap, shape: LeafShape): number {
  const count = n.keys.length;
  let saving = 0;
  if (count >= 2) saving += leafPrefixSaving(count, n.keys[0]!, n.keys[count - 1]!);
  const sizer = shape.sizer;
  if (sizer !== null && count > 0 && n.lazyBase !== undefined) {
    saving += lazySaving(n, cap, sizer, shape.memo ?? null);
  } else if (sizer !== null && count > 0) {
    const memo = shape.memo ?? null;
    let sz = n.sizes;
    if (sz === undefined || sz.n !== count) {
      sz = { n: count, facts: measureLeaf(n, cap, sizer, memo) };
      // A dirty node is its writer's alone, so it may keep the facts.
      if (n.page === 0 && n[mutationOwner] !== undefined) n.sizes = sz;
    } else {
      sizer.refresh(sz.facts, n.vals);
      if (leafSizesCheck) {
        const want = sizer.saving(measureLeaf(n, cap, sizer, null), count);
        const got = sizer.saving(sz.facts, count);
        if (want !== got) {
          throw new Error(
            `BUG: a leaf's kept size facts save ${got} bytes, a fresh measurement ${want}`,
          );
        }
      }
    }
    saving += sizer.saving(sz.facts, count);
  }
  return saving;
}

// lazySaving is contentSaving's column-region part for a lazy leaf: its facts come from its page
// (LeafSizer.measureLazy), not from decoded rows.
function lazySaving(n: PNode, cap: Cap, sizer: LeafSizer, memo: CompressMemo | null): number {
  const count = n.keys.length;
  let sz = n.sizes;
  if (sz === undefined || sz.n !== count) {
    sz = { n: count, facts: sizer.measureLazy(n, cap, memo) };
    // A dirty node is its writer's alone, so it may keep the facts.
    if (n.page === 0 && n[mutationOwner] !== undefined) n.sizes = sz;
  }
  sizer.refreshLazy(sz.facts, n);
  const got = sizer.saving(sz.facts, count);
  if (leafSizesCheck) {
    const rows = decodedRows(n);
    const fresh = sizer.newFacts();
    for (let i = 0; i < count; i++) sizer.addRow(fresh, n.keys[i]!, rows[i]!, cap, null);
    const want = sizer.saving(fresh, count);
    if (want !== got) {
      throw new Error(
        `BUG: a lazy leaf's size facts save ${got} bytes (${sizer.frozen(sz.facts)} columns frozen), a fresh measurement ${want}`,
      );
    }
  }
  return got;
}

// lazyCopy returns a dirty copy of leaf n that generation owns, with no row decoded: n is a Packed
// leaf, or a lazy leaf an earlier generation built.
//
// The copy's keys are views of the page. The copy already holds the page through lazyBase, and a
// page block is never written again. A split or merge copies the keys out first (materialize,
// decodedParts), so no separator keeps a page alive.
function lazyCopy(n: PNode, generation: MutationGeneration, sizer: LeafSizer | null): PNode {
  const p = n.packed;
  if (p !== undefined) {
    const count = p.n;
    const keys = new Array<Uint8Array>(count);
    const vals = new Array<Row>(count);
    const lazy = new Array<number>(count);
    for (let i = 0; i < count; i++) {
      keys[i] = p.key(i);
      vals[i] = NO_ROW;
      lazy[i] = i;
    }
    const weights = p.recordWeights();
    let wsum = 0;
    for (const w of weights) wsum += w;
    if (leafSizesCheck) {
      for (let i = 0; i < count; i++) {
        if (weights[i] !== p.weight(i)) {
          throw new Error(
            `BUG: record ${i} weighs ${weights[i]} by columns, ${p.weight(i)} by record`,
          );
        }
      }
    }
    const patchOK = p.inlineOnly();
    return {
      keys,
      vals,
      weights,
      children: [],
      page: 0,
      lazyBase: p,
      lazy,
      wsum,
      sameRows: true,
      patchOK,
      // A record of the page with a large-value form has a plan any column can change, so every
      // column counts as changed from the first replacement.
      colDirty: new Array<boolean>(p.colTypes.length).fill(!patchOK),
      [mutationOwner]: generation,
    };
  }
  return {
    keys: n.keys.slice(),
    vals: n.vals.slice(),
    weights: n.weights.slice(),
    children: [],
    page: 0,
    lazyBase: n.lazyBase,
    lazy: n.lazy!.slice(),
    wsum: totalWeight(n),
    sameRows: n.sameRows,
    patchOK: n.patchOK,
    colDirty: n.colDirty?.slice(),
    sizes:
      n.sizes !== undefined && sizer !== null
        ? { n: n.sizes.n, facts: sizer.cloneFacts(n.sizes.facts) }
        : undefined,
    [mutationOwner]: generation,
  };
}

// trackWeight starts the running weight total of a leaf its writer is about to edit in place.
function trackWeight(n: PNode): void {
  if (n.wsum !== undefined || !leafLazy) return;
  let sum = 0;
  for (const w of n.weights) sum += w;
  n.wsum = sum;
}

// rowSetChanged records that a row was inserted into or removed from a lazy leaf: its rows are no
// longer its page's, so no region can be copied and frozen facts no longer hold.
function rowSetChanged(n: PNode, sizer: LeafSizer | null): void {
  n.sameRows = false;
  n.colDirty = undefined;
  const sz = n.sizes;
  if (sz !== undefined && (sizer === null || sizer.frozen(sz.facts) > 0)) n.sizes = undefined;
}

// materialize decodes every row a lazy leaf still reads from its page and copies its keys, making it
// an ordinary Decoded leaf. Only the owning writer calls it.
function materialize(n: PNode): void {
  n.vals = decodedRows(n);
  n.keys = n.keys.map((k) => k.slice());
  n.lazyBase = undefined;
  n.lazy = undefined;
  n.sameRows = undefined;
  n.patchOK = undefined;
  n.colDirty = undefined;
  n.sizes = undefined; // buildLeaf measures the rows
}

// decodedLeafCopy returns leaf n as an ordinary Decoded leaf with the same page: every row decoded,
// its keys copied. For a persisted lazy root that is not re-faulted (storage.ts refaultRootSteps).
export function decodedLeafCopy(n: PNode): PNode {
  const { keys, vals, weights } = decodedParts(n);
  return { keys, vals, weights, children: [], page: n.page, tag: n.tag };
}

// replaceRow replaces row i of a lazy leaf its writer owns.
function replaceRow(
  n: PNode,
  i: number,
  key: Uint8Array,
  val: Row,
  weight: number,
  cap: Cap,
  shape: LeafShape,
): void {
  const edits = shape.edits!;
  const sizer = shape.sizer;
  const memo = shape.memo ?? null;
  const wasLazy = n.lazy![i]! >= 0;
  let changed: boolean[] | null = null;
  if (n.sameRows) {
    // Row i is the page's record i. Which columns does the new row store differently?
    const dirty = n.colDirty!;
    changed = new Array<boolean>(dirty.length).fill(false);
    // Past RECORD_MAX the record's disposition plan decides each value's form, so no column is known
    // unchanged and the leaf is not patched.
    if (!edits.changedCols(n.lazyBase!, i, key, val, cap, changed)) n.patchOK = false;
    for (let c = 0; c < changed.length; c++) {
      if (!n.patchOK) changed[c] = true;
      if (changed[c]) dirty[c] = true;
    }
  } else if (n.patchOK && !edits.rowAllInline(key, val, cap)) {
    n.patchOK = false;
  }
  let redo = false;
  const sz = n.sizes;
  if (sz !== undefined) {
    if (sizer === null || sz.n !== n.vals.length) {
      n.sizes = undefined;
    } else if (changed !== null) {
      sizer.replaceCols(sz.facts, n, i, key, val, cap, changed, wasLazy, memo);
      redo = true;
    } else if (sizer.removeRow(sz.facts, key, rowAt(n, i), cap, memo)) {
      sizer.addRow(sz.facts, key, val, cap, memo);
    } else {
      n.sizes = undefined;
    }
  }
  n.vals[i] = val;
  n.lazy![i] = -1;
  n.wsum! += weight - n.weights[i]!;
  n.weights[i] = weight;
  if (redo) sizer!.remeasure(sz!.facts, n, cap, memo);
}

// payloadAtMost reports whether a leaf's payload is at most limit. The content-dependent saving is
// computed only when the size without it does not already decide.
function payloadAtMost(n: PNode, limit: number, cap: Cap, shape: LeafShape): boolean {
  if (n.packed) return n.packed.payloadLen <= limit;
  const plain = totalWeight(n) + leafOverhead(n.keys.length, shape);
  if (plain <= limit) return true;
  const sz = n.sizes;
  const sizer = shape.sizer;
  const count = n.keys.length;
  if (sz !== undefined && sizer !== null && sz.n === count && sizer.stale(sz.facts)) {
    // A stale range overstates the saving: when even that leaves the leaf above the limit, the
    // columns need no rescan.
    let upper = sizer.saving(sz.facts, count);
    if (count >= 2) upper += leafPrefixSaving(count, n.keys[0]!, n.keys[count - 1]!);
    if (plain - upper > limit) return false;
  }
  return plain - contentSaving(n, cap, shape) <= limit;
}

// interiorPayload is an interior node's payload for these separators (format.md "Interior node";
// encrypted.md §2).
function interiorPayload(cap: Cap, keys: Uint8Array[]): number {
  let seps = 0;
  for (const k of keys) seps += k.length;
  return capPtr(cap) * (keys.length + 1) + 4 * keys.length + seps;
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
  const lowerBound = (key: Uint8Array, greater: boolean): number => {
    let lo = 0;
    let hi = nodeLen(n);
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      const c = compareBytes(keyAt(n, mid), key);
      if (c < 0 || (greater && c === 0)) lo = mid + 1;
      else hi = mid;
    }
    return lo;
  };
  const first = b.lo === null ? 0 : lowerBound(b.lo, !b.loInc);
  let last = b.hi === null ? nodeLen(n) : lowerBound(b.hi, b.hiInc);
  if (last < first) last = first;
  return [first, last];
}

// search binary-searches a LEAF's keys: found ⇒ keys[index] === key, else index is the insertion slot.
function search(n: PNode, key: Uint8Array): { index: number; found: boolean } {
  if (n.packed?.search !== undefined) return n.packed.search(key);
  let lo = 0;
  let hi = nodeLen(n);
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    const c = compareBytes(keyAt(n, mid), key);
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
function newLeaf(
  keys: Uint8Array[],
  vals: Row[],
  weights: number[],
  owner?: MutationGeneration,
): PNode {
  return { keys, vals, weights, children: [], page: 0, [mutationOwner]: owner };
}

function newInterior(keys: Uint8Array[], children: Child[], owner?: MutationGeneration): PNode {
  return { keys, vals: [], weights: [], children, page: 0, [mutationOwner]: owner };
}

// newFreshInterior is a fresh dirty interior with no origin and no owner -- what the deferred-interior
// fold writes in place of a stale page and each of its ancestors (remap.ts unstale).
export function newFreshInterior(keys: Uint8Array[], children: Child[]): PNode {
  return newInterior(keys, children);
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
  private generation: MutationGeneration;

  constructor(root: PNode | null = null, rowCount: bigint | null = 0n) {
    this.root = root;
    this.rowCount = rowCount;
    this.generation = newMutationGeneration();
  }

  clone(): PMap {
    this.freezeMutationGeneration();
    return new PMap(this.root, this.rowCount);
  }

  // unstale is the deferred-interior fold (deferred-interior.md §9.4): replace the root with a copy
  // whose stale interiors and their ancestors are dirty, when any is stale.
  unstale(fold: (root: PNode | null) => PNode | null): void {
    const r = fold(this.root);
    if (r !== null) this.root = r;
  }

  // Invalidate every node stamped by this generation without walking the tree. A later INSERT
  // obtains a fresh token and path-copies the first touched suffix; repeated invalidation is safe.
  freezeMutationGeneration(): void {
    this.generation.active = false;
  }

  private currentMutationGeneration(): MutationGeneration {
    if (!this.generation.active) this.generation = newMutationGeneration();
    return this.generation;
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

  // replaceRoot swaps in an equivalent root — the same page, freshly faulted (storage.ts
  // TableStore.demoteCleanLeaves). The row count is unchanged.
  replaceRoot(root: PNode): void {
    this.root = root;
  }

  // get looks up the row at key — a root→leaf descent (interior nodes only route, v24). src faults
  // an OnDisk leaf on the descent (null for a fully-resident in-memory tree); an I/O error
  // propagates as a thrown EngineError.
  get(key: Uint8Array, src: LeafSource | null): Row | undefined {
    return this.getCounted(key, src).row;
  }

  *getSteps(key: Uint8Array, src: LeafSource | null): Steps<Row | undefined> {
    return (yield* this.getCountedSteps(key, src)).row;
  }

  // One direct root→leaf point descent plus its visited-node count. Reconstructs a hit once and
  // creates neither a range bound nor an emitted storage-key object.
  getCounted(
    key: Uint8Array,
    src: LeafSource | null,
  ): { row: Row | undefined; nodes: number; rowsReconstructed: number } {
    return runSync(this.getCountedSteps(key, src));
  }

  *getCountedSteps(
    key: Uint8Array,
    src: LeafSource | null,
  ): Steps<{ row: Row | undefined; nodes: number; rowsReconstructed: number }> {
    let n = this.root;
    if (n === null) return { row: undefined, nodes: 0, rowsReconstructed: 0 };
    let nodes = 1;
    while (!isLeaf(n)) {
      n = yield* resolveChildSteps(n.children[childSlot(n, key)], src);
      nodes++;
    }
    const { index, found } = search(n, key);
    return {
      row: found ? rowAt(n, index) : undefined,
      nodes,
      rowsReconstructed: found ? 1 : 0,
    };
  }

  // insert inserts or overwrites key with val (on-disk record size weight); cap is the page payload
  // capacity and shape the leaf's column-class shape. Returns the previous row if key was present
  // (an overwrite, size unchanged), else undefined (a new insert, which grows the size). An
  // overwrite can change the weight, so it too may overflow and split.
  insert(
    key: Uint8Array,
    val: Row,
    weight: number,
    cap: Cap,
    shape: LeafShape,
    src: LeafSource | null,
  ): Row | undefined {
    return runSync(this.insertSteps(key, val, weight, cap, shape, src));
  }

  *insertSteps(
    key: Uint8Array,
    val: Row,
    weight: number,
    cap: Cap,
    shape: LeafShape,
    src: LeafSource | null,
  ): Steps<Row | undefined> {
    const ctx: InsCtx = { old: undefined, replaced: false, want: true };
    yield* this.insertCtxSteps(key, val, weight, cap, shape, src, ctx);
    return ctx.old;
  }

  // putSteps is insertSteps for a caller that does not use the previous row: a row a lazy leaf has
  // not decoded stays undecoded. It returns whether key was present.
  *putSteps(
    key: Uint8Array,
    val: Row,
    weight: number,
    cap: Cap,
    shape: LeafShape,
    src: LeafSource | null,
  ): Steps<boolean> {
    const ctx: InsCtx = { old: undefined, replaced: false, want: false };
    yield* this.insertCtxSteps(key, val, weight, cap, shape, src, ctx);
    return ctx.replaced;
  }

  private *insertCtxSteps(
    key: Uint8Array,
    val: Row,
    weight: number,
    cap: Cap,
    shape: LeafShape,
    src: LeafSource | null,
    ctx: InsCtx,
  ): Steps<void> {
    const generation = this.currentMutationGeneration();
    if (this.root === null) {
      this.root = newLeaf([key], [val], [weight], generation);
      if (this.rowCount !== null) this.rowCount += 1n;
      return;
    }
    const out = yield* nodeInsertSteps(
      this.root,
      key,
      val,
      weight,
      ctx,
      generation,
      src,
      cap,
      shape,
    );
    // A split root grows the tree by one level. A multi-way split hands up several separators, and
    // the new root is built like any interior (it may split in turn).
    let top = out;
    while (top.whole === null) {
      const keys = [top.sep];
      const children = [residentRef(top.left), residentRef(top.right)];
      for (const p of top.more) {
        keys.push(p.sep);
        children.push(residentRef(p.node));
      }
      // No fitting root (near-cap separators): leave it whole (0A000 at serialize).
      top = buildInterior(keys, children, cap, null, generation, true) ?? {
        whole: newInterior(keys, children, generation),
      };
    }
    this.root = top.whole;
    if (!ctx.replaced && this.rowCount !== null) this.rowCount += 1n;
  }

  // remove deletes key. Returns the removed row, or undefined if absent (then the map is unchanged).
  // cap is the page payload capacity and shape the leaf's column-class shape (the rebalance
  // threshold). src faults OnDisk leaves the delete descends into / rebalances against
  // (spec/design/pager.md §4).
  remove(key: Uint8Array, cap: Cap, shape: LeafShape, src: LeafSource | null): Row | undefined {
    return runSync(this.removeSteps(key, cap, shape, src));
  }

  *removeSteps(
    key: Uint8Array,
    cap: Cap,
    shape: LeafShape,
    src: LeafSource | null,
  ): Steps<Row | undefined> {
    return (yield* this.removeWantSteps(key, true, cap, shape, src)).removed;
  }

  // deleteSteps is removeSteps for a caller that does not use the removed row (see putSteps). It
  // returns whether key was present.
  *deleteSteps(
    key: Uint8Array,
    cap: Cap,
    shape: LeafShape,
    src: LeafSource | null,
  ): Steps<boolean> {
    return (yield* this.removeWantSteps(key, false, cap, shape, src)).ok;
  }

  private *removeWantSteps(
    key: Uint8Array,
    want: boolean,
    cap: Cap,
    shape: LeafShape,
    src: LeafSource | null,
  ): Steps<{ ok: boolean; removed: Row | undefined }> {
    if (this.root === null) return { ok: false, removed: undefined };
    const res = yield* nodeRemoveSteps(
      this.root,
      key,
      want,
      this.currentMutationGeneration(),
      src,
      cap,
      shape,
    );
    if (!res.ok) return { ok: false, removed: undefined };
    let newRoot: PNode | null = res.node;
    // The root may have drained: an empty leaf becomes the empty map; a 0-key interior root hands
    // the root down a level (height shrinks). The root is exempt from the underfull rule, so no
    // rebalance here.
    while (newRoot !== null && nodeLen(newRoot) === 0) {
      // The lone surviving child becomes the new root — fault it if it is an OnDisk leaf (a tree of
      // height 2 can collapse to its single bottom child).
      newRoot = isLeaf(newRoot) ? null : yield* resolveChildSteps(newRoot.children[0], src);
    }
    this.root = newRoot;
    if (this.rowCount !== null) this.rowCount -= 1n;
    return { ok: true, removed: res.removed };
  }

  // inorder returns all (key, row) pairs in ascending key order — a leaf walk (records are
  // leaf-only, v24). Eager (the cost contract charges per row in the executor loop, not here —
  // spec/design/cost.md); src faults each OnDisk leaf through the pool, and the faulted node is
  // dropped (GC) once its rows are collected, so the resident leaf set stays bounded by the pool,
  // not the tree (pager.md §4).
  inorder(src: LeafSource | null): { keys: Uint8Array[]; vals: Row[] } {
    const { keys, vals } = this.rangeEntriesCounted(unboundedBound(), src);
    return { keys, vals };
  }

  // nodeCount is the number of B+tree nodes (pages) in this tree — the page_read count a full
  // scan charges (spec/design/cost.md §3 "page_read"). A scan walks every node, so this is the
  // structural node count (interior + leaf); 0 for an empty map. Deterministic and byte-identical
  // across cores (the node boundaries are a §8 byte contract — format.md).
  //
  // All leaves sit at one depth (format.md "Fan-out"; the invariant open's skeleton load already
  // uses), so an interior's children are all leaves or all interiors. A leaf parent therefore
  // contributes its child count without visiting the children, and the walk is O(interior nodes)
  // rather than O(leaves): the planner calls it per statement. An OnDisk child is always a clean
  // leaf and is counted without loading it.
  nodeCount(): number {
    const count = (n: PNode): number => {
      if (isLeaf(n)) return 1;
      const first = n.children[0]!.node;
      if (first === null || isLeaf(first)) return 1 + n.children.length;
      let total = 1;
      for (const c of n.children) total += count(c.node!);
      return total;
    };
    return this.root === null ? 0 : count(this.root);
  }

  // Root-to-leaf node count (0 empty, 1 root leaf). Every leaf is at the same depth, so the
  // first-child path is the height; an on-disk child is always a leaf, so no page is faulted.
  height(): number {
    let height = 0;
    let n = this.root;
    while (n !== null) {
      height++;
      if (isLeaf(n)) break;
      const child = n.children[0]!.node;
      if (child === null) {
        height++;
        break;
      }
      n = child;
    }
    return height;
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
      let here = totalWeight(n);
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
    return runSync(this.rangeEntriesCountedSteps(b, src));
  }

  *rangeEntriesCountedSteps(
    b: KeyBound,
    src: LeafSource | null,
  ): Steps<{ keys: Uint8Array[]; vals: Row[]; nodes: number }> {
    const keys: Uint8Array[] = [];
    const vals: Row[] = [];
    const iterator = new TreeStepIterator(
      this.root,
      b,
      src,
      false,
      (node, index): [Uint8Array, Row] => [keyAt(node, index), rowAt(node, index)],
    );
    try {
      for (;;) {
        const item = yield* iterator.nextSteps();
        if (item.done) break;
        keys.push(item.value[0]);
        vals.push(item.value[1]);
      }
      return { keys, vals, nodes: iterator.nodesVisited };
    } finally {
      yield* iterator.returnSteps();
    }
  }

  // foldScan is the columnar lanes' walk (packed-leaf.md §11 Tracks A2–A4): rangeEntriesCounted's
  // windowed walk (so the visited-node set — and page_read — is identical), calling visit(n, i) per
  // admitted leaf record instead of reconstructing a row. visit reads the record's touched columns via
  // colAt or their stored bytes. Returns { rowCount, nodes }, so the caller charges the same page_read /
  // storage_row_read as the row feed.
  foldScan(
    b: KeyBound,
    src: LeafSource | null,
    visit: (n: PNode, i: number) => void,
  ): { rowCount: number; nodes: number } {
    return runSync(
      this.foldScanSteps(
        b,
        src,
        // biome-ignore lint/correctness/useYield: A synchronous visitor is a continuation with no effects.
        function* (node, index) {
          visit(node, index);
        },
      ),
    );
  }

  // `progress`, when given, receives the nodes visited even if the walk is interrupted (a canceled
  // lane charges the work it consumed).
  *foldScanSteps(
    b: KeyBound,
    src: LeafSource | null,
    visit: (n: PNode, i: number) => Steps<void>,
    progress?: { nodes: number },
  ): Steps<{ rowCount: number; nodes: number }> {
    let rowCount = 0;
    const iterator = new TreeStepIterator(
      this.root,
      b,
      src,
      false,
      (node, index): [PNode, number] => [node, index],
    );
    try {
      for (;;) {
        const item = yield* iterator.nextSteps();
        if (item.done) break;
        yield* visit(item.value[0], item.value[1]);
        rowCount++;
      }
      return { rowCount, nodes: iterator.nodesVisited };
    } finally {
      if (progress !== undefined) progress.nodes = iterator.nodesVisited;
      yield* iterator.returnSteps();
    }
  }

  // foldLeavesSteps is foldScanSteps a leaf at a time: visitLeaf(node, first, last) folds records
  // first..last of one leaf (in key order; the same records, in the same order, foldScanSteps visits
  // one by one), so a lane's per-record work is one loop iteration rather than a continuation.
  // rowCount counts the records of every leaf visitLeaf returned from.
  *foldLeavesSteps(
    b: KeyBound,
    src: LeafSource | null,
    visitLeaf: (n: PNode, first: number, last: number) => Steps<void>,
    progress?: { nodes: number },
  ): Steps<{ rowCount: number; nodes: number }> {
    let rowCount = 0;
    const iterator = new TreeStepIterator(this.root, b, src, false, () => undefined);
    try {
      for (;;) {
        const leaf = yield* iterator.nextLeafSteps();
        if (leaf === null) break;
        if (leaf.last < leaf.first) continue;
        yield* visitLeaf(leaf.node, leaf.first, leaf.last);
        rowCount += leaf.last - leaf.first + 1;
      }
      return { rowCount, nodes: iterator.nodesVisited };
    } finally {
      if (progress !== undefined) progress.nodes = iterator.nodesVisited;
      yield* iterator.returnSteps();
    }
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
    for (const [key, row] of this.scanRangeIter(b, src)) {
      if (!visit(key, row)) return;
    }
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
    for (const [key, row] of this.scanRangeRevIter(b, src)) {
      if (!visit(key, row)) return;
    }
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
  scanRangeIter(b: KeyBound, src: LeafSource | null): Generator<[Uint8Array, Row]> {
    return syncStepIterator(this.scanRangeStepsIter(b, src));
  }

  scanRangeStepsIter(
    b: KeyBound,
    src: LeafSource | null,
    reverse = false,
  ): StepIterator<[Uint8Array, Row]> {
    this.freezeMutationGeneration();
    return new TreeStepIterator(this.root, b, src, reverse, (node, index) => [
      keyAt(node, index),
      rowAt(node, index),
    ]);
  }

  // scanRangeRevIter is scanRangeIter in reverse — the pull-model equivalent of scanRangeRev,
  // yielding the in-bound pairs in DESCENDING key order (the exact reverse of scanRangeIter).
  scanRangeRevIter(b: KeyBound, src: LeafSource | null): Generator<[Uint8Array, Row]> {
    return syncStepIterator(this.scanRangeStepsIter(b, src, true));
  }

  // SELECT's row-only pull form: identical range traversal without allocating an unused [key,row]
  // tuple for every emitted record.
  scanRowsIter(b: KeyBound, src: LeafSource | null, reverse: boolean): Generator<Row> {
    return syncStepIterator(this.scanRowsStepsIter(b, src, reverse));
  }

  scanRowsStepsIter(b: KeyBound, src: LeafSource | null, reverse: boolean): StepIterator<Row> {
    this.freezeMutationGeneration();
    return new TreeStepIterator(this.root, b, src, reverse, rowAt);
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
            children[i] = onDiskRef(c.node.page, c.node.tag);
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
      return { keys: node.keys, vals: [], weights: [], children, page: node.page, tag: node.tag };
    };
    if (this.root !== null) {
      const rebuilt = demote(this.root);
      if (rebuilt !== null) this.root = rebuilt;
    }
  }
}

// Both public synchronous generators and runtime pulls drive this one retained traversal. A
// page read suspends before entering its child; no row or earlier traversal is replayed. hinted
// counts the descents into an interior's children still covered by its last read-ahead hint.
type ScanFrame = { node: PNode; next: number; last: number; hinted: number };

class TreeStepIterator<T> implements StepIterator<T> {
  private readonly frames: ScanFrame[] = [];
  private readonly bound: KeyBound;
  private readonly source: LeafSource | null;
  private readonly reverse: boolean;
  private readonly project: (node: PNode, index: number) => T;
  private closed = false;
  private traversed = 0;
  // window is the read-ahead window (spec/design/page-service.md §5.3): when the traversal descends
  // into a leaf of an interior node it already holds, it hints that leaf together with the next
  // leaves of that node it will visit, within the scan's bounds. The window starts at one leaf (no
  // read-ahead, so a scan stopped by LIMIT after one leaf fetches nothing extra) and doubles with each
  // hint up to READ_AHEAD_MAX, so a long scan fetches its leaves in batches of 64 — every leaf of a
  // subtree in one request when it has at most that many.
  private window = 1;
  nodesVisited = 0;

  constructor(
    root: PNode | null,
    bound: KeyBound,
    source: LeafSource | null,
    reverse: boolean,
    project: (node: PNode, index: number) => T,
  ) {
    this.bound = bound;
    this.source = source;
    this.reverse = reverse;
    this.project = project;
    if (root !== null) this.enter(root);
  }

  private enter(node: PNode): void {
    this.nodesVisited++;
    const leaf = isLeaf(node);
    const [first, end] = leaf ? entryWindow(this.bound, node) : childWindow(this.bound, node);
    const last = leaf ? end - 1 : end;
    this.frames.push({
      node,
      next: this.reverse ? last : first,
      last: this.reverse ? first : last,
      hinted: 0,
    });
  }

  // hint hints the descent into child index of frame's interior and the descents after it, in visit
  // order. It returns how many of those descents the hint covers, the current one included: the
  // caller hints again after that many.
  private hint(frame: ScanFrame, index: number): number {
    const src = this.source;
    if (src === null || src.prefetches?.() !== true) return Number.POSITIVE_INFINITY;
    const step = this.reverse ? -1 : 1;
    const pages: { page: number; tag?: Uint8Array }[] = [];
    let taken = 0;
    for (
      let i = index;
      taken < this.window && (this.reverse ? i >= frame.last : i <= frame.last);
      i += step, taken++
    ) {
      const c = frame.node.children[i]!;
      if (c.node === null) pages.push({ page: c.page, tag: c.tag });
    }
    taken = Math.max(taken, 1);
    if (pages.length === 0) return taken;
    this.window = Math.min(this.window * 2, READ_AHEAD_MAX);
    if (pages.length > 1) src.prefetch!(pages);
    return taken;
  }

  *nextSteps(): Steps<IteratorResult<T>> {
    if (this.closed) return { done: true, value: undefined };
    try {
      while (this.frames.length > 0) {
        const frame = this.frames[this.frames.length - 1]!;
        if (this.reverse ? frame.next < frame.last : frame.next > frame.last) {
          this.frames.pop();
          continue;
        }
        const index = frame.next;
        frame.next += this.reverse ? -1 : 1;
        if (isLeaf(frame.node)) {
          if ((++this.traversed & 63) === 0 && shouldCooperate(64)) yield COOPERATE;
          return { done: false, value: this.project(frame.node, index) };
        }
        if (frame.hinted === 0) frame.hinted = this.hint(frame, index);
        frame.hinted--;
        this.enter(yield* resolveChildSteps(frame.node.children[index]!, this.source));
      }
      this.closed = true;
      return { done: true, value: undefined };
    } catch (error) {
      this.closed = true;
      this.frames.length = 0;
      throw error;
    }
  }

  // nextLeafSteps returns the next leaf of an ascending traversal with the window of its records the
  // bound admits ([first, last], possibly empty), or null at the end — nextSteps a leaf at a time, for
  // a fold that visits a leaf's records in one loop rather than through one continuation each.
  *nextLeafSteps(): Steps<{ node: PNode; first: number; last: number } | null> {
    if (this.closed) return null;
    try {
      while (this.frames.length > 0) {
        const frame = this.frames[this.frames.length - 1]!;
        if (frame.next > frame.last) {
          this.frames.pop();
          continue;
        }
        if (isLeaf(frame.node)) {
          const first = frame.next;
          const last = frame.last;
          frame.next = last + 1;
          if (shouldCooperate(last - first + 1)) yield COOPERATE;
          return { node: frame.node, first, last };
        }
        const index = frame.next;
        frame.next += 1;
        if (frame.hinted === 0) frame.hinted = this.hint(frame, index);
        frame.hinted--;
        this.enter(yield* resolveChildSteps(frame.node.children[index]!, this.source));
      }
      this.closed = true;
      return null;
    } catch (error) {
      this.closed = true;
      this.frames.length = 0;
      throw error;
    }
  }

  // biome-ignore lint/correctness/useYield: Closing this iterator releases only resident traversal state.
  *returnSteps(): Steps<IteratorResult<T>> {
    this.closed = true;
    this.frames.length = 0;
    return { done: true, value: undefined };
  }
}

// pmapFromSkeleton reconstructs a map from a disk-loaded skeleton root (format.ts loadEnginePaged).
// Tables pass their exact v28 catalog count; indexes pass null.
export function pmapFromSkeleton(root: PNode | null, rowCount: bigint | null): PMap {
  return new PMap(root, rowCount);
}

// want says the caller uses the previous row (old).
type InsCtx = { old: Row | undefined; replaced: boolean; want: boolean };
// The result of inserting into a subtree: the rebuilt subtree, or a node that overflowed and split
// into left, a SEPARATOR KEY for the parent, and right. A leaf split COPIES the right leaf's first
// key up (no record leaves the leaf level); an interior split PUSHES its median separator up
// (format.md "Fan-out").
// more is the further nodes of a multi-way split (format.md "Multi-way split"), each with the
// separator before it; empty for a two-way split.
type SplitPiece = { sep: Uint8Array; node: PNode };
type InsOut =
  | { whole: PNode }
  | { whole: null; left: PNode; sep: Uint8Array; right: PNode; more: SplitPiece[] };

// joinSplit prepends left and sep to a split of the remaining entries.
function joinSplit(left: PNode, sep: Uint8Array, rest: InsOut): InsOut {
  if (rest.whole !== null) return { whole: null, left, sep, right: rest.whole, more: [] };
  return {
    whole: null,
    left,
    sep,
    right: rest.left,
    more: [{ sep: rest.sep, node: rest.right }, ...rest.more],
  };
}

// absorbSplit replaces child i of an interior's vectors (mutated in place) with the nodes of sub's
// split and inserts its separators. It returns the index of the last separator inserted — the
// edited position the interior's own split uses.
function absorbSplit(
  keys: Uint8Array[],
  children: Child[],
  i: number,
  sub: Extract<InsOut, { whole: null }>,
): number {
  keys.splice(i, 0, sub.sep, ...sub.more.map((p) => p.sep));
  children.splice(
    i,
    1,
    residentRef(sub.left),
    residentRef(sub.right),
    ...sub.more.map((p) => residentRef(p.node)),
  );
  return i + sub.more.length;
}

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

// splitPointRunning is splitPoint for a leaf with a sizer, over the scan's running facts instead of
// per-boundary tables: one pass up from the first row while the left half fits, one pass down from
// the last while the right half does. Both half sizes are monotone in m, so the passes stop at the
// first boundary that does not fit; the balanced boundary, when it lies past the last fitting one, is
// clamped to it either way. pay is the whole leaf's payload. Null when no two halves fit. Unless full
// is set, the passes are cut short: the left one stops at the balanced boundary and the right half is
// measured there alone, which gives the same boundary whenever that half fits (it is then at or past
// the lowest fitting boundary).
function splitPointRunning(
  keys: Uint8Array[],
  vals: Row[],
  scan: unknown,
  prefix: number[],
  pay: number,
  cap: Cap,
  shape: LeafShape,
  sizer: LeafSizer,
  rightEdge: boolean,
  full: boolean,
): number | null {
  const n = keys.length;
  const total = prefix[n]!;
  sizer.scanReset(scan);
  let mMax = 0;
  let mBalanced = 0;
  for (let m = 1; m <= n - 1; m++) {
    sizer.scanAdd(scan, vals, m - 1, true);
    // A half is no larger than its records and overhead (a one-key prefix costs up to two bytes),
    // so a boundary whose bound decides neither test is not measured.
    const bound = prefix[m]! + leafOverhead(m, shape) + 2;
    if (bound <= cap.c && (mBalanced !== 0 || 2 * bound < pay)) {
      mMax = m;
      continue;
    }
    const left =
      prefix[m]! +
      leafOverhead(m, shape) -
      leafPrefixSaving(m, keys[0]!, keys[m - 1]!) -
      sizer.scanSaving(scan, m);
    if (left > cap.c) break;
    mMax = m;
    if (mBalanced === 0 && 2 * left >= pay) {
      mBalanced = m;
      // The balanced boundary is the answer when the right half fits there.
      if (!rightEdge && !full) break;
    }
  }
  if (mMax === 0) return null;
  sizer.scanReset(scan);
  if (!full) {
    // The right half is measured once, at the boundary the left pass chose. It fits there in all
    // but rare leaves; the full passes serve those.
    const m = !rightEdge && mBalanced !== 0 ? mBalanced : mMax;
    for (let i = n - 1; i >= m; i--) sizer.scanAdd(scan, vals, i, false);
    const right =
      total -
      prefix[m]! +
      leafOverhead(n - m, shape) -
      leafPrefixSaving(n - m, keys[m]!, keys[n - 1]!) -
      sizer.scanSaving(scan, n - m);
    if (right <= cap.c) return m;
    return splitPointRunning(keys, vals, scan, prefix, pay, cap, shape, sizer, rightEdge, true);
  }
  let mMin = 0;
  for (let m = n - 1; m >= 1; m--) {
    sizer.scanAdd(scan, vals, m, false);
    const right =
      total -
      prefix[m]! +
      leafOverhead(n - m, shape) -
      leafPrefixSaving(n - m, keys[m]!, keys[n - 1]!) -
      sizer.scanSaving(scan, n - m);
    if (right > cap.c) break;
    mMin = m;
  }
  if (mMin === 0 || mMin > mMax) return null;
  let m = mMax;
  if (!rightEdge && mBalanced !== 0) m = mBalanced;
  return m < mMin ? mMin : m;
}

// buildLeaf builds a leaf from its parts; if its payload overflows cap, it splits 2-way COPY-UP
// (format.md "Leaf split"): the left leaf keeps records [0, m), the right leaf [m, N), and the
// separator handed up is a COPY of keys[m] (the right leaf's first key). edited is the index of
// the just-inserted/replaced record (null for the delete path's merge-overflow, which splits
// balanced). A leaf with a single over-cap record is left whole (defensive — the oversize surfaces
// as 0A000 when serialized). group, when set, says the rows come from leaves of one distinct group
// (sharedGroup): no dictionary payload repeats among them.
function buildLeaf(
  keys: Uint8Array[],
  vals: Row[],
  weights: number[],
  cap: Cap,
  shape: LeafShape,
  edited: number | null,
  owner?: MutationGeneration,
  group: object | null = null,
): InsOut {
  const n = keys.length;
  let total = 0;
  for (const w of weights) total += w;
  let pay = total + leafOverhead(n, shape);
  // The content-dependent savings are measured only when the leaf does not fit without them: one
  // scan of the rows gives the leaf's facts and, below, each candidate half's.
  const sizer = shape.sizer;
  const memo = shape.memo ?? null;
  let scan: unknown = null;
  let facts: unknown;
  if (pay > cap.c && n >= 2) {
    pay -= leafPrefixSaving(n, keys[0]!, keys[n - 1]!);
    if (sizer !== null) {
      if (group !== null) {
        scan = sizer.scanDistinct(keys, vals, cap, memo);
      } else {
        scan = sizer.scan(keys, vals, cap, memo);
        // A leaf with no repeated payload starts a group.
        if (!sizer.scanDups(scan)) group = {};
      }
      facts = sizer.scanFacts(scan, vals, 0, n, group);
      pay -= sizer.saving(facts, n);
    }
  }
  if (pay <= cap.c || n < 2) {
    const whole = newLeaf(keys, vals, weights, owner);
    // A dirty node is its writer's alone, so it may keep the facts.
    if (facts !== undefined && owner !== undefined) whole.sizes = { n, facts };
    return { whole };
  }

  const prefix = new Array<number>(n + 1);
  prefix[0] = 0;
  for (let i = 0; i < n; i++) prefix[i + 1] = prefix[i]! + weights[i]!;
  let fast: number | null | undefined;
  let fastOut: InsOut | null = null;
  if (sizer !== null) {
    // The split point is found with running facts, visiting only the rows the rule looks at. When
    // no two halves fit, the tables below serve the multi-way split.
    fast = splitPointRunning(
      keys,
      vals,
      scan,
      prefix,
      pay,
      cap,
      shape,
      sizer,
      edited === n - 1,
      false,
    );
    if (fast !== null) {
      const left = newLeaf(keys.slice(0, fast), vals.slice(0, fast), weights.slice(0, fast), owner);
      const right = newLeaf(keys.slice(fast), vals.slice(fast), weights.slice(fast), owner);
      if (owner !== undefined && !sizer.scanDups(scan)) {
        // No payload repeats in the leaf, so none does in a half: each half's facts are counted
        // without a lookup, and its writer keeps them. The halves are one distinct group: a later
        // merge of the two hashes nothing.
        left.sizes = { n: fast, facts: sizer.scanFacts(scan, vals, 0, fast, group) };
        right.sizes = { n: n - fast, facts: sizer.scanFacts(scan, vals, fast, n, group) };
      }
      fastOut = { whole: null, left, sep: keys[fast]!, right, more: [] };
      if (!leafSizesCheck) return fastOut;
      // Under the check, returned once the tabled rule below agrees.
    }
  }
  // Each candidate half is measured with its own key prefix, bases, widths and dictionaries.
  const bounds = sizer === null ? null : sizer.boundarySavings(keys, vals, cap, memo, scan);
  if (sizer !== null && bounds !== null && sizer.saving(facts, n) !== bounds.pre[n]) {
    throw new Error("BUG: a scanned leaf's facts do not save what its tabled ones do");
  }
  // Both stay monotone in m: a record added to a half adds at least its stored bytes, the half's
  // key prefix can only shorten, an offset width only widen, and a region's size is the smaller of
  // nondecreasing sizes (format.md "Leaf split").
  const leftpayload = (m: number): number =>
    prefix[m]! +
    leafOverhead(m, shape) -
    leafPrefixSaving(m, keys[0]!, keys[m - 1]!) -
    (bounds === null ? 0 : bounds.pre[m]!);
  const rightpayload = (m: number): number =>
    total -
    prefix[m]! +
    leafOverhead(n - m, shape) -
    leafPrefixSaving(n - m, keys[m]!, keys[n - 1]!) -
    (bounds === null ? 0 : bounds.suf[m]!);
  const m = splitPoint(1, n - 1, pay, cap.c, edited === n - 1, leftpayload, rightpayload);
  if (fast !== undefined && fast !== m) {
    throw new Error(`BUG: the running split point is ${fast}, the tabled one ${m}`);
  }
  if (fastOut !== null) return fastOut;
  if (m === null) {
    // No two fitting halves (format.md "Multi-way split"): one changed record can widen a whole
    // leaf's encodings. The left leaf takes the largest fitting run and the rest splits again.
    let mMax = 0;
    for (let mm = 1; mm <= n - 1 && leftpayload(mm) <= cap.c; mm++) mMax = mm;
    // A single record that does not fit: leave the node whole (0A000 at serialize).
    if (mMax === 0) return { whole: newLeaf(keys, vals, weights, owner) };
    const left = newLeaf(keys.slice(0, mMax), vals.slice(0, mMax), weights.slice(0, mMax), owner);
    const rest = buildLeaf(
      keys.slice(mMax),
      vals.slice(mMax),
      weights.slice(mMax),
      cap,
      shape,
      edited === null ? null : edited - mMax,
      owner,
    );
    return joinSplit(left, keys[mMax]!, rest);
  }
  return {
    whole: null,
    left: newLeaf(keys.slice(0, m), vals.slice(0, m), weights.slice(0, m), owner),
    sep: keys[m]!,
    right: newLeaf(keys.slice(m), vals.slice(m), weights.slice(m), owner),
    more: [],
  };
}

// buildInterior builds an interior node from its parts; if its payload overflows cap, it splits
// 2-way PUSH-UP (format.md "Interior split"): the left node keeps separators [0, m) + children
// [0, m], separator m moves up, the right node keeps [m+1, N) + children [m+1, N]. With N = 2
// (only reachable with near-cap separators) the split is pinned to m = 1, producing a legal N = 0
// right interior (the degenerate fan-out contract). Returns null when the node overflows and no
// valid two-way split point exists and multi is false — the caller (the interior MERGE path)
// abandons the merge. With multi (the insert path) it splits multi-way instead (format.md
// "Multi-way split") and never returns null.
function buildInterior(
  keys: Uint8Array[],
  children: Child[],
  cap: Cap,
  edited: number | null,
  owner: MutationGeneration | undefined,
  multi: boolean,
): InsOut | null {
  const n = keys.length;
  const pay = interiorPayload(cap, keys);
  if (pay <= cap.c || n < 2) return { whole: newInterior(keys, children, owner) };

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
    const ptr = capPtr(cap);
    const leftpayload = (mm: number): number => (ptr + 4) * mm + ptr + prefix[mm]!;
    const rightpayload = (mm: number): number =>
      (ptr + 4) * (n - 1 - mm) + ptr + (total - prefix[mm + 1]!);
    const got = splitPoint(1, n - 2, pay, cap.c, edited === n - 1, leftpayload, rightpayload);
    if (got === null) {
      if (!multi) return null;
      // Multi-way: the left node takes the largest fitting run, its next separator moves up, and
      // the rest splits again.
      let mMax = 0;
      for (let mm = 1; mm <= n - 2 && leftpayload(mm) <= cap.c; mm++) mMax = mm;
      // One separator that does not fit: leave the node whole (0A000 at serialize).
      if (mMax === 0) return { whole: newInterior(keys, children, owner) };
      const left = newInterior(keys.slice(0, mMax), children.slice(0, mMax + 1), owner);
      const rest = buildInterior(
        keys.slice(mMax + 1),
        children.slice(mMax + 1),
        cap,
        edited === null ? null : edited - mMax - 1,
        owner,
        true,
      );
      if (rest === null) return null;
      return joinSplit(left, keys[mMax]!, rest);
    }
    m = got;
  }

  return {
    whole: null,
    left: newInterior(keys.slice(0, m), children.slice(0, m + 1), owner),
    sep: keys[m]!,
    right: newInterior(keys.slice(m + 1), children.slice(m + 1), owner),
    more: [],
  };
}

// LEAF_EVEN_ROOM sets the room a sibling needs to take part in evening out: C / LEAF_EVEN_ROOM bytes
// free (format.md "Evening out with a sibling").
const LEAF_EVEN_ROOM = 8;

type Evened = { j: number; left: PNode; sep: Uint8Array; right: PNode };

// evenLeaves tries to even the overflowing leaf child i of n out with an adjacent sibling instead
// of splitting it (format.md "Evening out with a sibling"). sub is the leaf's own two-way split,
// key the key just written. The right sibling is tried, then the left one; an edit at the leaf's
// right edge tries the right one only, so an append to the tree's last leaf splits as it always did
// and reads no other leaf. A sibling takes part when its payload is at most C - C / LEAF_EVEN_ROOM;
// the two leaves' records are then split in two by the balanced rule, as the delete path's
// merge-overflow does (mergeAt). A candidate fails when the pair has no two-way split that fits —
// it would have to go multi-way, which the parent's key count cannot absorb here — or when the new
// separator would overflow n (mergeAt's second abandon rule).
//
// The returned halves and separator replace children j and j+1 and separator j of n, which keeps
// its key count; null means the leaf must split. n is not modified here.
function* evenLeavesSteps(
  n: PNode,
  i: number,
  sub: { left: PNode; right: PNode },
  key: Uint8Array,
  generation: MutationGeneration,
  src: LeafSource | null,
  cap: Cap,
  shape: LeafShape,
): Steps<Evened | null> {
  const own = sub.right.keys;
  const rightEdge = compareBytes(own[own.length - 1]!, key) === 0;
  const limit = cap.c - Math.floor(cap.c / LEAF_EVEN_ROOM);
  for (const s of [i + 1, i - 1]) {
    if (s < 0 || s >= n.children.length || (s < i && rightEdge)) continue;
    const sib = yield* resolveChildSteps(n.children[s], src);
    if (!payloadAtMost(sib, limit, cap, shape)) continue;
    const p = leafParts(sib);
    const l = sub.left;
    const r = sub.right;
    // The pair overflows, because the leaf alone does: a whole result means a single record that
    // fits no leaf, and a multi-way one that no two halves fit. Either way the leaf splits.
    const merged =
      s < i
        ? buildLeaf(
            p.keys.concat(l.keys, r.keys),
            p.vals.concat(l.vals, r.vals),
            p.weights.concat(l.weights, r.weights),
            cap,
            shape,
            null,
            generation,
          )
        : buildLeaf(
            l.keys.concat(r.keys, p.keys),
            l.vals.concat(r.vals, p.vals),
            l.weights.concat(r.weights, p.weights),
            cap,
            shape,
            null,
            generation,
          );
    const j = Math.min(i, s);
    if (
      merged.whole !== null ||
      merged.more.length > 0 ||
      interiorPayload(cap, n.keys) - n.keys[j]!.length + merged.sep.length > cap.c
    ) {
      continue;
    }
    return { j, left: merged.left, sep: merged.sep, right: merged.right };
  }
  return null;
}

// nodeInsert is the recursive insert. It descends to the holding LEAF (interior nodes only route,
// via childSlot); on overwrite it sets ctx and rebuilds with the value+weight replaced (which may
// now overflow). Splits propagate back up: a leaf split copies its boundary key up, an interior
// receiving a separator may push-split in turn.
function* nodeInsertSteps(
  node: PNode,
  key: Uint8Array,
  val: Row,
  weight: number,
  ctx: InsCtx,
  generation: MutationGeneration,
  src: LeafSource | null,
  cap: Cap,
  shape: LeafShape,
): Steps<InsOut> {
  let n = node;
  if (isLeaf(n)) {
    const { index, found } = search(n, key);
    if (lazyCopies(n, generation, shape)) {
      // A Packed leaf, or a lazy one an earlier generation built, is copied without decoding its
      // rows, and the copy is edited in place below.
      n = lazyCopy(n, generation, shape.sizer);
    }
    // Only this active generation may own a dirty node. Clone/cursor/publication invalidates the
    // token before exposing an alias; clean and old-generation nodes retain the path-copy fallback.
    if (n.page === 0 && n[mutationOwner] === generation && generation.active) {
      trackWeight(n);
      if (found) {
        ctx.replaced = true;
        if (n.lazyBase !== undefined) {
          if (ctx.want) ctx.old = rowAt(n, index);
          replaceRow(n, index, key, val, weight, cap, shape);
        } else {
          ctx.old = n.vals[index];
          const sz = n.sizes;
          if (sz !== undefined) {
            // The facts follow the row: the old one out, the new one in.
            const sizer = shape.sizer;
            const memo = shape.memo ?? null;
            const exact = sizer !== null && sz.n === n.keys.length;
            // Facts with a dictionary kept as distinct cannot take a row: they are dropped, and the
            // leaf is measured again only if its plain size no longer decides.
            if (
              exact &&
              !sizer.factsDistinct(sz.facts) &&
              sizer.removeRow(sz.facts, key, n.vals[index]!, cap, memo)
            ) {
              sizer.addRow(sz.facts, key, val, cap, memo);
            } else {
              n.sizes = undefined;
            }
          }
          n.vals[index] = val;
          if (n.wsum !== undefined) n.wsum += weight - n.weights[index]!;
          n.weights[index] = weight;
        }
      } else {
        const kept = n.sizes;
        if (
          kept !== undefined &&
          shape.sizer !== null &&
          kept.n === n.keys.length &&
          n.lazyBase === undefined
        ) {
          // Facts with a dictionary kept as distinct cannot take a row. (A lazy leaf's facts come
          // from its page and never hold one.)
          if (shape.sizer.factsDistinct(kept.facts)) n.sizes = undefined;
        }
        n.keys.splice(index, 0, key);
        n.vals.splice(index, 0, val);
        n.weights.splice(index, 0, weight);
        if (n.wsum !== undefined) n.wsum += weight;
        if (n.lazyBase !== undefined) {
          n.lazy!.splice(index, 0, -1);
          rowSetChanged(n, shape.sizer);
          if (n.patchOK && !shape.edits!.rowAllInline(key, val, cap)) n.patchOK = false;
        }
        const sz = n.sizes;
        if (sz !== undefined && shape.sizer !== null && sz.n === n.keys.length - 1) {
          shape.sizer.addRow(sz.facts, key, val, cap, shape.memo ?? null);
          sz.n++;
        } else {
          n.sizes = undefined;
        }
      }
      if (payloadAtMost(n, cap.c, cap, shape) || nodeLen(n) < 2) return { whole: n };
      // A split needs every row.
      if (n.lazyBase !== undefined) materialize(n);
      // The unchanged builder selects the exact split point/separator. This edited node is
      // abandoned when its returned halves replace it.
      return buildLeaf(n.keys, n.vals, n.weights, cap, shape, index, generation);
    }
    const { keys, vals, weights } = decodedParts(n);
    if (found) {
      ctx.old = vals[index];
      ctx.replaced = true;
      vals[index] = val;
      weights[index] = weight;
      return buildLeaf(keys, vals, weights, cap, shape, index, generation);
    }
    return buildLeaf(
      insertAt(keys, index, key),
      insertAt(vals, index, val),
      insertAt(weights, index, weight),
      cap,
      shape,
      index,
      generation,
    );
  }
  // Fault the target child (a resident interior, or an OnDisk leaf brought in for mutation — it
  // becomes a dirty resident node on the rebuilt path).
  const i = childSlot(n, key);
  const sub = yield* nodeInsertSteps(
    yield* resolveChildSteps(n.children[i], src),
    key,
    val,
    weight,
    ctx,
    generation,
    src,
    cap,
    shape,
  );
  if (n.page === 0 && n[mutationOwner] === generation && generation.active) {
    if (sub.whole !== null) {
      n.children[i] = residentRef(sub.whole);
      return { whole: n };
    }
    if (isLeaf(sub.left) && sub.more.length === 0) {
      // Even the leaf out with a sibling before accepting its two-way split.
      const ev = yield* evenLeavesSteps(n, i, sub, key, generation, src, cap, shape);
      if (ev !== null) {
        n.keys[ev.j] = ev.sep;
        n.sameKeys = false;
        n.children[ev.j] = residentRef(ev.left);
        n.children[ev.j + 1] = residentRef(ev.right);
        return { whole: n };
      }
    }
    const edited = absorbSplit(n.keys, n.children, i, sub);
    n.sameKeys = false;
    if (payload(n, cap, shape) <= cap.c || n.keys.length < 2) return { whole: n };
    const out = buildInterior(n.keys, n.children, cap, edited, generation, true);
    if (out === null) throw new Error("insert-path interior split always has a valid split point");
    return out;
  }
  if (sub.whole !== null) {
    // This node's separators are unchanged, so it cannot overflow — rebuild whole.
    const children = n.children.slice();
    children[i] = residentRef(sub.whole);
    return { whole: copyInterior(n, n.keys.slice(), children, true, generation) };
  }
  if (isLeaf(sub.left) && sub.more.length === 0) {
    const ev = yield* evenLeavesSteps(n, i, sub, key, generation, src, cap, shape);
    if (ev !== null) {
      const keys = n.keys.slice();
      keys[ev.j] = ev.sep;
      const children = n.children.slice();
      children[ev.j] = residentRef(ev.left);
      children[ev.j + 1] = residentRef(ev.right);
      return { whole: copyInterior(n, keys, children, false, generation) };
    }
  }
  const keys = n.keys.slice();
  const children = n.children.slice();
  const edited = absorbSplit(keys, children, i, sub);
  const out = buildInterior(keys, children, cap, edited, generation, true);
  if (out === null) {
    // The cap arithmetic guarantees a valid split point on the insert path (format.md "Why the
    // record cap") — reaching here would be an internal invariant break, not a data condition.
    throw new Error("insert-path interior split always has a valid split point");
  }
  // A rebuild that did not split still replaces this node one for one, with new separators.
  if (out.whole !== null) {
    const { origin } = originOf(n);
    if (origin !== undefined) out.whole.origin = origin;
  }
  return out;
}

type RemOut = { ok: boolean; node: PNode; removed: Row | undefined };

// nodeRemove is the recursive delete. It descends to the holding LEAF (a separator equal to the key
// just routes right — it is never itself deleted or replaced; separators may go stale, format.md
// "Delete"). Returns the rebuilt subtree (possibly underfull — the caller rebalances it) and the
// removed row; the touched child is rebalanced via rebalanceChild. A dirty node the active
// generation owns is edited in place, as nodeInsert does; any other node is path-copied into a new
// one the generation owns, so a later remove in the same statement reuses it.
function* nodeRemoveSteps(
  node: PNode,
  key: Uint8Array,
  want: boolean,
  generation: MutationGeneration,
  src: LeafSource | null,
  cap: Cap,
  shape: LeafShape,
): Steps<RemOut> {
  let n = node;
  if (isLeaf(n)) {
    const { index, found } = search(n, key);
    if (!found) return { ok: false, node: n, removed: undefined };
    if (lazyCopies(n, generation, shape)) n = lazyCopy(n, generation, shape.sizer); // see nodeInsertSteps
    if (ownedBy(n, generation)) {
      trackWeight(n);
      if (n.lazyBase !== undefined) {
        const sizer = shape.sizer;
        const sz = n.sizes;
        // Kept facts follow the row out; frozen ones hold only for the page's rows.
        const exact =
          sz !== undefined &&
          sizer !== null &&
          sz.n === n.keys.length &&
          sizer.frozen(sz.facts) === 0;
        const removed = want || exact ? rowAt(n, index) : undefined;
        if (sz !== undefined) {
          if (exact && sizer!.removeRow(sz.facts, key, removed!, cap, shape.memo ?? null)) sz.n--;
          else n.sizes = undefined;
        }
        n.lazy!.splice(index, 1);
        rowSetChanged(n, sizer);
        n.wsum! -= n.weights[index]!;
        n.keys.splice(index, 1);
        n.vals.splice(index, 1);
        n.weights.splice(index, 1);
        return { ok: true, node: n, removed };
      }
      const removed = n.vals[index];
      if (n.wsum !== undefined) n.wsum -= n.weights[index]!;
      const sz = n.sizes;
      if (sz !== undefined) {
        const sizer = shape.sizer;
        if (
          sizer !== null &&
          sz.n === n.keys.length &&
          sizer.removeRow(sz.facts, key, n.vals[index]!, cap, shape.memo ?? null)
        ) {
          sz.n--;
        } else {
          n.sizes = undefined;
        }
      }
      n.keys.splice(index, 1);
      n.vals.splice(index, 1);
      n.weights.splice(index, 1);
      return { ok: true, node: n, removed };
    }
    // decodedParts returns fresh arrays, so the copy is edited directly.
    const { keys, vals: rows, weights } = decodedParts(n);
    const removed = rows[index];
    keys.splice(index, 1);
    rows.splice(index, 1);
    weights.splice(index, 1);
    return { ok: true, node: newLeaf(keys, rows, weights, generation), removed };
  }
  const i = childSlot(n, key);
  const res = yield* nodeRemoveSteps(
    yield* resolveChildSteps(n.children[i], src),
    key,
    want,
    generation,
    src,
    cap,
    shape,
  );
  if (!res.ok) return { ok: false, node: n, removed: undefined };
  let rebuilt = n;
  if (ownedBy(n, generation)) {
    n.children[i] = residentRef(res.node);
  } else {
    const children = n.children.slice();
    children[i] = residentRef(res.node);
    rebuilt = copyInterior(n, n.keys.slice(), children, true, generation);
  }
  return {
    ok: true,
    node: yield* rebalanceChildSteps(rebuilt, i, generation, src, cap, shape),
    removed: res.removed,
  };
}

// lazyCopies reports whether an edit of leaf n starts from a lazy copy: the lazy paths are on, the
// tree has the format layer's part, and n is a Packed leaf or a lazy leaf this generation does not own.
function lazyCopies(n: PNode, generation: MutationGeneration, shape: LeafShape): boolean {
  return (
    leafLazy &&
    shape.edits !== undefined &&
    (n.packed !== undefined || n.lazyBase !== undefined) &&
    !ownedBy(n, generation)
  );
}

// ownedBy reports whether n is a dirty node generation may edit in place: only the active generation
// that built it, which a clone, cursor pin, or publication invalidates before exposing an alias.
function ownedBy(n: PNode, generation: MutationGeneration): boolean {
  return n.page === 0 && n[mutationOwner] === generation && generation.active;
}

// rebalanceChild: if children[i] is underfull (payload < cap/2), merge it with an adjacent sibling
// (prefer the right one), then split the merged node back if it overflows — the unified rebalance
// (no borrow). The returned parent may itself have lost a key and become underfull; its own parent
// handles that as the recursion unwinds.
function* rebalanceChildSteps(
  n: PNode,
  i: number,
  generation: MutationGeneration,
  src: LeafSource | null,
  cap: Cap,
  shape: LeafShape,
): Steps<PNode> {
  // children[i] was just rebuilt resident by nodeRemove, so inspecting it faults nothing.
  const child = yield* resolveChildSteps(n.children[i], src);
  // Underfull is payload < C/2 (format.md "Delete").
  const half = Math.ceil(cap.c / 2);
  if (
    isLeaf(child) ? !payloadAtMost(child, half - 1, cap, shape) : payload(child, cap, shape) >= half
  )
    return n;
  if (n.children.length < 2) {
    // A 0-key interior (one child, the degenerate max-separator shape) has no sibling to merge
    // with — its own parent merges IT away; the root case collapses in PMap.remove.
    return n;
  }
  if (!isLeaf(child) && emptyUnarySubtree(child)) {
    const parent = ownedBy(n, generation)
      ? n
      : copyInterior(n, n.keys.slice(), n.children.slice(), false, generation);
    parent.sameKeys = false;
    parent.keys.splice(Math.min(i, parent.keys.length - 1), 1);
    parent.children.splice(i, 1);
    return parent;
  }
  const j = i + 1 < n.children.length ? i : i - 1;
  return yield* mergeAtSteps(n, j, generation, src, cap, shape);
}

// An empty leaf below zero-key interiors has no same-parent sibling. Its first
// branching ancestor removes the empty unary subtree; an ordinary interior
// merge could split again and preserve the empty descendant leaf.
function emptyUnarySubtree(node: PNode): boolean {
  while (node.children.length === 1 && node.keys.length === 0) {
    const child = node.children[0].node;
    if (child === null) return false;
    node = child;
  }
  return isLeaf(node) && nodeLen(node) === 0;
}

// sharedGroup is the distinct group two leaves both belong to with exact facts, or null.
function sharedGroup(left: PNode, right: PNode, sizer: LeafSizer | null): object | null {
  const ls = left.sizes;
  const rs = right.sizes;
  if (sizer === null || ls === undefined || rs === undefined) return null;
  const group = sizer.factsGroup(ls.facts);
  if (group === null || group !== sizer.factsGroup(rs.facts)) return null;
  if (ls.n !== nodeLen(left) || rs.n !== nodeLen(right)) return null;
  if (sizer.frozen(ls.facts) !== 0 || sizer.frozen(rs.facts) !== 0) return null;
  return group;
}

// mergeAt merges children[j] and children[j+1] into one node M (format.md "Delete"): a LEAF merge
// concatenates the two record lists and the parent separator j is REMOVED (it was a routing copy —
// nothing comes down); an INTERIOR merge PULLS the separator DOWN between the two key lists (the
// merged children need a routing key between them). If M fits, it replaces the pair (the parent
// loses one key); if it overflows, it is split 2-way by the balanced rule and the halves + the new
// separator replace the pair (the parent's key count is unchanged). The merge is ABANDONED — the
// parent is returned unchanged, the underfull child tolerated — in two deterministic cases
// (format.md "Delete", the abandon rules): an INTERIOR M that overflows but admits no valid split
// (near-cap separators), and a split whose new separator is longer than the parent separator it
// would replace by enough to overflow the PARENT (the key count is unchanged, the byte size is not;
// nothing on the delete path splits an interior, so an over-cap parent would be unserializable).
function* mergeAtSteps(
  n: PNode,
  j: number,
  generation: MutationGeneration,
  src: LeafSource | null,
  cap: Cap,
  shape: LeafShape,
): Steps<PNode> {
  // Fault both children — the underfull child (just rebuilt resident) and its sibling, which may
  // still be an OnDisk leaf the delete never touched.
  const left = yield* resolveChildSteps(n.children[j], src);
  const right = yield* resolveChildSteps(n.children[j + 1], src);

  let merged: InsOut;
  if (isLeaf(left)) {
    // Read both leaves (either may be Packed) and copy each record once, straight into the merged
    // node or its two halves (buildLeaf's split rule; merge-overflow splits balanced). The two
    // inputs are left untouched, so an abandoned merge below keeps them exactly as they were.
    const l = leafParts(left);
    const r = leafParts(right);
    // A leaf's size depends on its keys and rows, so the split decision needs the merged lists:
    // buildLeaf decides it.
    const total = l.keys.length + r.keys.length;
    merged = buildLeaf(
      concatRange(l.keys, r.keys, 0, total),
      concatRange(l.vals, r.vals, 0, total),
      l.weights.concat(r.weights),
      cap,
      shape,
      null,
      generation,
      sharedGroup(left, right, shape.sizer),
    );
    // Unreachable: splitting between the two leaves gives two fitting halves. Abandon.
    if (merged.whole === null && merged.more.length > 0) return n;
  } else {
    const mkeys = [...left.keys, n.keys[j]!, ...right.keys];
    const mchildren = [...left.children, ...right.children];
    const out = buildInterior(mkeys, mchildren, cap, null, generation, false);
    // No valid 2-way split point (near-cap separators): abandon the merge — the two children and
    // the parent separator stay exactly as they were (underfull tolerated).
    if (out === null) return n;
    merged = out;
  }

  if (
    merged.whole === null &&
    interiorPayload(cap, n.keys) - n.keys[j]!.length + merged.sep.length > cap.c
  ) {
    // The replacement separator would overflow the parent: abandon the merge — the two children and
    // the parent separator stay exactly as they were (underfull tolerated).
    return n;
  }

  const parent = ownedBy(n, generation)
    ? n
    : copyInterior(n, n.keys.slice(), n.children.slice(), false, generation);
  parent.sameKeys = false; // past every abandon rule: the parent's separators change
  if (merged.whole !== null) {
    parent.keys.splice(j, 1);
    parent.children[j] = residentRef(merged.whole);
    parent.children.splice(j + 1, 1);
    return parent;
  }
  parent.keys[j] = merged.sep;
  parent.children[j] = residentRef(merged.left);
  parent.children[j + 1] = residentRef(merged.right);
  return parent;
}

// --- immutable array helpers (each returns a fresh array, leaving the input untouched) -------

function insertAt<T>(a: T[], i: number, x: T): T[] {
  const out = a.slice();
  out.splice(i, 0, x);
  return out;
}
