package jed

import "sync"

// Bounded buffer pool — a CLOCK (second-chance) cache of decoded leaf nodes keyed by on-disk page id
// (spec/design/pager.md §3). The demand-paging read path (P6.4b) faults a leaf through getOrLoad; the
// pool bounds the bytes resident leaves hold (cache_bytes), each leaf charged what this core holds for
// it (pnode.residentBytes), evicting under CLOCK until a new leaf fits. At least one leaf stays
// resident whatever the budget.
//
// No pins. Eviction only drops the cache entry — any node still referenced by a live tree or an
// in-flight read stays alive via GC, and a clean node is immutable so a re-load is a harmless
// duplicate (pager.md §4). A traversal holds at most a root→leaf path, a bound of tree height.
//
// Not a §8 byte contract (pager.md §3): the pool changes WHEN a page is resident, never WHAT a query
// observes (results and cost are invariant to it), so each core may implement it idiomatically — like
// P5.3's per-core concurrency.

// maxInitialBufferPoolIndexCapacity bounds the eager page-id map reservation. 8,192 entries cover
// the diagnosed 6,900-leaf cold population without turning the default 256 MiB budget (or a caller's
// much larger one) into an oversized allocation before a leaf is touched.
const maxInitialBufferPoolIndexCapacity = 8 * 1024

// bpIndexEntryBytes is the bytes a page-sized leaf is assumed to hold when sizing the initial index.
const bpIndexEntryBytes = 8 << 10

func initialBufferPoolIndexCapacity(budget int) int {
	return min(max(budget/bpIndexEntryBytes, 1), maxInitialBufferPoolIndexCapacity)
}

// bpSlot is one resident page: its id, the cached node, the bytes it is charged, and the CLOCK
// reference bit (set on access, cleared by the sweeping hand to grant a second chance).
type bpSlot struct {
	page       uint32
	node       *pnode
	bytes      int
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

// bufferPool is a CLOCK cache from page id to a decoded leaf node, bounded in bytes.
type bufferPool struct {
	mu sync.Mutex
	// budget is the most bytes the resident leaves are charged together (≥ 1); used is their sum.
	budget  int
	used    int
	slots   []bpSlot
	index   map[uint32]int
	hand    int
	loading map[uint32]*bpLoad
}

// newBufferPool returns a pool whose resident leaves hold at most budget bytes (clamped to ≥ 1), or
// one leaf when a single leaf is larger.
func newBufferPool(budget int) *bufferPool {
	if budget < 1 {
		budget = 1
	}
	return &bufferPool{
		budget:  budget,
		index:   make(map[uint32]int, initialBufferPoolIndexCapacity(budget)),
		loading: make(map[uint32]*bpLoad),
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

// contains reports whether page is resident or being loaded.
func (p *bufferPool) contains(page uint32) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	_, resident := p.index[page]
	_, loading := p.loading[page]
	return resident || loading
}

// insert adds a freshly-loaded page, evicting under CLOCK until it fits.
func (p *bufferPool) insert(page uint32, node *pnode) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.insertLocked(page, node)
}

func (p *bufferPool) insertLocked(page uint32, node *pnode) {
	bytes := node.residentBytes()
	p.fitLocked(bytes)
	p.index[page] = len(p.slots)
	p.slots = append(p.slots, bpSlot{page: page, node: node, bytes: bytes})
	p.used += bytes
}

// fitLocked evicts under CLOCK until bytes more fit the budget, or nothing is left to evict: a leaf
// larger than the whole budget is still admitted, alone.
func (p *bufferPool) fitLocked(bytes int) {
	for len(p.slots) > 0 && p.used+bytes > p.budget {
		p.invalidateLocked(p.slots[p.evictSlot()].page)
	}
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

// invalidateAll drops every cached page and detaches every in-flight load — a page client told by the
// owner that its whole cache may be stale (spec/pageservice/README.md §3.2).
func (p *bufferPool) invalidateAll() {
	p.mu.Lock()
	defer p.mu.Unlock()
	clear(p.slots)
	p.slots = p.slots[:0]
	p.used = 0
	p.index = make(map[uint32]int, initialBufferPoolIndexCapacity(p.budget))
	p.hand = 0
	for page, flight := range p.loading {
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
	p.used -= p.slots[i].bytes
	// Swap the last slot into the hole so the slice stays dense (the CLOCK hand stays well-formed),
	// then shrink.
	last := len(p.slots) - 1
	if i != last {
		moved := p.slots[last]
		p.slots[i] = moved
		p.index[moved.page] = i
	}
	p.slots[last] = bpSlot{} // no evicted node stays reachable from the slice's spare capacity
	p.slots = p.slots[:last]
	if len(p.slots) == 0 {
		p.hand = 0
	} else {
		p.hand %= len(p.slots)
	}
}

// capacity is the pool's byte budget (set at construction, re-bounded only by setCapacity).
func (p *bufferPool) capacity() int {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.budget
}

// setCapacity re-bounds the pool to budget bytes (clamped to ≥ 1), evicting under CLOCK until the
// resident set fits, keeping at least one leaf — a spilled temp domain's pool tightens from pinned to
// tempBuffers (spec/design/temp-tables.md §6). An in-flight load still inserts afterward, evicting as
// usual.
func (p *bufferPool) setCapacity(budget int) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if budget < 1 {
		budget = 1
	}
	p.budget = budget
	for len(p.slots) > 1 && p.used > p.budget {
		p.invalidateLocked(p.slots[p.evictSlot()].page)
	}
}

// resident is the number of pages currently resident.
func (p *bufferPool) resident() int {
	p.mu.Lock()
	defer p.mu.Unlock()
	return len(p.slots)
}

// residentBytes is the bytes the resident leaves are charged together — the bound the pool enforces
// (≤ budget, or one leaf).
func (p *bufferPool) residentBytes() int {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.used
}
