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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::error::Result;

/// Bound the eager page-id index reservation. 8,192 entries cover the diagnosed 6,900-leaf cold
/// population without turning the default 32,768-leaf cache ceiling (or a caller's much larger
/// budget) into an oversized allocation before a leaf is touched.
const MAX_INITIAL_INDEX_CAPACITY: usize = 8 * 1024;

fn initial_index_capacity(capacity: usize) -> usize {
    capacity.min(MAX_INITIAL_INDEX_CAPACITY)
}

/// One resident page: its id, the cached value, and the CLOCK reference bit (set on access, cleared
/// by the sweeping hand to grant a second chance).
struct Slot<T> {
    page_id: u32,
    value: Arc<T>,
    referenced: bool,
}

/// One page load shared by every caller that misses on the same page while its physical read and
/// decode are in progress. The result is published once; followers wait without holding the pool
/// mutex. Errors are shared too, so one corrupt/read-failing page still performs one load attempt.
struct LoadFlight<T> {
    result: Mutex<Option<Result<Arc<T>>>>,
    ready: Condvar,
    invalidated: AtomicBool,
}

impl<T> LoadFlight<T> {
    fn new() -> Self {
        LoadFlight {
            result: Mutex::new(None),
            ready: Condvar::new(),
            invalidated: AtomicBool::new(false),
        }
    }

    fn wait(&self) -> Result<Arc<T>> {
        let mut result = self.result.lock().expect("buffer-pool flight poisoned");
        loop {
            if let Some(result) = result.as_ref() {
                return result.clone();
            }
            result = self
                .ready
                .wait(result)
                .expect("buffer-pool flight poisoned");
        }
    }

    fn complete(&self, result: Result<Arc<T>>) {
        *self.result.lock().expect("buffer-pool flight poisoned") = Some(result);
        self.ready.notify_all();
    }

    fn invalidate(&self) {
        self.invalidated.store(true, Ordering::Release);
    }

    fn is_invalidated(&self) -> bool {
        self.invalidated.load(Ordering::Acquire)
    }
}

/// The short-critical-section CLOCK state. Physical read, checksum, and PAX parsing never run while
/// this mutex is held.
struct PoolState<T> {
    capacity: usize,
    slots: Vec<Slot<T>>,
    index: HashMap<u32, usize>,
    hand: usize,
    loading: HashMap<u32, Arc<LoadFlight<T>>>,
}

/// A bounded CLOCK cache from page id to a decoded value, with per-page single-flight misses.
pub(crate) struct BufferPool<T> {
    state: Mutex<PoolState<T>>,
}

impl<T> BufferPool<T> {
    /// A pool holding at most `capacity` pages (clamped to ≥ 1).
    pub(crate) fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        BufferPool {
            state: Mutex::new(PoolState {
                capacity,
                slots: Vec::new(),
                index: HashMap::with_capacity(initial_index_capacity(capacity)),
                hand: 0,
                loading: HashMap::new(),
            }),
        }
    }

    /// The decoded value for `page_id`: a cache **hit** returns the cached `Arc` (setting its
    /// reference bit), a **miss** calls `load` (read + decode the page), caches it — evicting one
    /// page under CLOCK if at capacity — and returns it. `load`'s error propagates uncached.
    pub(crate) fn get_or_load(
        &self,
        page_id: u32,
        load: impl FnOnce() -> Result<T>,
    ) -> Result<Arc<T>> {
        let (flight, leader) = {
            let mut state = self.state.lock().expect("buffer pool mutex poisoned");
            if let Some(&i) = state.index.get(&page_id) {
                state.slots[i].referenced = true;
                return Ok(state.slots[i].value.clone());
            }
            if let Some(flight) = state.loading.get(&page_id) {
                (Arc::clone(flight), false)
            } else {
                let flight = Arc::new(LoadFlight::new());
                state.loading.insert(page_id, Arc::clone(&flight));
                (flight, true)
            }
        };

        if !leader {
            return flight.wait();
        }

        // Deliberately outside the pool lock: distinct page faults can perform their checksum and PAX
        // parse concurrently. The pager separately serializes the short physical read against commit.
        let result = load().map(Arc::new);
        {
            let mut state = self.state.lock().expect("buffer pool mutex poisoned");
            let is_current = state
                .loading
                .get(&page_id)
                .is_some_and(|current| Arc::ptr_eq(current, &flight));
            if !flight.is_invalidated() {
                if let Ok(value) = &result {
                    state.insert(page_id, Arc::clone(value));
                }
            }
            // Publish before removing the flight while the state mutex excludes a new leader. A new
            // caller then either hits the inserted value or starts a fresh load after an error /
            // invalidation; existing followers all observe this leader's one result.
            flight.complete(result.clone());
            if is_current {
                state.loading.remove(&page_id);
            }
        }
        result
    }

    /// The number of pages currently resident — the bound the pool enforces (`≤ capacity`), surfaced
    /// publicly via [`crate::Engine::resident_leaves`] (P6.4c, spec/design/pager.md §3).
    pub(crate) fn resident(&self) -> usize {
        self.state
            .lock()
            .expect("buffer pool mutex poisoned")
            .slots
            .len()
    }

    /// Drop any cached entry for `page`, then mark and detach an in-flight load. Detaching prevents a
    /// post-commit caller from joining the old load; marking it non-cacheable closes the
    /// unlock-load-recheck race, so the old decode cannot be inserted after commit invalidation.
    /// Callers already waiting on that load still receive its immutable result.
    pub(crate) fn invalidate(&self, page: u32) {
        let mut state = self.state.lock().expect("buffer pool mutex poisoned");
        state.invalidate(page);
        if let Some(flight) = state.loading.remove(&page) {
            flight.invalidate();
        }
    }
}

impl<T> PoolState<T> {
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

    /// Drop any cached entry for `page` — required when a commit REWRITES a page in place, which happens
    /// when within-session compaction (a reclaim domain — temp, or an in-memory database with reclamation
    /// on) hands a freed page id back to a new node: the pool caches by page id, so the stale decode of
    /// the page's PRIOR content must be evicted or a later fault returns old rows. A no-op when the page
    /// is not resident — the common case, since a copy-on-write commit without reuse only ever writes
    /// fresh, never-cached high-water pages (so the main file path pays only a map lookup).
    fn invalidate(&mut self, page: u32) {
        let Some(i) = self.index.remove(&page) else {
            return;
        };
        // Swap the last slot into the hole so the Vec stays dense (capacity accounting + the CLOCK hand
        // stay well-formed), then pop.
        let last = self.slots.len() - 1;
        if i != last {
            self.slots.swap(i, last);
            self.index.insert(self.slots[i].page_id, i);
        }
        self.slots.pop();
        if self.slots.is_empty() {
            self.hand = 0;
        } else {
            self.hand %= self.slots.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    /// A loader that counts how many times it actually read a page (a cache miss).
    fn counting<'a>(loads: &'a Cell<u32>, v: u32) -> impl FnOnce() -> Result<u32> + 'a {
        move || {
            loads.set(loads.get() + 1);
            Ok(v)
        }
    }

    #[test]
    fn hit_returns_cached_without_reloading() {
        let pool: BufferPool<u32> = BufferPool::new(4);
        let loads = Cell::new(0);
        assert_eq!(*pool.get_or_load(7, counting(&loads, 70)).unwrap(), 70);
        assert_eq!(*pool.get_or_load(7, counting(&loads, 70)).unwrap(), 70);
        assert_eq!(loads.get(), 1, "second access is a cache hit");
        assert_eq!(pool.resident(), 1);
    }

    #[test]
    fn resident_set_never_exceeds_capacity() {
        let pool: BufferPool<u32> = BufferPool::new(3);
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
        let pool: BufferPool<u32> = BufferPool::new(3);
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
        let pool: BufferPool<u32> = BufferPool::new(1);
        let loads = Cell::new(0);
        pool.get_or_load(1, counting(&loads, 1)).unwrap();
        pool.get_or_load(2, counting(&loads, 2)).unwrap();
        pool.get_or_load(1, counting(&loads, 1)).unwrap(); // 1 was evicted by 2 → reload
        assert_eq!(loads.get(), 3);
        assert_eq!(pool.resident(), 1);
    }

    #[test]
    fn initial_index_reservation_is_bounded() {
        assert_eq!(initial_index_capacity(1), 1);
        assert_eq!(initial_index_capacity(6_900), 6_900);
        assert_eq!(initial_index_capacity(8_192), 8_192);
        assert_eq!(initial_index_capacity(usize::MAX), 8_192);

        let pool = BufferPool::<u32>::new(6_900);
        assert!(
            pool.state.lock().unwrap().index.capacity() >= 6_900,
            "constructor must apply the initial index reservation"
        );

        // The diagnosed million-row ramp touches about 6,900 leaves under the default 32,768-leaf
        // pool. Pin the allocation property directly: that population must not grow/rehash the
        // eagerly reserved page-id index.
        let value = Arc::new(0u32);
        let pool = BufferPool::new(32_768);
        let mut state = pool.state.lock().unwrap();
        let reserved = state.index.capacity();
        for page in 0..6_900 {
            state.insert(page, Arc::clone(&value));
        }
        assert_eq!(
            state.index.capacity(),
            reserved,
            "diagnosed cold population must not reallocate the page-id index",
        );
    }

    #[test]
    fn distinct_page_loaders_run_outside_the_pool_lock() {
        let pool = Arc::new(BufferPool::new(4));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let (started_tx, started_rx) = mpsc::channel();
        let mut handles = Vec::new();

        for page in [11u32, 12] {
            let pool = Arc::clone(&pool);
            let release = Arc::clone(&release);
            let started_tx = started_tx.clone();
            handles.push(thread::spawn(move || {
                pool.get_or_load(page, || {
                    started_tx.send(page).unwrap();
                    let (lock, ready) = &*release;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = ready.wait(released).unwrap();
                    }
                    Ok(page)
                })
                .unwrap()
            }));
        }
        drop(started_tx);

        let first = started_rx.recv_timeout(Duration::from_secs(2));
        let second = started_rx.recv_timeout(Duration::from_secs(2));
        {
            let (lock, ready) = &*release;
            *lock.lock().unwrap() = true;
            ready.notify_all();
        }
        let values: Vec<u32> = handles.into_iter().map(|h| *h.join().unwrap()).collect();

        assert!(first.is_ok(), "no page loader entered");
        assert!(
            second.is_ok(),
            "a distinct page loader was serialized behind the pool lock"
        );
        assert_eq!(values, vec![11, 12]);
    }

    #[test]
    fn same_page_miss_is_single_flight() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let pool = Arc::new(BufferPool::new(4));
        let loads = Arc::new(AtomicUsize::new(0));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let (leader_tx, leader_rx) = mpsc::channel();

        let leader = {
            let pool = Arc::clone(&pool);
            let loads = Arc::clone(&loads);
            let release = Arc::clone(&release);
            thread::spawn(move || {
                pool.get_or_load(7, || {
                    loads.fetch_add(1, Ordering::SeqCst);
                    leader_tx.send(()).unwrap();
                    let (lock, ready) = &*release;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = ready.wait(released).unwrap();
                    }
                    Ok(70)
                })
                .unwrap()
            })
        };
        leader_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let follower = {
            let pool = Arc::clone(&pool);
            let loads = Arc::clone(&loads);
            thread::spawn(move || {
                pool.get_or_load(7, || {
                    loads.fetch_add(1, Ordering::SeqCst);
                    Ok(71)
                })
                .unwrap()
            })
        };
        {
            let (lock, ready) = &*release;
            *lock.lock().unwrap() = true;
            ready.notify_all();
        }

        let first = leader.join().unwrap();
        let second = follower.join().unwrap();
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(*first, 70);
    }

    #[test]
    fn invalidate_during_load_prevents_stale_reinsertion() {
        let pool = Arc::new(BufferPool::new(4));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let (started_tx, started_rx) = mpsc::channel();

        let loading = {
            let pool = Arc::clone(&pool);
            let release = Arc::clone(&release);
            thread::spawn(move || {
                pool.get_or_load(9, || {
                    started_tx.send(()).unwrap();
                    let (lock, ready) = &*release;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = ready.wait(released).unwrap();
                    }
                    Ok(90)
                })
                .unwrap()
            })
        };
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        pool.invalidate(9);

        // A caller from the newly published snapshot must not join the detached old flight. It loads
        // fresh content immediately, even while the old caller is still parsing its immutable bytes.
        let fresh = pool.get_or_load(9, || Ok(91)).unwrap();
        assert_eq!(*fresh, 91);
        {
            let (lock, ready) = &*release;
            *lock.lock().unwrap() = true;
            ready.notify_all();
        }
        assert_eq!(*loading.join().unwrap(), 90);
        assert_eq!(
            *pool.get_or_load(9, || Ok(92)).unwrap(),
            91,
            "invalidated old decode must not replace the fresh cache entry"
        );
    }
}
