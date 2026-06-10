// In-memory storage seam (CLAUDE.md §9). A table's rows are held in a PMap — a persistent
// (copy-on-write) ordered map keyed by the primary-key encoding (spec/design/encoding.md), so
// iteration is in key order (the order-preserving encoding makes that the correct logical order
// with no comparator) and the whole store is an O(1) clone that snapshots independently of its
// source. That cheap, structurally-shared clone is what carries the §3 staging-buffer /
// transaction model (spec/design/transactions.md §2): a TableStore clone is the committed
// version a reader holds while a writer mutates its own copy.
//
// Since Phase 6 (P6.1) the PMap is the page-backed B-tree, so the store carries the page payload
// cap (= page_size − 12) and the column types to weigh each record (recordSize) for the
// size-driven split (spec/fileformat/format.md).

import type { Value } from "./value.ts";
import type { ScalarType } from "./types.ts";
import { PMap, pmapFromLoaded } from "./pmap.ts";
import type { KeyBound, LeafSource, PNode } from "./pmap.ts";
import type { SharedPaging } from "./paging.ts";
import { recordSize } from "./format.ts";

// Row is a stored row: one value per column, in column order.
export type Row = Value[];

// Entry is one stored (encoded key, row) pair.
export type Entry = { key: Uint8Array; row: Row };

// PagedSource is the buffer-pool leaf source for one store (spec/design/pager.md §4): faults a clean
// leaf page through this database's shared pool, decoding it with this table's column types. A store
// with no paging (in-memory) builds none (null) and never faults.
class PagedSource implements LeafSource {
  private paging: SharedPaging;
  private colTypes: ScalarType[];

  constructor(paging: SharedPaging, colTypes: ScalarType[]) {
    this.paging = paging;
    this.colTypes = colTypes;
  }

  loadLeaf(page: number): PNode {
    return this.paging.faultLeaf(page, this.colTypes);
  }
}

// TableStore holds one table's rows, keyed by encoded primary key.
export class TableStore {
  private rows: PMap;
  // nextRowid is the next synthetic rowid for a table with no primary key. Monotonic —
  // never reused, so a DELETE-then-INSERT cannot collide with a freed key. Unused for
  // tables with a primary key. Reconstructed on load (spec/fileformat).
  private nextRowid: bigint;
  // cap is the page payload capacity C = page_size − 12 (the split threshold). colTypes are the
  // column types, for computing record weights (recordSize).
  private cap: number;
  private colTypes: ScalarType[];
  // paging is the shared pager + leaf buffer pool for a file-backed database (spec/design/pager.md):
  // the read/mutation path faults OnDisk leaves through it. null for an in-memory database and for a
  // table created in-session (fully resident until the file is reopened); attached by the demand-paged
  // file load. Shared (reference) — a snapshot clone shares the one pool per database.
  private paging: SharedPaging | null;

  constructor(
    cap: number,
    colTypes: ScalarType[],
    rows: PMap = new PMap(),
    nextRowid = 0n,
    paging: SharedPaging | null = null,
  ) {
    this.cap = cap;
    this.colTypes = colTypes;
    this.rows = rows;
    this.nextRowid = nextRowid;
    this.paging = paging;
  }

  // clone returns an independent O(1) snapshot of the store: the PMap clone shares structure
  // (nodes are immutable), so mutating one store leaves the clone untouched. The foundation of
  // the transaction model (spec/design/transactions.md §2). The shared paging context is shared, not
  // copied (one pool per database).
  clone(): TableStore {
    return new TableStore(this.cap, this.colTypes, this.rows.clone(), this.nextRowid, this.paging);
  }

  // attachPaging attaches this database's shared paging context (the demand-paged file load,
  // format.ts): the store's OnDisk leaves now fault through the pool. One pool per database.
  attachPaging(paging: SharedPaging): void {
    this.paging = paging;
  }

  // leafSrc builds this store's leaf source, or null for an in-memory store that never faults.
  private leafSrc(): LeafSource | null {
    return this.paging === null ? null : new PagedSource(this.paging, this.colTypes);
  }

  // weight is this row's on-disk record size — the weight the page-backed B-tree splits on. Accounts
  // for out-of-line spill at cap (an externalized value weighs its pointer, not its full body —
  // large-values.md §12), so split points match the serialized pages.
  private weight(key: Uint8Array, row: Row): number {
    return recordSize(this.colTypes, key, row, this.cap);
  }

  // insert adds a row under its encoded key. Returns false if the key already exists
  // (primary-key uniqueness); the caller decides how to surface that. May fault the target leaf
  // through the buffer pool (an I/O error then throws).
  insert(key: Uint8Array, row: Row): boolean {
    const src = this.leafSrc();
    if (this.rows.get(key, src) !== undefined) return false;
    this.rows.insert(key, row, this.weight(key, row), this.cap, src);
    return true;
  }

  // allocRowid returns the next monotonic rowid (for a table with no primary key) and
  // advances the counter. Never returns a previously-issued value.
  allocRowid(): bigint {
    const r = this.nextRowid;
    this.nextRowid++;
    return r;
  }

  // bumpRowidTo ensures the rowid counter is at least n (used on load to set it past
  // every rowid already present, so future inserts don't collide).
  bumpRowidTo(n: bigint): void {
    if (n > this.nextRowid) this.nextRowid = n;
  }

  // replace overwrites the row stored at an existing key (UPDATE). The key is
  // unchanged, so key order and the rowid counter are untouched. May fault the target leaf.
  replace(key: Uint8Array, row: Row): void {
    this.rows.insert(key, row, this.weight(key, row), this.cap, this.leafSrc());
  }

  // remove deletes the row at key (DELETE). Returns whether a row was present. May fault leaves the
  // delete descends into / rebalances against.
  remove(key: Uint8Array): boolean {
    return this.rows.remove(key, this.cap, this.leafSrc()) !== undefined;
  }

  // get looks up a row by its exact encoded key. May fault the holding leaf through the buffer pool.
  get(key: Uint8Array): Row | undefined {
    return this.rows.get(key, this.leafSrc());
  }

  // iterInKeyOrder returns the rows in primary-key (encoded byte) order. Eager: leaves fault through
  // the pool during the walk and are dropped (GC) as their rows are collected, so the resident leaf
  // set stays bounded by the pool, not the table (spec/design/pager.md §4).
  iterInKeyOrder(): Row[] {
    return this.rows.inorder(this.leafSrc()).vals;
  }

  // nodeCount is the number of B-tree nodes (pages) in this store — the page_read count a full
  // scan charges (spec/design/cost.md §3 "page_read"). 0 for an empty table.
  nodeCount(): number {
    return this.rows.nodeCount();
  }

  // entriesInKeyOrder returns all (key, row) pairs in encoded-key order. Used by the
  // on-disk serializer, which stores each row's key verbatim (the key is not always
  // reconstructable from the row — e.g. a no-PK table's synthetic rowid).
  entriesInKeyOrder(): Entry[] {
    const { keys, vals } = this.rows.inorder(this.leafSrc());
    return keys.map((k, i) => ({ key: k, row: vals[i] }));
  }

  // rangeRows returns the rows whose primary key lies within the bound, in key order — a bounded
  // B-tree scan that faults only the leaves the bound spans (spec/design/cost.md §3 "bounded scan").
  rangeRows(b: KeyBound): Row[] {
    return this.rows.rangeEntries(b, this.leafSrc()).vals;
  }

  // rangeEntries returns the (key, row) pairs whose primary key lies within the bound, in key order
  // (the mutation paths need the keys to remove/replace).
  rangeEntries(b: KeyBound): Entry[] {
    const { keys, vals } = this.rows.rangeEntries(b, this.leafSrc());
    return keys.map((k, i) => ({ key: k, row: vals[i] }));
  }

  // overlapNodeCount is the number of B-tree nodes a bounded scan over b visits — the page_read it
  // charges (cost.md §3). Equals nodeCount for the unbounded bound.
  overlapNodeCount(b: KeyBound): number {
    return this.rows.overlapNodeCount(b);
  }

  // scanRange streams the rows whose primary key lies within b to visit, in key order, stopping
  // (without faulting further leaves) the moment visit returns false — the genuine LIMIT short-circuit
  // (spec/design/cost.md §3 "LIMIT short-circuit").
  scanRange(b: KeyBound, visit: (key: Uint8Array, row: Row) => boolean): void {
    this.rows.scanRange(b, this.leafSrc(), visit);
  }

  // treeRoot is the root B-tree node of this store, for the page-backed serializer
  // (spec/fileformat/format.md). null for an empty table.
  treeRoot(): PNode | null {
    return this.rows.rootNode();
  }

  // setTree installs a loaded B-tree as this store's contents (format.ts loadDatabase).
  setTree(root: PNode | null, length: number): void {
    this.rows = pmapFromLoaded(root, length);
  }

  // len returns the row count.
  len(): number {
    return this.rows.size;
  }
}
