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
// observes — so each core realizes it idiomatically (like P5.3's per-core concurrency). The pool
// internally locks only hit/flight/CLOCK bookkeeping; pagerMu separately serializes physical reads
// against commit writes. Checksum + PAX parsing run outside both locks.

import "sync"

// DefaultCacheBytes is the default memory budget for the resident leaf cache, in bytes (256 MiB) — the
// OpenOptions.CacheBytes default (spec/design/pager.md §3, api.md §2.1). Sized so the dominant case —
// a RAM-sized database (CLAUDE.md §9) — stays fully cache-resident under the default; stated in bytes
// so the budget does not silently scale with a file's page size. Converted to a leaf-page capacity by
// cacheLeaves.
const defaultCacheBytes = 256 * 1024 * 1024

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
	pagerMu sync.Mutex
	pgr     *pager
	pool    *bufferPool
	// batches is set when the store fetches several pages in one request (a page client over the
	// stream transport): scans read ahead (prefetchLeaves).
	batches bool
}

// newSharedPaging wraps an open pager with a CLOCK pool of capacity leaves.
func newSharedPaging(p *pager, capacity int) *sharedPaging {
	return &sharedPaging{pgr: p, pool: newBufferPool(capacity), batches: p.batchesReads()}
}

// codec is the database's page codec when its file is encrypted (spec/fileformat/encrypted.md): every
// body page read through this context is opened, and every commit page sealed, under its keys. nil for
// a plaintext file and every in-memory or temp store. It lives on the pager, which selects the meta
// slot it authenticates.
func (s *sharedPaging) codec() *pageCodec { return s.pgr.codec }

// cap is the page capacities the file's variant implies (format.md *Page model*, encrypted.md §1).
func (s *sharedPaging) cap() pageCap { return capOf(s.pgr.pageSize, s.pgr.codec != nil) }

// readBody reads the body page ref names and returns its plaintext block: the stored bytes of a
// plaintext file, or an encrypted page checked against the pointer's tag and opened (encrypted.md §4)
// — the decryption runs outside the pager lock.
func (s *sharedPaging) readBody(ref pageRef) ([]byte, error) {
	block, err := s.readBlock(ref.page)
	if err != nil {
		return nil, err
	}
	return s.openBlock(ref, block)
}

// readBodyUnlocked is readBody without the pager lock — for the walks that read pager blocks unlocked
// (the open-time load and the commit-time reachability walks).
func (s *sharedPaging) readBodyUnlocked(ref pageRef) ([]byte, error) {
	block, err := s.pgr.readBlock(ref.page)
	if err != nil {
		return nil, err
	}
	return s.openBlock(ref, block)
}

func (s *sharedPaging) openBlock(ref pageRef, block []byte) ([]byte, error) {
	codec := s.pgr.codec
	if codec == nil {
		return block, nil
	}
	return codec.openBody(ref.page, block, ref.tag)
}

// faultLeaf faults the clean leaf at page to a resident node, through the buffer pool: a hit returns
// the cached node, a miss reads + decodes the page (with this table's colTypes) and caches it,
// evicting under CLOCK if full. A page id belongs to exactly one table, so caching by global page id
// with a caller-supplied decoder is consistent (pager.md §4).
func (s *sharedPaging) faultLeaf(page pageRef, colTypes []colType) (*pnode, error) {
	node, err := s.pool.getOrLoad(page.page, func() (*pnode, error) {
		block, err := s.readBody(page)
		if err != nil {
			return nil, err
		}
		// Lazy decode (spec/design/large-values.md §14): an external/compressed value stays an
		// unfetched reference — no chain read, no decompression. The scan layer resolves the
		// columns a query touches through readBlock below; a deferred value the touched set
		// missed self-resolves through the paging handle stamped here (bplus-reshape.md §5, B4).
		return decodeLeafNode(block, page, colTypes, s)
	})
	if err != nil {
		return nil, err
	}
	// The pool is keyed by page alone, so a pointer into another tree's leaf can return a node
	// decoded for that tree's columns. Its directories were validated against those columns, not
	// these, so serving it would index regions that do not exist (storage.md §6).
	if !sameLeafShape(node.packed.colTypes, colTypes) {
		return nil, newError(DataCorrupted, "leaf page belongs to a different tree")
	}
	return node, nil
}

// prefetchLeaves reads ahead (spec/design/page-service.md §5.3): it brings the leaves at pages that
// are not resident into the pool with one batched store read, decoded as a fault would decode them.
// At most a quarter of the pool is read ahead at once, so read-ahead cannot evict the leaf the scan is
// on. A failure is ignored: the fault that follows reads the page again and reports it.
func (s *sharedPaging) prefetchLeaves(pages []pageRef, colTypes []colType) {
	if !s.batches {
		return
	}
	limit := max(s.pool.capacityPages()/4, 1)
	var missing []pageRef
	for _, page := range pages {
		if len(missing) == limit {
			break
		}
		if !s.pool.contains(page.page) {
			missing = append(missing, page)
		}
	}
	if len(missing) < 2 {
		return
	}
	indices := make([]uint32, len(missing))
	for i, page := range missing {
		indices[i] = page.page
	}
	s.pagerMu.Lock()
	blocks, err := s.pgr.readBlocks(indices)
	s.pagerMu.Unlock()
	if err != nil {
		return
	}
	for i, page := range missing {
		block := blocks[i]
		_, _ = s.pool.getOrLoad(page.page, func() (*pnode, error) {
			plain, err := s.openBlock(page, block)
			if err != nil {
				return nil, err
			}
			return decodeLeafNode(plain, page, colTypes, s)
		})
	}
}

// sameLeafShape reports whether a leaf decoded for columns a can be read as columns b: the same
// column count and, per column, the same value codec. A same-width leaf of another type (bytea served
// as text) would stay in bounds here but reach the encoders and comparators as the wrong kind.
func sameLeafShape(a, b []colType) bool {
	if len(a) != len(b) {
		return false
	}
	if len(a) == 0 || &a[0] == &b[0] {
		return true
	}
	for i := range a {
		if !sameValueCodec(a[i], b[i]) {
			return false
		}
	}
	return true
}

// sameValueCodec compares two column types by their stored value codec: the scalar, array element,
// range element, or composite fields, recursively. Names, typmods, and nullability do not change the
// stored bytes, so they are ignored.
func sameValueCodec(a, b colType) bool {
	if a.Composite != b.Composite || (a.Elem == nil) != (b.Elem == nil) || (a.RangeElem == nil) != (b.RangeElem == nil) {
		return false
	}
	switch {
	case a.Composite:
		if len(a.Fields) != len(b.Fields) {
			return false
		}
		for i := range a.Fields {
			if !sameValueCodec(a.Fields[i].Type, b.Fields[i].Type) {
				return false
			}
		}
		return true
	case a.Elem != nil:
		return sameValueCodec(*a.Elem, *b.Elem)
	case a.RangeElem != nil:
		return sameValueCodec(*a.RangeElem, *b.RangeElem)
	default:
		return a.Scalar == b.Scalar
	}
}

// readBlock reads one page through the shared pager under the pager lock — the overflow-chain read
// path the scan layer's read-on-touch resolution uses (large-values.md §14). The lock covers only
// the physical read; leaf checksum/PAX decode happens after it is released.
func (s *sharedPaging) readBlock(page uint32) ([]byte, error) {
	s.pagerMu.Lock()
	defer s.pagerMu.Unlock()
	return s.pgr.readBlock(page)
}

// withPager runs fn with the pager locked — the commit write path (file.go persist pwrites dirty
// pages + meta) takes the same lock the fault path does, so they cannot race.
func (s *sharedPaging) withPager(fn func(*pager) error) error {
	s.pagerMu.Lock()
	defer s.pagerMu.Unlock()
	return fn(s.pgr)
}

func (s *sharedPaging) commitRequiresReopen() bool {
	s.pagerMu.Lock()
	defer s.pagerMu.Unlock()
	return s.pgr.commitRequiresReopen()
}

// close closes the backing file (Engine.Close).
func (s *sharedPaging) close() error {
	s.pagerMu.Lock()
	defer s.pagerMu.Unlock()
	return s.pgr.close()
}

// capacity is the pool's leaf-page capacity — carried over to the replacement pool when compaction
// reopens the database (spec/design/api.md §2.6).
func (s *sharedPaging) capacity() int {
	return s.pool.capacityPages()
}

// setCapacity re-bounds the pool to capacity leaves, evicting until the resident set fits (a spilled
// temp domain, spec/design/temp-tables.md §6).
func (s *sharedPaging) setCapacity(capacity int) {
	s.pool.setCapacity(capacity)
}

// residentLeaves is the number of leaf pages currently resident in the pool — the bound the
// demand-paging tests assert stays below the budget even for a database far larger than it. P6.4c
// promotes it to the public memory-budget surface.
func (s *sharedPaging) residentLeaves() int {
	return s.pool.resident()
}

// revoke makes every later access through this paging fail with err and closes the store: a page-service
// owner that released ownership no longer touches the file (spec/pageservice/README.md §6.5). Pages
// already in the pool still serve the snapshots that cached them.
func (s *sharedPaging) revoke(err error) {
	s.pagerMu.Lock()
	defer s.pagerMu.Unlock()
	_ = s.pgr.store.close()
	s.pgr.store = revokedStore{err: err}
	s.pgr.poisoned = true
}

// revokedStore is the byte store of revoked storage: every operation fails.
type revokedStore struct{ err error }

func (r revokedStore) readAt(int64, int) ([]byte, error) { return nil, r.err }
func (r revokedStore) writeAt(int64, []byte) error       { return r.err }
func (r revokedStore) sync() error                       { return r.err }
func (r revokedStore) size() (int64, error)              { return 0, r.err }
func (r revokedStore) setSize(int64) error               { return r.err }
func (r revokedStore) close() error                      { return nil }
