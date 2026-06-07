package jed

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

// bpSlot is one resident page: its id, the cached node, and the CLOCK reference bit (set on access,
// cleared by the sweeping hand to grant a second chance).
type bpSlot struct {
	page       uint32
	node       *pnode
	referenced bool
}

// bufferPool is a bounded CLOCK cache from page id to a decoded leaf node.
type bufferPool struct {
	capacity int
	slots    []bpSlot
	index    map[uint32]int
	hand     int
}

// newBufferPool returns a pool holding at most capacity pages (clamped to ≥ 1).
func newBufferPool(capacity int) *bufferPool {
	if capacity < 1 {
		capacity = 1
	}
	return &bufferPool{capacity: capacity, index: make(map[uint32]int)}
}

// getOrLoad returns the decoded node for page: a cache hit returns the cached node (setting its
// reference bit), a miss calls load (read + decode the page), caches it — evicting one page under
// CLOCK if at capacity — and returns it. load's error propagates uncached.
func (p *bufferPool) getOrLoad(page uint32, load func() (*pnode, error)) (*pnode, error) {
	if i, ok := p.index[page]; ok {
		p.slots[i].referenced = true
		return p.slots[i].node, nil
	}
	node, err := load()
	if err != nil {
		return nil, err
	}
	p.insert(page, node)
	return node, nil
}

// insert adds a freshly-loaded page, evicting one under CLOCK if at capacity.
func (p *bufferPool) insert(page uint32, node *pnode) {
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

// resident is the number of pages currently resident — the bound the pool enforces (≤ capacity).
func (p *bufferPool) resident() int { return len(p.slots) }
