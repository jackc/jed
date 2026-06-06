// In-memory storage seam (CLAUDE.md §9). A table's rows are held in a PMap — a persistent
// (copy-on-write) ordered map keyed by the primary-key encoding (spec/design/encoding.md), so
// iteration is in key order (the order-preserving encoding makes that the correct logical order
// with no comparator) and the whole store is an O(1) clone that snapshots independently of its
// source. That cheap, structurally-shared clone is what carries the §3 staging-buffer /
// transaction model (spec/design/transactions.md §2): a TableStore clone is the committed
// version a reader holds while a writer mutates its own copy.

import type { Value } from "./value.ts";
import { PMap } from "./pmap.ts";

// Row is a stored row: one value per column, in column order.
export type Row = Value[];

// Entry is one stored (encoded key, row) pair.
export type Entry = { key: Uint8Array; row: Row };

// TableStore holds one table's rows, keyed by encoded primary key.
export class TableStore {
  private rows: PMap;
  // nextRowid is the next synthetic rowid for a table with no primary key. Monotonic —
  // never reused, so a DELETE-then-INSERT cannot collide with a freed key. Unused for
  // tables with a primary key. Reconstructed on load (spec/fileformat).
  private nextRowid: bigint;

  constructor(rows: PMap = new PMap(), nextRowid = 0n) {
    this.rows = rows;
    this.nextRowid = nextRowid;
  }

  // clone returns an independent O(1) snapshot of the store: the PMap clone shares structure
  // (nodes are immutable), so mutating one store leaves the clone untouched. The foundation of
  // the transaction model (spec/design/transactions.md §2).
  clone(): TableStore {
    return new TableStore(this.rows.clone(), this.nextRowid);
  }

  // insert adds a row under its encoded key. Returns false if the key already exists
  // (primary-key uniqueness); the caller decides how to surface that.
  insert(key: Uint8Array, row: Row): boolean {
    if (this.rows.get(key) !== undefined) return false;
    this.rows.insert(key, row);
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
  // unchanged, so key order and the rowid counter are untouched.
  replace(key: Uint8Array, row: Row): void {
    this.rows.insert(key, row);
  }

  // remove deletes the row at key (DELETE). Returns whether a row was present.
  remove(key: Uint8Array): boolean {
    return this.rows.remove(key) !== undefined;
  }

  // get looks up a row by its exact encoded key.
  get(key: Uint8Array): Row | undefined {
    return this.rows.get(key);
  }

  // iterInKeyOrder returns the rows in primary-key (encoded byte) order.
  iterInKeyOrder(): Row[] {
    return this.rows.inorder().vals;
  }

  // entriesInKeyOrder returns all (key, row) pairs in encoded-key order. Used by the
  // on-disk serializer, which stores each row's key verbatim (the key is not always
  // reconstructable from the row — e.g. a no-PK table's synthetic rowid).
  entriesInKeyOrder(): Entry[] {
    const { keys, vals } = this.rows.inorder();
    return keys.map((k, i) => ({ key: k, row: vals[i] }));
  }

  // len returns the row count.
  len(): number {
    return this.rows.size;
  }
}
