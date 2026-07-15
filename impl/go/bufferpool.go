package jed

import "sync"

// Bounded buffer pool — a CLOCK (second-chance) cache of decoded leaf nodes keyed by on-disk page id
// (spec/design/pager.md §3). The demand-paging read path (P6.4b) faults a leaf through getOrLoad; the
// pool bounds how many leaves are resident at once, evicting under CLOCK when full.
//
// No pins. Eviction only drops the cache entry — any node still referenced by a live tree or an
// in-flight read stays alive via GC, and a clean node is immutable so a re-load is a harmless
// duplicate (pager.md §4). A traversal holds at most a root→leaf path, a bound of tree height.
//
// Not a §8 byte contract (pager.md §3): the pool changes WHEN a page is resident, never WHAT a query
// observes (results and cost are invariant to it), so each core may implement it idiomatically — like
// P5.3's per-core concurrency.

// maxInitialBufferPoolIndexCapacity bounds the eager page-id map reservation. 8,192 entries cover
// the diagnosed 6,900-leaf cold population without turning the default 32,768-leaf cache ceiling (or
// a caller's much larger budget) into an oversized allocation before a leaf is touched.
const maxInitialBufferPoolIndexCapacity = 8 * 1024

func initialBufferPoolIndexCapacity(capacity int) int {
	return min(capacity, maxInitialBufferPoolIndexCapacity)
}

// bpSlot is one resident page: its id, the cached node, and the CLOCK reference bit (set on access,
// cleared by the sweeping hand to grant a second chance).
type bpSlot struct {
	page       uint32
	node       *pnode
	referenced bool
}

// bpLoad is one page load shared by every caller that misses on that page while its physical read
// and decode are in progress. Closing done publishes node/err to followers. invalidated closes the
// unlock-load-recheck race: a commit that rewrites this page id prevents the old decode from entering
// the cache after checksum/PAX parsing completes.
type bpLoad struct {
	done        chan struct{}
	node        *pnode
	err         error
	invalidated bool
}

// bufferPool is a bounded CLOCK cache from page id to a decoded leaf node.
type bufferPool struct {
	mu       sync.Mutex
	capacity int
	slots    []bpSlot
	index    map[uint32]int
	hand     int
	loading  map[uint32]*bpLoad
}

// newBufferPool returns a pool holding at most capacity pages (clamped to ≥ 1).
func newBufferPool(capacity int) *bufferPool {
	if capacity < 1 {
		capacity = 1
	}
	return &bufferPool{
		capacity: capacity,
		index:    make(map[uint32]int, initialBufferPoolIndexCapacity(capacity)),
		loading:  make(map[uint32]*bpLoad),
	}
}

// getOrLoad returns the decoded node for page: a cache hit returns the cached node (setting its
// reference bit), a miss calls load (read + decode the page), caches it — evicting one page under
// CLOCK if at capacity — and returns it. load's error propagates uncached.
func (p *bufferPool) getOrLoad(page uint32, load func() (*pnode, error)) (*pnode, error) {
	p.mu.Lock()
	if i, ok := p.index[page]; ok {
		p.slots[i].referenced = true
		node := p.slots[i].node
		p.mu.Unlock()
		return node, nil
	}
	if flight, ok := p.loading[page]; ok {
		p.mu.Unlock()
		<-flight.done
		return flight.node, flight.err
	}
	flight := &bpLoad{done: make(chan struct{})}
	p.loading[page] = flight
	p.mu.Unlock()

	// Deliberately outside the pool mutex: distinct page faults can perform checksum and PAX parsing
	// concurrently. sharedPaging separately serializes the short physical read against commit.
	node, err := load()

	p.mu.Lock()
	if err == nil && !flight.invalidated {
		p.insertLocked(page, node)
	}
	flight.node = node
	flight.err = err
	if p.loading[page] == flight {
		delete(p.loading, page)
	}
	close(flight.done)
	p.mu.Unlock()
	return flight.node, flight.err
}

// insert adds a freshly-loaded page, evicting one under CLOCK if at capacity.
func (p *bufferPool) insert(page uint32, node *pnode) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.insertLocked(page, node)
}

func (p *bufferPool) insertLocked(page uint32, node *pnode) {
	if len(p.slots) < p.capacity {
		p.index[page] = len(p.slots)
		p.slots = append(p.slots, bpSlot{page: page, node: node})
		return
	}
	victim := p.evictSlot()
	delete(p.index, p.slots[victim].page)
	p.index[page] = victim
	p.slots[victim] = bpSlot{page: page, node: node}
}

// evictSlot advances the CLOCK hand, clearing the reference bit of each page it passes (a second
// chance), and returns the index of the first unreferenced page to evict. Terminates within two
// sweeps (every page's bit is cleared on the first pass).
func (p *bufferPool) evictSlot() int {
	for {
		i := p.hand
		p.hand = (p.hand + 1) % len(p.slots)
		if p.slots[i].referenced {
			p.slots[i].referenced = false
		} else {
			return i
		}
	}
}

// invalidate drops any cached entry for page — required when a commit REWRITES a page in place, which
// happens when within-session compaction (a reclaim domain — temp, or an in-memory database with
// reclamation on) hands a freed page id back to a new node: the pool caches by page id, so the stale
// decode of the page's PRIOR content must be evicted or a later fault returns old rows. A no-op when the
// page is not resident — the common case, since a copy-on-write commit without reuse only ever writes
// fresh, never-cached high-water pages (so the main file path pays only a map lookup). An in-flight
// decode is marked non-cacheable and detached so it cannot reinsert stale content — or attract a
// post-commit caller — after this invalidation. Callers already waiting on it still receive its result.
func (p *bufferPool) invalidate(page uint32) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.invalidateLocked(page)
	if flight, ok := p.loading[page]; ok {
		flight.invalidated = true
		delete(p.loading, page)
	}
}

func (p *bufferPool) invalidateLocked(page uint32) {
	i, ok := p.index[page]
	if !ok {
		return
	}
	delete(p.index, page)
	// Swap the last slot into the hole so the slice stays dense (capacity accounting + the CLOCK hand
	// stay well-formed), then shrink.
	last := len(p.slots) - 1
	if i != last {
		moved := p.slots[last]
		p.slots[i] = moved
		p.index[moved.page] = i
	}
	p.slots = p.slots[:last]
	if len(p.slots) == 0 {
		p.hand = 0
	} else {
		p.hand %= len(p.slots)
	}
}

// resident is the number of pages currently resident — the bound the pool enforces (≤ capacity).
func (p *bufferPool) resident() int {
	p.mu.Lock()
	defer p.mu.Unlock()
	return len(p.slots)
}
