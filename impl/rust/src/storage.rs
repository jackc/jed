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
//! so each entry's on-disk **weight** (record size) and the page payload `cap` (= `page_size − 16`)
//! govern when a node splits (spec/fileformat/format.md). The store holds the column types and
//! `cap` so it can compute weights ([`crate::format::record_size`]) the map itself never needs.

use std::sync::Arc;

use crate::catalog::ColType;
use crate::error::Result;
use crate::paging::SharedPaging;
use crate::pmap::{KeyBound, LeafSource, Node, PMap};
use crate::value::{Unfetched, Value};

/// A stored row: one value per column, in column order.
pub type Row = Vec<Value>;

/// The buffer-pool leaf source for one store (spec/design/pager.md §4): faults a clean leaf page
/// through this database's shared pool, decoding it with this table's column types. Built per call
/// from the store's `paging` + `col_types` (disjoint field borrows, so it composes with a `&mut`
/// mutation of `rows`). A store with no `paging` (in-memory) builds none and never faults.
struct PagedSource<'a> {
    paging: &'a SharedPaging,
    col_types: &'a Arc<Vec<ColType>>,
}

impl LeafSource for PagedSource<'_> {
    fn load_leaf(&self, page: u32) -> Result<Arc<Node>> {
        self.paging.fault_leaf(page, self.col_types)
    }
}

/// A **pull** scan cursor over a [`TableStore`]'s `(key, row)` pairs within a bound (the S3
/// streaming cursor, spec/design/streaming.md §4/§5). It **owns** an O(1) snapshot clone of the
/// store — the copy-on-write persistent map shares structure, so the clone pins the root and keeps
/// its pages alive for the cursor's whole life (transactions.md §5) — and an underlying
/// [`RangeCursor`] over that snapshot's map. Each [`next`](StoreScan::next) rebuilds the leaf source
/// from the owned snapshot's `paging`/`col_types` (the disjoint-field [`make_src`] discipline), so
/// the cursor borrows nothing and is `'static`: a streaming `Rows` can own it and outlive the handle
/// that produced it. Built by [`TableStore::store_scan`].
pub(crate) struct StoreScan {
    store: TableStore,
    cursor: crate::pmap::RangeCursor,
}

impl StoreScan {
    /// The next in-bound `(key, row)` pair, or `None` at end. Yields the EXACT same sequence as the
    /// push [`scan_range`](TableStore::scan_range) / [`scan_range_rev`](TableStore::scan_range_rev)
    /// (the S2 contract). Faults a clean leaf through the snapshot's pool only when the traversal
    /// descends into it, so a caller that stops pulling early faults no leaves past the stop (the
    /// LIMIT short-circuit, cost.md §3). The returned row is **owned** (cloned out), eviction-safe
    /// under demand paging (pager.md §4), exactly like [`PMap::iter`].
    pub(crate) fn next(&mut self) -> Result<Option<(Vec<u8>, Row)>> {
        let src = make_src(&self.store.paging, &self.store.col_types);
        let src_ref = src.as_ref().map(|s| s as &dyn LeafSource);
        self.cursor.next(src_ref)
    }

    /// Materialize the unfetched values in the columns `mask` selects, in place, through the snapshot's
    /// pager (large-values.md §14) — the per-row resolve step of a streaming pipeline. Delegates to the
    /// owned snapshot store, so it reads through the same pinned pages the scan walks.
    pub(crate) fn resolve_columns(&self, row: &mut Row, mask: &[bool]) -> Result<()> {
        self.store.resolve_columns(row, mask)
    }
}

/// Build the leaf source from a store's paging context + column types (disjoint from `rows`). Free
/// function (not a `&self` method) so the borrow is of `paging`/`col_types` only — `rows` stays free
/// to be mutated alongside it.
fn make_src<'a>(
    paging: &'a Option<Arc<SharedPaging>>,
    col_types: &'a Arc<Vec<ColType>>,
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
    /// Page payload capacity `C = page_size − 16` (`PAGE_HEADER`) — the split threshold for the
    /// page-backed B-tree (spec/fileformat/format.md). Fixed for the life of the database.
    cap: usize,
    /// The table's resolved column types ([`ColType`] — a scalar, or a composite resolved to its
    /// field-type tree), for the value codec and for computing each row's on-disk record weight
    /// ([`crate::format::record_size`]). `Arc` so a snapshot clone stays O(1).
    col_types: Arc<Vec<ColType>>,
    /// The shared pager + leaf buffer pool for a **file-backed** database (spec/design/pager.md):
    /// the read/mutation path faults `OnDisk` leaves through it. `None` for an in-memory database
    /// and for a table created in-session (fully resident until the file is reopened); attached by
    /// the demand-paged file load. `Arc` so a snapshot clone shares the one pool per database.
    paging: Option<Arc<SharedPaging>>,
}

impl TableStore {
    /// A new empty store for a table whose columns have the given resolved types, serializing at
    /// page payload `cap` (= `page_size − 16`). In-memory (no paging) until [`attach_paging`].
    pub fn new(cap: usize, col_types: Vec<ColType>) -> Self {
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
        let k = self.col_types.len(); // PAX leaf directory overhead (format.md v23)
        let src = make_src(&self.paging, &self.col_types);
        let src_ref = src.as_ref().map(|s| s as &dyn LeafSource);
        if self.rows.get(&key, src_ref)?.is_some() {
            return Ok(false);
        }
        self.rows.insert(key, row, w, self.cap, k, src_ref)?;
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
        let k = self.col_types.len();
        let src = make_src(&self.paging, &self.col_types);
        let src_ref = src.as_ref().map(|s| s as &dyn LeafSource);
        self.rows
            .insert(key.to_vec(), row, w, self.cap, k, src_ref)?;
        Ok(())
    }

    /// Remove the row at `key` (DELETE). Returns whether a row was present. May fault leaves the
    /// delete descends into / rebalances against.
    pub fn remove(&mut self, key: &[u8]) -> Result<bool> {
        let k = self.col_types.len();
        let src = make_src(&self.paging, &self.col_types);
        let src_ref = src.as_ref().map(|s| s as &dyn LeafSource);
        Ok(self.rows.remove(key, self.cap, k, src_ref)?.is_some())
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
    /// `(page_read, value_decompress)` units: every B-tree node plus — for the query's **touched
    /// columns** (`mask`, cost.md §3 "The touched set") — one `page_read` per overflow chain page
    /// and `ceil(raw/C)` `value_decompress` slabs per compressed stored value
    /// (spec/design/large-values.md §8/§12/§14). Equals `(node_count, 0)` when no touched record
    /// spills or compresses — and the row walk is skipped entirely when no touched column type
    /// can spill, so fixed-width tables and untouching queries pay nothing extra.
    pub fn scan_units(&self, mask: &[bool]) -> Result<(usize, usize)> {
        let mut pages = self.node_count();
        let mut slabs = 0usize;
        if crate::format::any_spillable_masked(&self.col_types, mask) {
            for (k, row) in self.iter_entries()? {
                let u = crate::format::record_scan_units(&self.col_types, &k, &row, self.cap, mask);
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
    pub(crate) fn overlap_scan_units(&self, b: &KeyBound, mask: &[bool]) -> Result<(usize, usize)> {
        let mut pages = self.overlap_node_count(b);
        let mut slabs = 0usize;
        if crate::format::any_spillable_masked(&self.col_types, mask) {
            for (k, row) in self.range_entries(b)? {
                let u = crate::format::record_scan_units(&self.col_types, &k, &row, self.cap, mask);
                pages += u.pages;
                slabs += u.decompress;
            }
        }
        Ok((pages, slabs))
    }

    /// Fused single-descent bounded scan: the admitted `(key, row)` entries PLUS the
    /// `(page_read, value_decompress)` cost block the bound charges — exactly
    /// [`range_entries`](TableStore::range_entries) + [`overlap_scan_units`](TableStore::overlap_scan_units),
    /// computed in ONE B-tree traversal instead of three (the windowed walk visits precisely the
    /// nodes `overlap_node_count` counts, and the per-admitted-record spill/compress units are
    /// computed inline from the entries it collects). Byte-identical cost and rows by construction.
    pub(crate) fn range_scan_with_units(
        &self,
        b: &KeyBound,
        mask: &[bool],
    ) -> Result<(Vec<(Vec<u8>, Row)>, usize, usize)> {
        let src = make_src(&self.paging, &self.col_types);
        let src_ref = src.as_ref().map(|s| s as &dyn LeafSource);
        let (entries, mut pages) = self.rows.range_entries_counted(b, src_ref)?;
        let mut slabs = 0usize;
        if crate::format::any_spillable_masked(&self.col_types, mask) {
            for (k, row) in &entries {
                let u = crate::format::record_scan_units(&self.col_types, k, row, self.cap, mask);
                pages += u.pages;
                slabs += u.decompress;
            }
        }
        Ok((entries, pages, slabs))
    }

    /// Fused single-descent full scan: every `(key, row)` entry PLUS the full-scan cost block —
    /// [`iter_entries`](TableStore::iter_entries) + [`scan_units`](TableStore::scan_units) in one
    /// traversal (the unbounded bound visits every node, so the count equals `node_count`).
    pub(crate) fn scan_with_units(
        &self,
        mask: &[bool],
    ) -> Result<(Vec<(Vec<u8>, Row)>, usize, usize)> {
        self.range_scan_with_units(&KeyBound::unbounded(), mask)
    }

    /// Fused single-descent point lookup: the row at `key` (if any) PLUS the
    /// `(page_read, value_decompress)` block its point bound charges — the index fetch path's
    /// [`get`](TableStore::get) + [`overlap_scan_units`](TableStore::overlap_scan_units) in one
    /// descent.
    pub(crate) fn get_with_units(
        &self,
        key: &[u8],
        mask: &[bool],
    ) -> Result<(Option<Row>, usize, usize)> {
        let point = KeyBound {
            lo: Some(key.to_vec()),
            lo_inc: true,
            hi: Some(key.to_vec()),
            hi_inc: true,
        };
        let (entries, pages, slabs) = self.range_scan_with_units(&point, mask)?;
        Ok((entries.into_iter().next().map(|(_, v)| v), pages, slabs))
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

    /// Whether `row` holds an unfetched large-value reference in any column `mask` selects —
    /// the cheap gate before [`resolve_columns`](TableStore::resolve_columns) clones/faults.
    pub(crate) fn needs_resolution(row: &Row, mask: &[bool]) -> bool {
        row.iter()
            .zip(mask.iter())
            .any(|(v, &m)| m && matches!(v, Value::Unfetched(_)))
    }

    /// Materialize the unfetched values in the columns `mask` selects, in place, through this
    /// store's pager (spec/design/large-values.md §14). The scan layer calls this per admitted
    /// row with the query's touched-set mask — the same static set the cost block charges
    /// (cost.md §3), so the physical chain reads / decompressions are exactly what the
    /// `page_read`/`value_decompress` units metered. Resolution mutates only the scan's owned
    /// copy, never the shared tree, so repeated scans re-read (and are re-charged) consistently.
    pub(crate) fn resolve_columns(&self, row: &mut Row, mask: &[bool]) -> Result<()> {
        for (i, v) in row.iter_mut().enumerate() {
            if !mask[i] {
                continue;
            }
            if let Value::Unfetched(u) = v {
                let paging = self
                    .paging
                    .as_ref()
                    .expect("an unfetched value implies a paged store");
                let fetch = |p: u32| paging.pager().read_block(p);
                *v = crate::format::resolve_unfetched(&self.col_types[i], u, &fetch)?;
            }
        }
        Ok(())
    }

    /// Materialize **every** unfetched value in `row` (all columns). The mutation path uses this
    /// on a row it is about to re-store (UPDATE), so the stored row is fully resident and its
    /// weight/disposition re-plan exactly like an eager writer's (large-values.md §14).
    pub(crate) fn resolve_all(&self, row: &mut Row) -> Result<()> {
        let all = vec![true; self.col_types.len()];
        self.resolve_columns(row, &all)
    }

    /// Materialize only the **inline-deferred** values in `row` — the `Unfetched::Inline` form L2
    /// introduces (spec/design/lazy-record.md §5b) — leaving the large-value forms (External /
    /// InlineComp / ExternalComp) deferred for the §14 touched-set path. The internal
    /// index/FK-maintenance write paths read a faulted row's *key* columns directly (not via a
    /// touched-set mask); a key column is always inline (a value too large to be a key cannot be
    /// one), so this restores exactly the pre-L2 picture those paths were written against — inline
    /// values resident, large values deferred. It is **cost-free**: an inline value's bytes are
    /// already owned, so resolution reads no overflow page and decompresses nothing, and inline
    /// values carry no metered units (cost.md §3). Used in place of [`resolve_all`], which would
    /// instead read an untouched *spilled* column's chain (unmetered I/O the §14 contract forbids
    /// on these paths).
    pub(crate) fn resolve_inline_columns(&self, row: &mut Row) -> Result<()> {
        for (i, v) in row.iter_mut().enumerate() {
            if matches!(v, Value::Unfetched(Unfetched::Inline { .. }))
                && let Value::Unfetched(u) = v
            {
                // An inline form reads no overflow pages — the fetch is never invoked.
                let fetch = |_p: u32| -> Result<Vec<u8>> {
                    unreachable!("inline-deferred resolution reads no overflow pages")
                };
                *v = crate::format::resolve_unfetched(&self.col_types[i], u, &fetch)?;
            }
        }
        Ok(())
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

    /// Like [`scan_range`](TableStore::scan_range) but yields the in-bound rows in **descending**
    /// key order — a `DESC` reverse scan (spec/design/cost.md §3), stopping the same way on a false
    /// `visit` so a reverse top-N short-circuits from the high end.
    pub(crate) fn scan_range_rev(
        &self,
        b: &KeyBound,
        visit: &mut dyn FnMut(&[u8], &Row) -> Result<bool>,
    ) -> Result<()> {
        let src = make_src(&self.paging, &self.col_types);
        let src_ref = src.as_ref().map(|s| s as &dyn LeafSource);
        self.rows.scan_range_rev(b, src_ref, visit)
    }

    /// A **pull** scan cursor over this store's `(key, row)` pairs within `b`, in ascending
    /// (`reverse = false`) or descending (`reverse = true`) key order — the pull-model equivalent of
    /// [`scan_range`](TableStore::scan_range) (spec/design/streaming.md §4/§5, the S3 streaming
    /// cursor). It **owns an O(1) snapshot clone** of this store (the persistent map shares
    /// structure, so the clone pins the root and keeps its pages alive for the cursor's life —
    /// transactions.md §5), and rebuilds the leaf source per [`next`](StoreScan::next) call from that
    /// owned snapshot's paging context, so the returned `StoreScan` borrows nothing and is `'static`.
    /// This is what lets a streaming `Rows` cursor outlive the handle that produced it.
    pub(crate) fn store_scan(&self, b: KeyBound, reverse: bool) -> StoreScan {
        StoreScan {
            cursor: self.rows.range_cursor(b, reverse),
            store: self.clone(),
        }
    }

    /// The root B-tree node of this table's store, for the page-backed serializer
    /// (spec/fileformat/format.md). `None` for an empty table.
    pub(crate) fn tree_root(&self) -> Option<&Arc<Node>> {
        self.rows.root()
    }

    /// The table's resolved column types — the value codec's input on every serialize/decode path
    /// (spec/design/composite.md §4). Empty for an index store (records are the key alone).
    pub(crate) fn col_types(&self) -> &[ColType] {
        &self.col_types
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

    /// Total on-disk record bytes this store holds — the deterministic, cross-core-identical
    /// footprint measure the temp-table budget sums (spec/design/temp-tables.md §7).
    pub(crate) fn stored_bytes(&self) -> u64 {
        self.rows.resident_record_bytes()
    }
}
