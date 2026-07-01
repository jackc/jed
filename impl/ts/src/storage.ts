// In-memory storage seam (CLAUDE.md §9). A table's rows are held in a PMap — a persistent
// (copy-on-write) ordered map keyed by the primary-key encoding (spec/design/encoding.md), so
// iteration is in key order (the order-preserving encoding makes that the correct logical order
// with no comparator) and the whole store is an O(1) clone that snapshots independently of its
// source. That cheap, structurally-shared clone is what carries the §3 staging-buffer /
// transaction model (spec/design/transactions.md §2): a TableStore clone is the committed
// version a reader holds while a writer mutates its own copy.
//
// Since Phase 6 (P6.1) the PMap is the page-backed B-tree, so the store carries the page payload
// cap (= page_size − 16) and the column types to weigh each record (recordSize) for the
// size-driven split (spec/fileformat/format.md).

import type { Value } from "./value.ts";
import type { ColType } from "./catalog.ts";
import { PMap, pmapFromLoaded, unboundedBound } from "./pmap.ts";
import type { KeyBound, LeafSource, PNode } from "./pmap.ts";
import type { SharedPaging } from "./paging.ts";
import {
  resolveUnfetched,
  anySpillable,
  anySpillableMasked,
  recordCompressUnits,
  recordScanUnits,
  recordSize,
} from "./format.ts";

// Row is a stored row: one value per column, in column order.
export type Row = Value[];

// Entry is one stored (encoded key, row) pair.
export type Entry = { key: Uint8Array; row: Row };

// PagedSource is the buffer-pool leaf source for one store (spec/design/pager.md §4): faults a clean
// leaf page through this database's shared pool, decoding it with this table's column types. A store
// with no paging (in-memory) builds none (null) and never faults.
class PagedSource implements LeafSource {
  private paging: SharedPaging;
  private colTypes: ColType[];

  constructor(paging: SharedPaging, colTypes: ColType[]) {
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
  // cap is the page payload capacity C = page_size − 16 (the split threshold). colTypes are the
  // column types, for computing record weights (recordSize).
  private cap: number;
  private colTypes: ColType[];
  // paging is the shared pager + leaf buffer pool for a file-backed database (spec/design/pager.md):
  // the read/mutation path faults OnDisk leaves through it. null for an in-memory database and for a
  // table created in-session (fully resident until the file is reopened); attached by the demand-paged
  // file load. Shared (reference) — a snapshot clone shares the one pool per database.
  private paging: SharedPaging | null;

  constructor(
    cap: number,
    colTypes: ColType[],
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
    this.rows.insert(key, row, this.weight(key, row), this.cap, this.colTypes.length, src);
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
    this.rows.insert(
      key,
      row,
      this.weight(key, row),
      this.cap,
      this.colTypes.length,
      this.leafSrc(),
    );
  }

  // remove deletes the row at key (DELETE). Returns whether a row was present. May fault leaves the
  // delete descends into / rebalances against.
  remove(key: Uint8Array): boolean {
    return this.rows.remove(key, this.cap, this.colTypes.length, this.leafSrc()) !== undefined;
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

  // scanUnits is the up-front cost block a FULL scan of this store charges, as
  // (page_read, value_decompress) units: every B-tree node plus — for the query's TOUCHED
  // columns (mask, cost.md §3 "The touched set") — one page_read per overflow chain page and
  // ceil(raw/C) value_decompress slabs per compressed stored value (spec/design/large-values.md
  // §8/§12/§14). Equals (nodeCount, 0) when no touched record spills or compresses — and the row
  // walk is skipped entirely when no touched column type can spill, so fixed-width tables and
  // untouching queries pay nothing extra.
  scanUnits(mask: boolean[]): { pages: number; slabs: number } {
    let pages = this.nodeCount();
    let slabs = 0;
    if (anySpillableMasked(this.colTypes, mask)) {
      for (const e of this.entriesInKeyOrder()) {
        const u = recordScanUnits(this.colTypes, e.key, e.row, this.cap, mask);
        pages += u.pages;
        slabs += u.decompress;
      }
    }
    return { pages, slabs };
  }

  // overlapScanUnits is the up-front cost block a BOUNDED scan over b charges, as
  // (page_read, value_decompress) units: the nodes the bound's key range intersects plus the chain
  // pages and decompress slabs of the records the bound admits (cost.md §3;
  // spec/design/large-values.md §8/§12/§13). An empty bound or a point-lookup miss admits no record
  // and adds nothing beyond the path nodes.
  overlapScanUnits(b: KeyBound, mask: boolean[]): { pages: number; slabs: number } {
    let pages = this.overlapNodeCount(b);
    let slabs = 0;
    if (anySpillableMasked(this.colTypes, mask)) {
      for (const e of this.rangeEntries(b)) {
        const u = recordScanUnits(this.colTypes, e.key, e.row, this.cap, mask);
        pages += u.pages;
        slabs += u.decompress;
      }
    }
    return { pages, slabs };
  }

  // rangeScanWithUnits is the fused single-descent bounded scan: the admitted (key, row) entries
  // PLUS the (page_read, value_decompress) cost block the bound charges — exactly rangeEntries +
  // overlapScanUnits, computed in ONE B-tree traversal instead of three (the windowed walk visits
  // precisely the nodes overlapNodeCount counts, and the per-admitted-record spill/compress units
  // are computed inline from the entries it collects). Byte-identical cost and rows by construction.
  rangeScanWithUnits(
    b: KeyBound,
    mask: boolean[],
  ): { entries: Entry[]; pages: number; slabs: number } {
    return this.rangeScanWithUnitsRecon(b, mask, null);
  }

  // rangeScanWithUnitsMasked is rangeScanWithUnits reconstructing ONLY the touched columns (mask) per
  // admitted row — the read-only SELECT feed's Track A1 touched-column path (packed-leaf.md §11): a
  // Packed leaf skips decoding the untouched columns (left NULL). Identical cost and identical results
  // for a consumer that reads only the touched columns; NOT for one that needs the whole row (mutation /
  // FK / index-maintenance keep the whole-row variant, which recompute keys from the old row). Cost is
  // byte-identical — reconstruction masking carries no cost unit (cost.md §3); the block below reads only
  // `mask` (the same static touched set).
  rangeScanWithUnitsMasked(
    b: KeyBound,
    mask: boolean[],
  ): { entries: Entry[]; pages: number; slabs: number } {
    return this.rangeScanWithUnitsRecon(b, mask, mask);
  }

  // The fused bounded scan shared by the whole-row and touched-column variants. `mask` is the cost
  // touched set (which columns' spill/compress to charge — a §8 byte contract, unchanged across
  // variants). `recon` drives leaf reconstruction: null rebuilds the whole row, a mask rebuilds only its
  // columns (the rest NULL). Reconstruction masking is cost-neutral (no per-column-decode unit,
  // cost.md §3) — the cost block below reads only `mask`.
  private rangeScanWithUnitsRecon(
    b: KeyBound,
    mask: boolean[],
    recon: boolean[] | null,
  ): { entries: Entry[]; pages: number; slabs: number } {
    const { keys, vals, nodes } = this.rows.rangeEntriesCounted(b, this.leafSrc(), recon);
    const entries = keys.map((k, i) => ({ key: k, row: vals[i] }));
    let pages = nodes;
    let slabs = 0;
    if (anySpillableMasked(this.colTypes, mask)) {
      for (const e of entries) {
        const u = recordScanUnits(this.colTypes, e.key, e.row, this.cap, mask);
        pages += u.pages;
        slabs += u.decompress;
      }
    }
    return { entries, pages, slabs };
  }

  // scanWithUnits is the fused single-descent full scan: every (key, row) entry PLUS the full-scan
  // cost block — entriesInKeyOrder + scanUnits in one traversal (the unbounded bound visits every
  // node, so the count equals nodeCount).
  scanWithUnits(mask: boolean[]): { entries: Entry[]; pages: number; slabs: number } {
    return this.rangeScanWithUnits(unboundedBound(), mask);
  }

  // scanWithUnitsMasked is scanWithUnits reconstructing only the touched columns — the read-only
  // full-scan SELECT feed (see rangeScanWithUnitsMasked).
  scanWithUnitsMasked(mask: boolean[]): { entries: Entry[]; pages: number; slabs: number } {
    return this.rangeScanWithUnitsMasked(unboundedBound(), mask);
  }

  // getWithUnits is the fused single-descent point lookup: the row at key (if any) PLUS the
  // (page_read, value_decompress) block its point bound charges — the index fetch path's get +
  // overlapScanUnits in one descent.
  getWithUnits(
    key: Uint8Array,
    mask: boolean[],
  ): { row: Row | undefined; pages: number; slabs: number } {
    const point: KeyBound = { lo: key, loInc: true, hi: key, hiInc: true };
    const { entries, pages, slabs } = this.rangeScanWithUnits(point, mask);
    return { row: entries.length > 0 ? entries[0].row : undefined, pages, slabs };
  }

  // writeCompressUnits is the value_compress slabs storing this record costs — one ceil(raw/C)
  // block per disposition-plan compression attempt (cost.md §3; large-values.md §13). Charged by
  // the executor once per stored row version at the INSERT/UPDATE write site. Zero whenever the
  // record fits inline-plain (no attempt runs), so existing costs do not move.
  writeCompressUnits(key: Uint8Array, row: Row): number {
    if (!anySpillable(this.colTypes)) return 0;
    return recordCompressUnits(this.colTypes, key, row, this.cap);
  }

  // resolveColumns returns `row` with the unfetched values in the columns `mask` selects
  // materialized through this store's pager (spec/design/large-values.md §14). The scan layer
  // calls this per admitted row with the query's touched-set mask — the same static set the cost
  // block charges (cost.md §3), so the physical chain reads / decompressions are exactly what
  // the page_read/value_decompress units metered. When nothing needs resolution the row is
  // returned as-is; otherwise a fresh copy is built — stored rows are shared with the tree and
  // must never be mutated in place. Repeated scans therefore re-read (and are re-charged)
  // consistently.
  resolveColumns(row: Row, mask: boolean[]): Row {
    if (!row.some((v, i) => mask[i] && v.kind === "unfetched")) return row;
    const paging = this.paging;
    if (paging === null) throw new Error("an unfetched value implies a paged store");
    const fetch = (p: number): Uint8Array => paging.readBlock(p);
    return row.map((v, i) =>
      mask[i] && v.kind === "unfetched" ? resolveUnfetched(this.colTypes[i]!, v.ref, fetch) : v,
    );
  }

  // resolveInlineColumns returns `row` with only its inline-deferred values — the form 0x00
  // unfetched form L2 introduces (lazy-record.md §5b) — materialized, leaving the large-value forms
  // (0x02/0x03/0x04) deferred for the §14 touched-set path. The internal index/FK-maintenance write
  // paths read a faulted row's key columns directly (not via a touched-set mask); a key column is
  // always inline (a value too large to be a key cannot be one), so this restores exactly the pre-L2
  // picture those paths assume — inline values resident, large values deferred. It is cost-free: an
  // inline value's bytes are already owned, so it reads no overflow page and decompresses nothing.
  // Used in place of resolveAll, which would instead read an untouched spilled column's chain
  // (unmetered I/O the §14 contract forbids on these paths).
  resolveInlineColumns(row: Row): Row {
    if (!row.some((v) => v.kind === "unfetched" && v.ref.form === 0x00)) return row;
    // An inline form reads no overflow pages — the fetch is never invoked.
    const fetch = (): Uint8Array => {
      throw new Error("inline-deferred resolution reads no overflow pages");
    };
    return row.map((v, i) =>
      v.kind === "unfetched" && v.ref.form === 0x00
        ? resolveUnfetched(this.colTypes[i]!, v.ref, fetch)
        : v,
    );
  }

  // resolveAll materializes EVERY unfetched value in `row` (all columns). The mutation path uses
  // this on a row it is about to re-store (UPDATE), so the stored row is fully resident and its
  // weight/disposition re-plan exactly like an eager writer's (large-values.md §14).
  resolveAll(row: Row): Row {
    return this.resolveColumns(
      row,
      this.colTypes.map(() => true),
    );
  }

  // scanRange streams the rows whose primary key lies within b to visit, in key order, stopping
  // (without faulting further leaves) the moment visit returns false — the genuine LIMIT short-circuit
  // (spec/design/cost.md §3 "LIMIT short-circuit").
  scanRange(
    b: KeyBound,
    recon: boolean[] | null,
    visit: (key: Uint8Array, row: Row) => boolean,
  ): void {
    this.rows.scanRange(b, this.leafSrc(), recon, visit);
  }

  // scanRangeRev is scanRange in reverse: it yields the in-bound rows in DESCENDING key order — a
  // DESC reverse scan (spec/design/cost.md §3), stopping the same way on a false visit so a reverse
  // top-N short-circuits from the high end.
  scanRangeRev(
    b: KeyBound,
    recon: boolean[] | null,
    visit: (key: Uint8Array, row: Row) => boolean,
  ): void {
    this.rows.scanRangeRev(b, this.leafSrc(), recon, visit);
  }

  // scanIter is the PULL form of scanRange / scanRangeRev — a generator yielding (key, row) within b
  // in ascending (reverse=false) or descending (reverse=true) key order (the S2 pull cursor wrapped
  // for the S3 streaming pipeline, spec/design/streaming.md §4). The persistent map shares structure,
  // so this store (a snapshot clone) pins its pages for the cursor's life (transactions.md §5); a leaf
  // faults through the pool only on descent, so a caller that stops early faults no leaves past the
  // stop (the LIMIT short-circuit, cost.md §3).
  scanIter(
    b: KeyBound,
    reverse: boolean,
    recon: boolean[] | null,
  ): Generator<[Uint8Array, Row]> {
    const src = this.leafSrc();
    return reverse
      ? this.rows.scanRangeRevIter(b, src, recon)
      : this.rows.scanRangeIter(b, src, recon);
  }

  // treeRoot is the root B-tree node of this store, for the page-backed serializer
  // (spec/fileformat/format.md). null for an empty table.
  treeRoot(): PNode | null {
    return this.rows.rootNode();
  }

  // columnTypes is the store's resolved per-column ColTypes (a scalar, or a composite resolved to
  // its field tree), for the composite-aware value codec / store coercion (spec/design/composite.md
  // §4). Built once at putTable and read back by the serializer/loader rather than re-walking the
  // type catalog on every row.
  columnTypes(): ColType[] {
    return this.colTypes;
  }

  // setTree installs a loaded B-tree as this store's contents (format.ts loadEngine).
  setTree(root: PNode | null, length: number): void {
    this.rows = pmapFromLoaded(root, length);
  }

  // len returns the row count.
  len(): number {
    return this.rows.size;
  }

  // storedBytes is the total on-disk record bytes this store holds — the deterministic,
  // cross-core-identical footprint measure the temp-table budget sums (spec/design/temp-tables.md §7).
  storedBytes(): number {
    return this.rows.residentRecordBytes();
  }
}
