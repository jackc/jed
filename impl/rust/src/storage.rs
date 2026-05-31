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
}

impl TableStore {
    pub fn new() -> Self {
        TableStore {
            rows: BTreeMap::new(),
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
