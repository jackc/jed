package jed

// Shared paging context for a file-backed database (spec/design/pager.md §2/§3): the open pager plus
// the bounded leaf bufferPool, shared by every table store and snapshot of one database. Page ids are
// file-global (one page space per file), so there is exactly ONE pool and one pager per database; a
// TableStore/Snapshot clone shares the same *sharedPaging pointer.
//
// The read path faults a clean leaf through faultLeaf: a pool hit returns the cached node, a miss
// reads the page through the pager, decodes it (the node codec, format.go) and caches it, evicting
// under CLOCK when full. No pins (pager.md §4): eviction only drops the cache entry, and a clean leaf
// is immutable so any node still referenced stays alive (GC) and a re-load is a harmless duplicate.
//
// Not a §8 byte contract (pager.md §3): the pool changes WHEN a page is resident, never WHAT a query
// observes — so each core realizes it idiomatically (like P5.3's per-core concurrency). One mutex
// guards both the pager and the pool, so the read (fault) and commit (write) paths cannot race.

import "sync"

// DefaultCacheBytes is the default memory budget for the resident leaf cache, in bytes (256 MiB) — the
// OpenOptions.CacheBytes default (spec/design/pager.md §3, api.md §2.1). Sized so the dominant case —
// a RAM-sized database (CLAUDE.md §9) — stays fully cache-resident under the default; stated in bytes
// so the budget does not silently scale with a file's page size. Converted to a leaf-page capacity by
// cacheLeaves.
const DefaultCacheBytes = 256 * 1024 * 1024

// cacheLeaves converts a byte budget to a resident-leaf-page capacity for a file of pageSize bytes:
// max(1, cacheBytes / pageSize) (pager.md §3). The max(1, …) floor keeps one leaf resident even when
// cacheBytes < pageSize — the minimum to walk a root→leaf path. The divisor is clamped to ≥ 1 so a
// malformed pageSize = 0 cannot divide by zero (the loader rejects it separately as corrupt — format.go).
func cacheLeaves(cacheBytes int, pageSize uint32) int {
	div := int(pageSize)
	if div < 1 {
		div = 1
	}
	if n := cacheBytes / div; n > 1 {
		return n
	}
	return 1
}

// sharedPaging is one database's pager + leaf buffer pool, shared (pointer) by all its stores and
// snapshots.
type sharedPaging struct {
	mu   sync.Mutex
	pgr  *pager
	pool *bufferPool
}

// newSharedPaging wraps an open pager with a CLOCK pool of capacity leaves.
func newSharedPaging(p *pager, capacity int) *sharedPaging {
	return &sharedPaging{pgr: p, pool: newBufferPool(capacity)}
}

// faultLeaf faults the clean leaf at page to a resident node, through the buffer pool: a hit returns
// the cached node, a miss reads + decodes the page (with this table's colTypes) and caches it,
// evicting under CLOCK if full. A page id belongs to exactly one table, so caching by global page id
// with a caller-supplied decoder is consistent (pager.md §4).
func (s *sharedPaging) faultLeaf(page uint32, colTypes []ColType) (*pnode, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.pool.getOrLoad(page, func() (*pnode, error) {
		block, err := s.pgr.readBlock(page)
		if err != nil {
			return nil, err
		}
		// Lazy decode (spec/design/large-values.md §14): an external/compressed value stays an
		// unfetched reference — no chain read, no decompression. The scan layer resolves the
		// columns a query touches through readBlock below.
		return decodeLeafNode(block, page, colTypes)
	})
}

// readBlock reads one page through the shared pager under the paging lock — the overflow-chain
// read path the scan layer's read-on-touch resolution uses (large-values.md §14); concurrent
// readers may resolve while another faults a leaf, so the same mutex serializes both.
func (s *sharedPaging) readBlock(page uint32) ([]byte, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.pgr.readBlock(page)
}

// withPager runs fn with the pager locked — the commit write path (file.go persist pwrites dirty
// pages + meta) takes the same lock the fault path does, so they cannot race.
func (s *sharedPaging) withPager(fn func(*pager) error) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return fn(s.pgr)
}

// close closes the backing file (Database.Close).
func (s *sharedPaging) close() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.pgr.close()
}

// residentLeaves is the number of leaf pages currently resident in the pool — the bound the
// demand-paging tests assert stays below the budget even for a database far larger than it. P6.4c
// promotes it to the public memory-budget surface.
func (s *sharedPaging) residentLeaves() int {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.pool.resident()
}
