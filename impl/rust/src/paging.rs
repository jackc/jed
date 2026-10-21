//! Shared paging context for a **file-backed** database (spec/design/pager.md §2/§3): the open
//! [`Pager`] plus the bounded leaf [`BufferPool`], shared by every table store and snapshot of one
//! database. Page ids are file-global (one page space per file), so there is exactly **one** pool and
//! one pager per database, behind `Arc<SharedPaging>` — a `TableStore`/`Snapshot` clone shares it.
//!
//! The read path faults a clean **leaf** through [`SharedPaging::fault_leaf`]: a pool hit returns the
//! cached node, a miss reads the page through the pager, decodes it (the node codec, format.rs) and
//! caches it, evicting under CLOCK when full. **No pins** (pager.md §4): eviction only drops the
//! cache entry, and a clean leaf is immutable so any in-flight `Arc` stays valid and a re-load is a
//! harmless duplicate. An in-memory database has no `SharedPaging` (it is fully resident).
//!
//! Not a §8 byte contract (pager.md §3): the pool changes *when* a page is resident, never *what* a
//! query observes — so each core realizes it idiomatically (like P5.3's per-core concurrency). The
//! pool internally locks only for hit/flight/CLOCK bookkeeping; the physical read takes the pager
//! lock separately, and checksum + PAX parsing run outside both locks. Commits still serialize with
//! the physical read through the pager lock.

use std::sync::{Arc, Mutex, MutexGuard};

use crate::bufferpool::BufferPool;
use crate::catalog::ColType;
use crate::error::Result;
use crate::format::{Cap, PageRef};
use crate::pager::Pager;
use crate::pmap::Node;

/// The default memory budget for the resident leaf cache, in **bytes** (256 MiB) — the
/// [`crate::OpenOptions::cache_bytes`] default (spec/design/pager.md §3, api.md §2.1). Sized so the
/// dominant case — a RAM-sized database (CLAUDE.md §9) — stays fully cache-resident under the
/// default; stated in bytes so the budget does not silently scale with a file's page size.
/// Converted to a leaf-page capacity by [`cache_leaves`].
pub(crate) const DEFAULT_CACHE_BYTES: usize = 256 * 1024 * 1024;

/// Convert a byte budget to a resident-leaf-page capacity for a file of `page_size` bytes:
/// `max(1, cache_bytes / page_size)` (pager.md §3). The `max(1, …)` floor keeps one leaf resident even
/// when `cache_bytes < page_size` — the minimum to walk a root→leaf path. The divisor is clamped to
/// ≥ 1 so a malformed `page_size = 0` cannot divide by zero (the loader rejects it separately as
/// corrupt — format.rs).
pub(crate) fn cache_leaves(cache_bytes: usize, page_size: u32) -> usize {
    (cache_bytes / (page_size as usize).max(1)).max(1)
}

/// One database's pager + leaf buffer pool, shared (`Arc`) by all its stores and snapshots.
pub(crate) struct SharedPaging {
    pager: Mutex<Pager>,
    pool: BufferPool<Node>,
    /// The store fetches several pages in one request (a page client): scans read ahead
    /// ([`prefetch_leaves`](Self::prefetch_leaves)).
    batches: bool,
    /// An encrypted database's page codec (spec/fileformat/encrypted.md): every body page read
    /// through this context is opened, and every commit page sealed, under its keys. `None` for a
    /// plaintext file and every in-memory or temp store.
    codec: Option<Arc<crate::crypto::PageCodec>>,
    /// The capacities the file's variant implies (format.md *Page model*, encrypted.md §1).
    cap: Cap,
}

impl SharedPaging {
    /// Wrap an open `pager` with a CLOCK pool of `capacity` leaves.
    pub(crate) fn new(pager: Pager, capacity: usize) -> Arc<SharedPaging> {
        SharedPaging::with_codec(pager, capacity, None)
    }

    /// [`new`](Self::new) for a file `codec` encrypts (`None`: plaintext).
    pub(crate) fn with_codec(
        pager: Pager,
        capacity: usize,
        codec: Option<Arc<crate::crypto::PageCodec>>,
    ) -> Arc<SharedPaging> {
        let batches = pager.batches_reads();
        let cap = Cap::of(pager.page_size(), codec.is_some());
        Arc::new(SharedPaging {
            pager: Mutex::new(pager),
            pool: BufferPool::new(capacity),
            batches,
            codec,
            cap,
        })
    }

    /// The database's page codec, when it is encrypted.
    pub(crate) fn codec(&self) -> Option<&Arc<crate::crypto::PageCodec>> {
        self.codec.as_ref()
    }

    /// The database's page capacities (plaintext or encrypted layout).
    pub(crate) fn cap(&self) -> Cap {
        self.cap
    }

    /// Read the body page `page` names and return its plaintext block: the stored bytes of a
    /// plaintext file, or an encrypted page checked against the pointer's tag and opened
    /// (encrypted.md §4) — the decryption runs outside the pager lock.
    pub(crate) fn read_body(&self, page: PageRef) -> Result<Vec<u8>> {
        let block = self
            .pager
            .lock()
            .expect("pager mutex poisoned")
            .read_block(page.page)?;
        self.open_block(page, block)
    }

    fn open_block(&self, page: PageRef, block: Vec<u8>) -> Result<Vec<u8>> {
        match &self.codec {
            None => Ok(block),
            Some(codec) => codec.open_body(page.page, &block, &page.tag),
        }
    }

    /// Fault the clean **leaf** at `page` to a resident node, through the buffer pool: a hit returns
    /// the cached `Arc`, a miss reads + decodes the page (with this table's `col_types`) and caches
    /// it, evicting under CLOCK if full. A page id belongs to exactly one table, so caching by global
    /// page id with a caller-supplied decoder is consistent (pager.md §4).
    pub(crate) fn fault_leaf(
        self: &Arc<Self>,
        page: PageRef,
        col_types: &Arc<Vec<ColType>>,
    ) -> Result<Arc<Node>> {
        let node = self.pool.get_or_load(page.page, || {
            let block = self.read_body(page)?;
            // Packed decode (packed-leaf.md §5): retain the page block + PAX directories, decode no
            // values — a spillable value is reconstructed as an unfetched reference only when a
            // column the query touches is read. The shared `col_types` `Arc` is cloned into the leaf
            // (O(1)), so a resident leaf adds no per-leaf column-type copy (§9).
            crate::format::decode_leaf_node(
                block,
                page,
                Arc::clone(col_types),
                Arc::downgrade(self),
                self.cap,
            )
        })?;
        // The pool is keyed by page alone, so a pointer into another tree's leaf can return a node
        // decoded for that tree's columns. Its directories were validated against those columns,
        // not these, so serving it would index regions that do not exist (storage.md §7).
        if let Some(packed) = &node.packed
            && !crate::format::same_leaf_shape(packed.col_types(), col_types)
        {
            return Err(crate::error::EngineError::new(
                crate::error::SqlState::DataCorrupted,
                "leaf page belongs to a different tree",
            ));
        }
        Ok(node)
    }

    /// Whether scans over this paging read ahead (spec/design/page-service.md §5.3).
    pub(crate) fn batches_reads(&self) -> bool {
        self.batches
    }

    /// Read ahead (spec/design/page-service.md §5.3): bring the leaves at `pages` that are not
    /// resident into the pool with one batched store read, decoded as a fault would decode them. At
    /// most a quarter of the pool is read ahead at once, so read-ahead cannot evict the leaf the scan
    /// is on. A failure is ignored: the fault that follows reads the page again and reports it.
    pub(crate) fn prefetch_leaves(
        self: &Arc<Self>,
        pages: &[PageRef],
        col_types: &Arc<Vec<ColType>>,
    ) {
        if !self.batches {
            return;
        }
        let limit = (self.pool.capacity() / 4).max(1);
        let missing: Vec<PageRef> = pages
            .iter()
            .copied()
            .filter(|page| !self.pool.contains(page.page))
            .take(limit)
            .collect();
        if missing.len() < 2 {
            return;
        }
        let indices: Vec<u32> = missing.iter().map(|p| p.page).collect();
        let Ok(blocks) = self
            .pager
            .lock()
            .expect("pager mutex poisoned")
            .read_blocks(&indices)
        else {
            return;
        };
        for (page, block) in missing.into_iter().zip(blocks) {
            let _ = self.pool.get_or_load(page.page, || {
                crate::format::decode_leaf_node(
                    self.open_block(page, block)?,
                    page,
                    Arc::clone(col_types),
                    Arc::downgrade(self),
                    self.cap,
                )
            });
        }
    }

    /// Lock the pager for the commit write path (file.rs `persist` pwrites dirty pages + meta).
    pub(crate) fn pager(&self) -> MutexGuard<'_, Pager> {
        self.pager.lock().expect("pager mutex poisoned")
    }

    /// Drop any stale pool entry for a rewritten page (bufferpool.rs `invalidate`): a no-op unless a
    /// reclaim domain reused a freed page id, in which case the pool's prior decode must be evicted. This
    /// marks and detaches an in-flight decode as well as dropping a resident entry, closing the stale
    /// insertion race and making post-commit callers load fresh now that decode runs outside the pool
    /// lock. Commit paths call this after writing; the reader-liveness watermark ensures an old
    /// snapshot's page is never reused under that reader.
    pub(crate) fn invalidate(&self, page: u32) {
        self.pool.invalidate(page);
    }

    /// Drop every resident leaf (a page client whose owner invalidated its whole cache,
    /// spec/pageservice/README.md §3.2).
    pub(crate) fn invalidate_all(&self) {
        self.pool.invalidate_all();
    }

    /// Make every later access through this paging fail with `error` and close its store: a
    /// page-service owner that released ownership no longer touches the file
    /// (spec/pageservice/README.md §6.5). Leaves already in the pool still serve the snapshots that
    /// cached them.
    pub(crate) fn revoke(&self, error: crate::error::EngineError) {
        let mut pager = self.pager();
        drop(pager.swap_store(Box::new(crate::blockstore::RevokedBlockStore(error))));
        pager.poison();
    }

    /// The pool's leaf-page capacity — carried over to the replacement pool when compaction
    /// reopens the database (spec/design/api.md §2.6).
    pub(crate) fn capacity(&self) -> usize {
        self.pool.capacity()
    }

    /// Re-bound the pool to `capacity` leaves, evicting until the resident set fits (a spilled temp
    /// domain, spec/design/temp-tables.md §6).
    pub(crate) fn set_capacity(&self, capacity: usize) {
        self.pool.set_capacity(capacity);
    }

    /// The number of leaf pages currently resident in the pool — the gauge the public
    /// [`crate::Engine::resident_leaves`] reports and the `cache_pages` budget bounds (P6.4c,
    /// spec/design/pager.md §3).
    pub(crate) fn resident_leaves(&self) -> usize {
        self.pool.resident()
    }
}
