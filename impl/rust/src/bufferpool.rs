//! Bounded buffer pool — a CLOCK (second-chance) cache of decoded pages keyed by on-disk page id
//! (spec/design/pager.md §3). The demand-paging read path (P6.4b) faults a leaf page through
//! [`BufferPool::get_or_load`]; the pool bounds how many pages are resident at once, evicting under
//! CLOCK when full.
//!
//! **No pins.** Eviction only drops the cache *entry* — any in-flight `Arc` still keeps that node
//! alive, and a clean node is immutable so a re-load is a harmless duplicate (pager.md §4). A
//! traversal holds at most a root→leaf path, a bound of tree height, so the transient overage is
//! negligible.
//!
//! **Not a §8 byte contract** (pager.md §3): the pool changes *when* a page is resident, never
//! *what* a query observes (results and cost are invariant to it), so each core may implement it
//! idiomatically — like P5.3's per-core concurrency. Generic over the cached value so it is
//! decoupled from the node codec and unit-testable on its own.

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::Result;

/// One resident page: its id, the cached value, and the CLOCK reference bit (set on access, cleared
/// by the sweeping hand to grant a second chance).
struct Slot<T> {
    page_id: u32,
    value: Arc<T>,
    referenced: bool,
}

/// A bounded CLOCK cache from page id to a decoded value.
pub(crate) struct BufferPool<T> {
    capacity: usize,
    slots: Vec<Slot<T>>,
    index: HashMap<u32, usize>,
    hand: usize,
}

impl<T> BufferPool<T> {
    /// A pool holding at most `capacity` pages (clamped to ≥ 1).
    pub(crate) fn new(capacity: usize) -> Self {
        BufferPool {
            capacity: capacity.max(1),
            slots: Vec::new(),
            index: HashMap::new(),
            hand: 0,
        }
    }

    /// The decoded value for `page_id`: a cache **hit** returns the cached `Arc` (setting its
    /// reference bit), a **miss** calls `load` (read + decode the page), caches it — evicting one
    /// page under CLOCK if at capacity — and returns it. `load`'s error propagates uncached.
    pub(crate) fn get_or_load(
        &mut self,
        page_id: u32,
        load: impl FnOnce() -> Result<T>,
    ) -> Result<Arc<T>> {
        if let Some(&i) = self.index.get(&page_id) {
            self.slots[i].referenced = true;
            return Ok(self.slots[i].value.clone());
        }
        let value = Arc::new(load()?);
        self.insert(page_id, value.clone());
        Ok(value)
    }

    /// Insert a freshly-loaded page, evicting one under CLOCK if at capacity.
    fn insert(&mut self, page_id: u32, value: Arc<T>) {
        if self.slots.len() < self.capacity {
            self.index.insert(page_id, self.slots.len());
            self.slots.push(Slot {
                page_id,
                value,
                referenced: false,
            });
            return;
        }
        let victim = self.evict_slot();
        self.index.remove(&self.slots[victim].page_id);
        self.index.insert(page_id, victim);
        self.slots[victim] = Slot {
            page_id,
            value,
            referenced: false,
        };
    }

    /// Advance the CLOCK hand, clearing the reference bit of each page it passes (a second chance),
    /// and return the index of the first unreferenced page to evict. Terminates within two sweeps
    /// (every page's bit is cleared on the first pass).
    fn evict_slot(&mut self) -> usize {
        loop {
            let i = self.hand;
            self.hand = (self.hand + 1) % self.slots.len();
            if self.slots[i].referenced {
                self.slots[i].referenced = false;
            } else {
                return i;
            }
        }
    }

    /// The number of pages currently resident — the bound the pool enforces (`≤ capacity`), surfaced
    /// publicly via [`crate::Engine::resident_leaves`] (P6.4c, spec/design/pager.md §3).
    pub(crate) fn resident(&self) -> usize {
        self.slots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A loader that counts how many times it actually read a page (a cache miss).
    fn counting<'a>(loads: &'a Cell<u32>, v: u32) -> impl FnOnce() -> Result<u32> + 'a {
        move || {
            loads.set(loads.get() + 1);
            Ok(v)
        }
    }

    #[test]
    fn hit_returns_cached_without_reloading() {
        let mut pool: BufferPool<u32> = BufferPool::new(4);
        let loads = Cell::new(0);
        assert_eq!(*pool.get_or_load(7, counting(&loads, 70)).unwrap(), 70);
        assert_eq!(*pool.get_or_load(7, counting(&loads, 70)).unwrap(), 70);
        assert_eq!(loads.get(), 1, "second access is a cache hit");
        assert_eq!(pool.resident(), 1);
    }

    #[test]
    fn resident_set_never_exceeds_capacity() {
        let mut pool: BufferPool<u32> = BufferPool::new(3);
        let loads = Cell::new(0);
        for p in 0..100u32 {
            pool.get_or_load(p, counting(&loads, p)).unwrap();
            assert!(
                pool.resident() <= 3,
                "resident {} exceeds capacity",
                pool.resident()
            );
        }
        assert_eq!(
            loads.get(),
            100,
            "every distinct page was a miss (none re-cached)"
        );
    }

    #[test]
    fn clock_gives_a_referenced_page_a_second_chance() {
        // Fill {0,1,2}; touch 0 (sets its ref bit); inserting 3 should evict 1 (the first
        // unreferenced under the hand), sparing the recently-touched 0.
        let mut pool: BufferPool<u32> = BufferPool::new(3);
        let loads = Cell::new(0);
        for p in 0..3u32 {
            pool.get_or_load(p, counting(&loads, p)).unwrap();
        }
        pool.get_or_load(0, counting(&loads, 0)).unwrap(); // hit → ref bit on 0
        pool.get_or_load(3, counting(&loads, 3)).unwrap(); // miss → evicts 1
        assert_eq!(loads.get(), 4);
        // 0 survived (a hit, no reload); 1 was evicted (a reload).
        let before = loads.get();
        pool.get_or_load(0, counting(&loads, 0)).unwrap();
        assert_eq!(loads.get(), before, "0 was spared — still cached");
        pool.get_or_load(1, counting(&loads, 1)).unwrap();
        assert_eq!(loads.get(), before + 1, "1 was evicted — reloaded");
    }

    #[test]
    fn capacity_one_evicts_every_time() {
        let mut pool: BufferPool<u32> = BufferPool::new(1);
        let loads = Cell::new(0);
        pool.get_or_load(1, counting(&loads, 1)).unwrap();
        pool.get_or_load(2, counting(&loads, 2)).unwrap();
        pool.get_or_load(1, counting(&loads, 1)).unwrap(); // 1 was evicted by 2 → reload
        assert_eq!(loads.get(), 3);
        assert_eq!(pool.resident(), 1);
    }
}
