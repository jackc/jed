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
//! two locks are taken pool-then-pager and never the reverse (the commit write path locks only the
//! pager), so they cannot deadlock.

use std::sync::{Arc, Mutex, MutexGuard};

use crate::bufferpool::BufferPool;
use crate::error::Result;
use crate::pager::Pager;
use crate::pmap::Node;
use crate::types::ScalarType;

/// The default resident-leaf budget (pages). A handle-level memory-budget API is P6.4c; until then
/// this bounds the resident leaf set for every file-backed database. Sized so a modest working set
/// stays cache-resident while a larger-than-RAM file still pages within the bound (pager.md §3).
pub(crate) const DEFAULT_LEAF_POOL_PAGES: usize = 1024;

/// One database's pager + leaf buffer pool, shared (`Arc`) by all its stores and snapshots.
pub(crate) struct SharedPaging {
    pager: Mutex<Pager>,
    pool: Mutex<BufferPool<Node>>,
}

impl SharedPaging {
    /// Wrap an open `pager` with a CLOCK pool of `capacity` leaves.
    pub(crate) fn new(pager: Pager, capacity: usize) -> Arc<SharedPaging> {
        Arc::new(SharedPaging {
            pager: Mutex::new(pager),
            pool: Mutex::new(BufferPool::new(capacity)),
        })
    }

    /// Fault the clean **leaf** at `page` to a resident node, through the buffer pool: a hit returns
    /// the cached `Arc`, a miss reads + decodes the page (with this table's `col_types`) and caches
    /// it, evicting under CLOCK if full. A page id belongs to exactly one table, so caching by global
    /// page id with a caller-supplied decoder is consistent (pager.md §4).
    pub(crate) fn fault_leaf(&self, page: u32, col_types: &[ScalarType]) -> Result<Arc<Node>> {
        let mut pool = self.pool.lock().expect("buffer pool mutex poisoned");
        pool.get_or_load(page, || {
            let block = self
                .pager
                .lock()
                .expect("pager mutex poisoned")
                .read_block(page)?;
            crate::format::decode_leaf_node(&block, page, col_types)
        })
    }

    /// Lock the pager for the commit write path (file.rs `persist` pwrites dirty pages + meta).
    pub(crate) fn pager(&self) -> MutexGuard<'_, Pager> {
        self.pager.lock().expect("pager mutex poisoned")
    }

    /// The number of leaf pages currently resident in the pool — the gauge the public
    /// [`crate::Database::resident_leaves`] reports and the `cache_pages` budget bounds (P6.4c,
    /// spec/design/pager.md §3).
    pub(crate) fn resident_leaves(&self) -> usize {
        self.pool
            .lock()
            .expect("buffer pool mutex poisoned")
            .resident()
    }
}
