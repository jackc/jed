//! In-memory storage seam (CLAUDE.md §9).
//!
//! A table's rows are held in a [`PMap`](crate::pmap) — a persistent (copy-on-write) ordered
//! map keyed by the primary-key encoding (spec/design/encoding.md), so iteration is in key
//! order (the order-preserving encoding makes that the correct logical order with no
//! comparator) **and** the whole store is an O(1) `Clone` that snapshots independently of its
//! source. That cheap, structurally-shared clone is what carries the §3 staging-buffer /
//! transaction model (spec/design/transactions.md §2): a `TableStore` clone is the committed
//! version a reader holds while a writer mutates its own copy.
//!
//! Since Phase 6 (P6.1) the [`PMap`] is the **page-backed B-tree**: its fan-out is size-driven,
//! so each entry's on-disk **weight** (record size) and the page payload `cap` (= `page_size − 12`)
//! govern when a node splits (spec/fileformat/format.md). The store holds the column types and
//! `cap` so it can compute weights ([`crate::format::record_size`]) the map itself never needs.

use std::sync::Arc;

use crate::error::Result;
use crate::paging::SharedPaging;
use crate::pmap::{KeyBound, LeafSource, Node, PMap};
use crate::types::ScalarType;
use crate::value::Value;

/// A stored row: one value per column, in column order.
pub type Row = Vec<Value>;

/// The buffer-pool leaf source for one store (spec/design/pager.md §4): faults a clean leaf page
/// through this database's shared pool, decoding it with this table's column types. Built per call
/// from the store's `paging` + `col_types` (disjoint field borrows, so it composes with a `&mut`
/// mutation of `rows`). A store with no `paging` (in-memory) builds none and never faults.
struct PagedSource<'a> {
    paging: &'a SharedPaging,
    col_types: &'a [ScalarType],
}

impl LeafSource for PagedSource<'_> {
    fn load_leaf(&self, page: u32) -> Result<Arc<Node>> {
        self.paging.fault_leaf(page, self.col_types)
    }
}

/// Build the leaf source from a store's paging context + column types (disjoint from `rows`). Free
/// function (not a `&self` method) so the borrow is of `paging`/`col_types` only — `rows` stays free
/// to be mutated alongside it.
fn make_src<'a>(
    paging: &'a Option<Arc<SharedPaging>>,
    col_types: &'a [ScalarType],
) -> Option<PagedSource<'a>> {
    paging.as_ref().map(|p| PagedSource {
        paging: p,
        col_types,
    })
}

/// A single table's rows, keyed by encoded primary key. Cloning is O(1) and yields an
/// independent snapshot (the [`PMap`] shares structure; mutating one leaves the other
/// untouched) — the foundation of the transaction model (spec/design/transactions.md §2).
#[derive(Clone)]
pub struct TableStore {
    rows: PMap,
    /// Next synthetic rowid for a table with no primary key. Monotonic — never
    /// reused, so a DELETE-then-INSERT cannot collide with a freed key. Unused for
    /// tables that have a primary key. Reconstructed on load (spec/fileformat).
    next_rowid: i64,
    /// Page payload capacity `C = page_size − 12` — the split threshold for the page-backed
    /// B-tree (spec/fileformat/format.md). Fixed for the life of the database.
    cap: usize,
    /// The table's column types, for computing each row's on-disk record weight
    /// ([`crate::format::record_size`]). `Arc` so a snapshot clone stays O(1).
    col_types: Arc<Vec<ScalarType>>,
    /// The shared pager + leaf buffer pool for a **file-backed** database (spec/design/pager.md):
    /// the read/mutation path faults `OnDisk` leaves through it. `None` for an in-memory database
    /// and for a table created in-session (fully resident until the file is reopened); attached by
    /// the demand-paged file load. `Arc` so a snapshot clone shares the one pool per database.
    paging: Option<Arc<SharedPaging>>,
}

impl TableStore {
    /// A new empty store for a table whose columns have the given types, serializing at page
    /// payload `cap` (= `page_size − 12`). In-memory (no paging) until [`attach_paging`].
    pub fn new(cap: usize, col_types: Vec<ScalarType>) -> Self {
        TableStore {
            rows: PMap::new(),
            next_rowid: 0,
            cap,
            col_types: Arc::new(col_types),
            paging: None,
        }
    }

    /// Attach this database's shared paging context (the demand-paged file load, format.rs): the
    /// store's `OnDisk` leaves now fault through the pool. One pool per database, shared by every
    /// store and snapshot.
    pub(crate) fn attach_paging(&mut self, paging: Arc<SharedPaging>) {
        self.paging = Some(paging);
    }

    /// This row's on-disk record size — the weight the page-backed B-tree splits on. Accounts for
    /// out-of-line spill at `cap` (an externalized value weighs its pointer, not its full body —
    /// spec/design/large-values.md §12), so split points match the serialized pages.
    fn weight(&self, key: &[u8], row: &Row) -> u32 {
        crate::format::record_size(&self.col_types, key, row, self.cap) as u32
    }

    /// Insert a row under its encoded key. Returns `Ok(false)` if the key already exists
    /// (primary-key uniqueness); the caller decides how to surface that. May fault the target leaf
    /// through the buffer pool (an I/O error then propagates).
    pub fn insert(&mut self, key: Vec<u8>, row: Row) -> Result<bool> {
        let w = self.weight(&key, &row); // full `&self` borrow — taken before the leaf source
        let src = make_src(&self.paging, &self.col_types);
        let src_ref = src.as_ref().map(|s| s as &dyn LeafSource);
        if self.rows.get(&key, src_ref)?.is_some() {
            return Ok(false);
        }
        self.rows.insert(key, row, w, self.cap, src_ref)?;
        Ok(true)
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
    /// key order and the rowid counter are untouched. The caller only replaces keys it
    /// just found, so the overwrite always lands on a present key. May fault the target leaf.
    pub fn replace(&mut self, key: &[u8], row: Row) -> Result<()> {
        let w = self.weight(key, &row);
        let src = make_src(&self.paging, &self.col_types);
        let src_ref = src.as_ref().map(|s| s as &dyn LeafSource);
        self.rows.insert(key.to_vec(), row, w, self.cap, src_ref)?;
        Ok(())
    }

    /// Remove the row at `key` (DELETE). Returns whether a row was present. May fault leaves the
    /// delete descends into / rebalances against.
    pub fn remove(&mut self, key: &[u8]) -> Result<bool> {
        let src = make_src(&self.paging, &self.col_types);
        let src_ref = src.as_ref().map(|s| s as &dyn LeafSource);
        Ok(self.rows.remove(key, self.cap, src_ref)?.is_some())
    }

    /// Look up a row by its exact encoded key. Returns an **owned** row — under demand paging the
    /// holding leaf may live only in the buffer pool (spec/design/pager.md §4, [`PMap::get`]).
    pub fn get(&self, key: &[u8]) -> Result<Option<Row>> {
        let src = make_src(&self.paging, &self.col_types);
        let src_ref = src.as_ref().map(|s| s as &dyn LeafSource);
        self.rows.get(key, src_ref)
    }

    /// All rows in primary-key (encoded byte) order, as **owned** rows (spec/design/pager.md §4,
    /// [`PMap::iter`]). Eager: leaves fault through the pool during the walk and are dropped as their
    /// rows are copied out, so the resident leaf set stays bounded by the pool, not the table.
    pub fn iter_in_key_order(&self) -> Result<Vec<Row>> {
        let src = make_src(&self.paging, &self.col_types);
        let src_ref = src.as_ref().map(|s| s as &dyn LeafSource);
        Ok(self
            .rows
            .iter(src_ref)?
            .into_iter()
            .map(|(_, v)| v)
            .collect())
    }

    /// The number of B-tree nodes (pages) in this store — the `page_read` count a full scan
    /// charges (spec/design/cost.md §3 "page_read"). `0` for an empty table.
    pub fn node_count(&self) -> usize {
        self.rows.node_count()
    }

    /// All `(encoded key, row)` pairs in key order, as **owned** pairs (spec/design/pager.md §4,
    /// [`PMap::iter`]). Used by the executor's UPDATE/DELETE scan and the on-disk free-list rowid
    /// reconstruction (spec/fileformat/format.md). Eager; bounded resident leaves as `iter_in_key_order`.
    pub fn iter_entries(&self) -> Result<Vec<(Vec<u8>, Row)>> {
        let src = make_src(&self.paging, &self.col_types);
        let src_ref = src.as_ref().map(|s| s as &dyn LeafSource);
        self.rows.iter(src_ref)
    }

    /// The rows whose primary key lies within the bound, in key order — a bounded B-tree scan that
    /// faults only the leaves the bound spans (spec/design/cost.md §3 "bounded scan").
    pub(crate) fn range_rows(&self, b: &KeyBound) -> Result<Vec<Row>> {
        let src = make_src(&self.paging, &self.col_types);
        let src_ref = src.as_ref().map(|s| s as &dyn LeafSource);
        Ok(self
            .rows
            .range_entries(b, src_ref)?
            .into_iter()
            .map(|(_, v)| v)
            .collect())
    }

    /// The `(encoded key, row)` pairs whose primary key lies within the bound, in key order (the
    /// mutation paths need the keys to remove/replace).
    pub(crate) fn range_entries(&self, b: &KeyBound) -> Result<Vec<(Vec<u8>, Row)>> {
        let src = make_src(&self.paging, &self.col_types);
        let src_ref = src.as_ref().map(|s| s as &dyn LeafSource);
        self.rows.range_entries(b, src_ref)
    }

    /// The number of B-tree nodes a bounded scan over `b` visits — the `page_read` it charges
    /// (cost.md §3). Equals `node_count` for the unbounded bound.
    pub(crate) fn overlap_node_count(&self, b: &KeyBound) -> usize {
        self.rows.overlap_node_count(b)
    }

    /// The up-front cost block a **full scan** of this store charges, as
    /// `(page_read, value_decompress)` units: every B-tree node plus one `page_read` per overflow
    /// chain page, and `ceil(raw/C)` `value_decompress` slabs per compressed stored value
    /// (cost.md §3; spec/design/large-values.md §8/§12/§13). Equals `(node_count, 0)` when no
    /// record spills or compresses — and the row walk is skipped entirely when no column type can
    /// spill, so fixed-width tables pay nothing extra.
    pub fn scan_units(&self) -> Result<(usize, usize)> {
        let mut pages = self.node_count();
        let mut slabs = 0usize;
        if crate::format::any_spillable(&self.col_types) {
            for (k, row) in self.iter_entries()? {
                let u = crate::format::record_scan_units(&self.col_types, &k, &row, self.cap);
                pages += u.pages;
                slabs += u.decompress;
            }
        }
        Ok((pages, slabs))
    }

    /// The up-front cost block a **bounded scan** over `b` charges, as
    /// `(page_read, value_decompress)` units: the nodes the bound's key range intersects plus the
    /// chain pages and decompress slabs of the records the bound admits (cost.md §3;
    /// spec/design/large-values.md §8/§12/§13). An empty bound or a point-lookup miss admits no
    /// record and adds nothing beyond the path nodes.
    pub(crate) fn overlap_scan_units(&self, b: &KeyBound) -> Result<(usize, usize)> {
        let mut pages = self.overlap_node_count(b);
        let mut slabs = 0usize;
        if crate::format::any_spillable(&self.col_types) {
            for (k, row) in self.range_entries(b)? {
                let u = crate::format::record_scan_units(&self.col_types, &k, &row, self.cap);
                pages += u.pages;
                slabs += u.decompress;
            }
        }
        Ok((pages, slabs))
    }

    /// The `value_compress` slabs storing this record costs — one `ceil(raw/C)` block per
    /// disposition-plan compression attempt (cost.md §3; large-values.md §13). Charged by the
    /// executor once per stored row version at the INSERT/UPDATE write site. Zero whenever the
    /// record fits inline-plain (no attempt runs), so existing costs do not move.
    pub(crate) fn write_compress_units(&self, key: &[u8], row: &Row) -> usize {
        if !crate::format::any_spillable(&self.col_types) {
            return 0;
        }
        crate::format::record_compress_units(&self.col_types, key, row, self.cap)
    }

    /// Stream the rows whose primary key lies within `b` to `visit`, in key order, stopping (without
    /// faulting further leaves) the moment `visit` returns `Ok(false)` — the genuine LIMIT
    /// short-circuit (spec/design/cost.md §3 "LIMIT short-circuit").
    pub(crate) fn scan_range(
        &self,
        b: &KeyBound,
        visit: &mut dyn FnMut(&[u8], &Row) -> Result<bool>,
    ) -> Result<()> {
        let src = make_src(&self.paging, &self.col_types);
        let src_ref = src.as_ref().map(|s| s as &dyn LeafSource);
        self.rows.scan_range(b, src_ref, visit)
    }

    /// The root B-tree node of this table's store, for the page-backed serializer
    /// (spec/fileformat/format.md). `None` for an empty table.
    pub(crate) fn tree_root(&self) -> Option<&Arc<Node>> {
        self.rows.root()
    }

    /// Install a loaded B-tree as this store's contents (format.rs `from_image`).
    pub(crate) fn set_tree(&mut self, root: Option<Arc<Node>>, len: usize) {
        self.rows = PMap::from_loaded(root, len);
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}
