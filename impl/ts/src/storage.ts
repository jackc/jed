// In-memory storage seam (CLAUDE.md §9). Rows are keyed by their primary-key encoding
// (spec/design/encoding.md); iteration is in key order. JS Map is insertion-ordered, so
// — like the Go core — rows are held under the encoded key as a *binary string* (each
// byte 0–255 becomes one code unit) and sorted on iteration. Since every code unit is
// ≤ 255, the default string `<` is exactly unsigned byte (memcmp) order, which is the
// order-preserving key order. NEVER rely on insertion order (CLAUDE.md §8: no
// iteration-order leak).

import type { Value } from "./value.ts";

// Row is a stored row: one value per column, in column order.
export type Row = Value[];

// Entry is one stored (encoded key, row) pair.
export type Entry = { key: Uint8Array; row: Row };

// keyString encodes key bytes as a binary string (one code unit per byte). Keys are
// tiny (≤ a few bytes), so this is cheap.
function keyString(key: Uint8Array): string {
  let s = "";
  for (const b of key) s += String.fromCharCode(b);
  return s;
}

// keyBytes is the inverse of keyString.
function keyBytes(s: string): Uint8Array {
  const out = new Uint8Array(s.length);
  for (let i = 0; i < s.length; i++) out[i] = s.charCodeAt(i);
  return out;
}

// TableStore holds one table's rows, keyed by encoded primary key.
export class TableStore {
  private rows: Map<string, Row>;
  // nextRowid is the next synthetic rowid for a table with no primary key. Monotonic —
  // never reused, so a DELETE-then-INSERT cannot collide with a freed key. Unused for
  // tables with a primary key. Reconstructed on load (spec/fileformat).
  private nextRowid: bigint;

  constructor() {
    this.rows = new Map();
    this.nextRowid = 0n;
  }

  // insert adds a row under its encoded key. Returns false if the key already exists
  // (primary-key uniqueness); the caller decides how to surface that.
  insert(key: Uint8Array, row: Row): boolean {
    const k = keyString(key);
    if (this.rows.has(k)) return false;
    this.rows.set(k, row);
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
    this.rows.set(keyString(key), row);
  }

  // remove deletes the row at key (DELETE). Returns whether a row was present.
  remove(key: Uint8Array): boolean {
    return this.rows.delete(keyString(key));
  }

  // get looks up a row by its exact encoded key.
  get(key: Uint8Array): Row | undefined {
    return this.rows.get(keyString(key));
  }

  private sortedKeys(): string[] {
    return [...this.rows.keys()].sort(); // bytewise == memcmp == key order
  }

  // iterInKeyOrder returns the rows in primary-key (encoded byte) order.
  iterInKeyOrder(): Row[] {
    return this.sortedKeys().map((k) => this.rows.get(k)!);
  }

  // entriesInKeyOrder returns all (key, row) pairs in encoded-key order. Used by the
  // on-disk serializer, which stores each row's key verbatim (the key is not always
  // reconstructable from the row — e.g. a no-PK table's synthetic rowid).
  entriesInKeyOrder(): Entry[] {
    return this.sortedKeys().map((k) => ({ key: keyBytes(k), row: this.rows.get(k)! }));
  }

  // len returns the row count.
  len(): number {
    return this.rows.size;
  }
}
