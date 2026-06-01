//! In-memory storage seam (CLAUDE.md §9).
//!
//! Step-5a is in-memory only; on-disk persistence (the block device, the byte
//! format, the Rust↔Go round-trip) is step-5b behind this same seam. Rows are held
//! keyed by their primary-key encoding (spec/design/encoding.md) in a sorted map, so
//! iteration is in key order — the order-preserving encoding is what makes that the
//! correct logical order with no comparator.

use std::collections::BTreeMap;

use crate::value::Value;

/// A stored row: one value per column, in column order.
pub type Row = Vec<Value>;

/// A single table's rows, keyed by encoded primary key. `BTreeMap<Vec<u8>, _>`
/// orders by raw byte order — exactly the order-preserving key encoding's contract.
#[derive(Default)]
pub struct TableStore {
    rows: BTreeMap<Vec<u8>, Row>,
    /// Next synthetic rowid for a table with no primary key. Monotonic — never
    /// reused, so a DELETE-then-INSERT cannot collide with a freed key. Unused for
    /// tables that have a primary key. Reconstructed on load (spec/fileformat).
    next_rowid: i64,
}

impl TableStore {
    pub fn new() -> Self {
        TableStore {
            rows: BTreeMap::new(),
            next_rowid: 0,
        }
    }

    /// Insert a row under its encoded key. Returns false if the key already exists
    /// (primary-key uniqueness); the caller decides how to surface that.
    pub fn insert(&mut self, key: Vec<u8>, row: Row) -> bool {
        if self.rows.contains_key(&key) {
            return false;
        }
        self.rows.insert(key, row);
        true
    }

    /// Allocate the next monotonic rowid (for a table with no primary key) and
    /// advance the counter. Never returns a previously-issued value.
    pub fn alloc_rowid(&mut self) -> i64 {
        let r = self.next_rowid;
        self.next_rowid += 1;
        r
    }

    /// Ensure the rowid counter is at least `n` (used on load to set it past every
    /// rowid already present, so future inserts don't collide).
    pub fn bump_rowid_to(&mut self, n: i64) {
        if n > self.next_rowid {
            self.next_rowid = n;
        }
    }

    /// Replace the row stored at an existing key (UPDATE). The key is unchanged, so
    /// key order and the rowid counter are untouched. Panics if the key is absent —
    /// the caller only replaces keys it just found.
    pub fn replace(&mut self, key: &[u8], row: Row) {
        let slot = self.rows.get_mut(key).expect("replace at an existing key");
        *slot = row;
    }

    /// Remove the row at `key` (DELETE). Returns whether a row was present.
    pub fn remove(&mut self, key: &[u8]) -> bool {
        self.rows.remove(key).is_some()
    }

    /// Look up a row by its exact encoded key.
    pub fn get(&self, key: &[u8]) -> Option<&Row> {
        self.rows.get(key)
    }

    /// Iterate rows in primary-key (encoded byte) order.
    pub fn iter_in_key_order(&self) -> impl Iterator<Item = &Row> {
        self.rows.values()
    }

    /// Iterate `(encoded key, row)` pairs in key order. Used by the on-disk
    /// serializer (spec/fileformat/format.md), which stores each row's key verbatim.
    pub fn iter_entries(&self) -> impl Iterator<Item = (&Vec<u8>, &Row)> {
        self.rows.iter()
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}
