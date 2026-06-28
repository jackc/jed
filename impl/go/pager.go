package jed

// Block-device pager — the host-independent storage policy (spec/design/pager.md) above the
// blockStore host seam (spec/design/hosts.md §3). It composes a blockStore kept open for the handle's
// life, expressing the rest of the core's page-level operations (readBlock/writeBlock/reserve/sync)
// over the host's byte device — converting a page index to a byte offset (index × pageSize), owning
// the 1 MiB preallocation chunk, and deciding which durability barrier each step needs. The host (the
// file backing, fileBlockStore in blockstore.go) is the only per-platform code below; everything here
// is identical across hosts (hosts.md §1).
//
// The whole-image load and the commit route through readBlock/writeBlock; the bounded buffer pool +
// lazy node loading that make the resident set bounded (P6.4b) read through this same readBlock. The
// fault-injection seam (spec/design/storage.md §7) lives here, not in the host — it tests the commit
// recipe, which is host-independent (hosts.md §3).

import "encoding/binary"

// preallocChunkBytes is the file-growth step — ~1 MiB worth of pages preallocated at once. Growing
// the file in chunks of real, durably-allocated zero blocks is what lets a steady-state commit write
// its body into already-allocated space, so the per-commit fdatasync (pager.sync) carries no ext4
// metadata-journaling for a file-size change — the durable-commit win (spec/design/pager.md §7,
// TODO.md). The chunk's one-time allocating fsync (pager.reserve) amortizes across the chunk's commits.
const preallocChunkBytes = 1024 * 1024

// preallocChunkPages is the preallocation chunk in pages for a file of pageSize bytes: max(1,
// 1 MiB / pageSize). Page sizes are powers of two ≤ 64 KiB, so this divides 1 MiB evenly — the
// physical file therefore grows in exact 1 MiB steps regardless of page size.
func preallocChunkPages(pageSize uint32) uint32 {
	if pageSize == 0 {
		pageSize = 1
	}
	if c := uint32(preallocChunkBytes) / pageSize; c > 0 {
		return c
	}
	return 1
}

// pager is a block device: fixed-size pages addressed by index, over a blockStore host kept open for
// the handle's life. One page at a time (storage.md §2); the demand-paging buffer pool (P6.4b) faults
// pages in through readBlock.
type pager struct {
	store    blockStore
	pageSize uint32
	// allocatedPages is the number of pages physically allocated on disk — the file length in pages,
	// which the chunked preallocation (reserve) runs ahead of the committed high-water. A commit whose
	// pages all fall below this never grows the file (storage.md §9). Distinct from the committed
	// logical pageCount the meta records: the slack pages in [pageCount, allocatedPages) are
	// unreferenced trailing zeros (no byte-contract impact — past the high-water).
	allocatedPages uint32
	// fault is the armed one-shot commit fault — the fault-injection seam (spec/design/storage.md §7),
	// nil unless a test armed one with armFault. Production never arms one, so the checks in
	// writeBlock/sync are a single nil branch. Not a §8 byte contract: like the buffer pool (pager.md
	// §3), the seam is per-core internal machinery; the cross-core contract is the recovery outcome.
	fault *commitFault
	// bodyWrites/syncs count body-page writes (index ≥ 2) and sync() calls since the fault was armed,
	// driving faultBodyWrite / faultSync respectively.
	bodyWrites uint32
	syncs      uint32
}

// faultPoint selects a point in the commit write sequence at which the fault-injection seam
// (spec/design/storage.md §7) simulates a crash. Pages 0/1 are always the meta slots and every
// body/catalog page is ≥ 2 (format.md), so faultMetaWrite is identified by the page index, never by
// counting body pages. Testing only.
type faultPoint int

const (
	faultBodyWrite faultPoint = iota // the nth write to a body page (index ≥ 2), before the body sync
	faultMetaWrite                   // the meta-slot write (index < 2): the publish, after the body is synced
	faultSync                        // the nth sync() since arming (1 = body barrier, 2 = meta barrier)
)

// commitFault is a one-shot crash/tear the pager simulates at a chosen commit point (storage.md §7).
// Testing only.
type commitFault struct {
	point faultPoint
	n     uint32 // for faultBodyWrite/faultSync: the 1-based ordinal; ignored for faultMetaWrite
	// tearBytes, for a write point, is the count of leading page bytes to write before failing (a torn
	// page); a negative value means write nothing (a clean crash before the page lands). Ignored for
	// faultSync.
	tearBytes int
}

// armFault arms a one-shot commit fault (the fault-injection seam, spec/design/storage.md §7) and
// resets the since-arm counters, so the next commit's body-write / meta-write / sync sequence triggers
// it. The fault auto-disarms when it fires. Testing only.
func (p *pager) armFault(f commitFault) {
	p.fault = &f
	p.bodyWrites = 0
	p.syncs = 0
}

// pagerFromStore adopts an already-open store as the byte backing, reading the page size from its
// meta header (offset 8, format.md). The host layer (file.go) opens the host — mapping a missing
// path to 58P01 — and hands it here wrapped in a blockStore. A store smaller than a meta header, or a
// zero page size, is XX001.
func pagerFromStore(store blockStore) (*pager, error) {
	size, err := store.size()
	if err != nil {
		return nil, err
	}
	if size < 12 {
		return nil, NewError(DataCorrupted, "database file smaller than a meta header")
	}
	header, err := store.readAt(0, 12)
	if err != nil {
		return nil, err
	}
	pageSize := binary.BigEndian.Uint32(header[8:12])
	if pageSize == 0 {
		return nil, NewError(DataCorrupted, "zero page size in meta header")
	}
	// The allocation high-water is the current file length in pages — already past the committed
	// pageCount if a prior session preallocated slack (reused for free on this session's growth).
	return &pager{
		store:          store,
		pageSize:       pageSize,
		allocatedPages: uint32(size / int64(pageSize)),
	}, nil
}

// readBlock reads one page (block index) — random access, the demand-paging read path (P6.4b).
// Converts the page index to a byte offset for the host's readAt.
func (p *pager) readBlock(index uint32) ([]byte, error) {
	return p.store.readAt(int64(index)*int64(p.pageSize), int(p.pageSize))
}

// writeBlock writes one page (bytes) at block index. Overwrites in place — persist always reserves
// the high-water first, so the target is already-allocated space (a reused free page, or a
// preallocated slot past the old high-water). bytes is one page wide.
func (p *pager) writeBlock(index uint32, bytes []byte) error {
	if err := p.faultOnWrite(index, bytes); err != nil {
		return err
	}
	return p.store.writeAt(int64(index)*int64(p.pageSize), bytes)
}

// faultOnWrite is the fault-injection seam's write hook (spec/design/storage.md §7): nil-fault → no-op
// (zero production cost). If an armed fault targets this write it optionally performs a torn partial
// write, then disarms and returns an injected-crash error so persist aborts mid-commit.
func (p *pager) faultOnWrite(index uint32, bytes []byte) error {
	if p.fault == nil {
		return nil
	}
	f := p.fault
	hit := false
	switch f.point {
	case faultMetaWrite:
		hit = index < 2
	case faultBodyWrite:
		if index >= 2 {
			p.bodyWrites++
			hit = p.bodyWrites == f.n
		}
	}
	if !hit {
		return nil
	}
	p.fault = nil // one-shot
	if f.tearBytes >= 0 {
		k := f.tearBytes
		if k > len(bytes) {
			k = len(bytes)
		}
		if err := p.store.writeAt(int64(index)*int64(p.pageSize), bytes[:k]); err != nil {
			return err
		}
	}
	return injectedCrash()
}

// reserve ensures the file has at least minPages physically-allocated pages, growing it in fixed
// chunks (preallocChunkPages) of real, durably-allocated zero blocks when short. persist calls it
// before each commit's body write with the new committed high-water, so that write — and almost
// every commit's — lands entirely in already-allocated space and its fdatasync (pager.sync) pays no
// metadata journaling (spec/design/pager.md §7). The growth itself is a full fsync (f.Sync): the
// block allocation + the new file size must be durable before commits rely on writing into the
// region (else the body fdatasync would have to flush that metadata, defeating the point).
// Crash-safe: the preallocated pages are unreferenced zeros past the committed pageCount, so a crash
// before the next commit publishes simply ignores them. The preallocation policy (the 1 MiB chunk,
// the chunk-aligned target) is host-independent and stays here; the durable grow itself — real zero
// blocks + a full fsync — is the host's setSize, the metadata barrier (hosts.md §2.1/§3).
func (p *pager) reserve(minPages uint32) error {
	if minPages <= p.allocatedPages {
		return nil
	}
	chunk := preallocChunkPages(p.pageSize)
	target := ((minPages + chunk - 1) / chunk) * chunk
	if err := p.store.setSize(int64(target) * int64(p.pageSize)); err != nil {
		return err
	}
	p.allocatedPages = target
	return nil
}

// sync is the metadata-free durability barrier — the host's data-only sync (fdatasync). Called twice
// per commit — body pages, then the meta — to honour the body-before-meta write-ordering rule
// (format.md, file.go persist). Data-only (not a full fsync) so an overwrite into the preallocated
// region (reserve) flushes only the data, never a file-size/inode-timestamp metadata journal
// (spec/design/pager.md §7).
func (p *pager) sync() error {
	if p.fault != nil && p.fault.point == faultSync {
		p.syncs++
		if p.syncs == p.fault.n {
			p.fault = nil // one-shot
			return injectedCrash()
		}
	}
	return p.store.sync()
}

// injectedCrash is the error an armed fault returns to abort persist mid-commit — a simulated crash,
// reported as an ordinary I/O failure so the commit path rolls back exactly as a real write error
// would (spec/design/storage.md §7). Testing only.
func injectedCrash() error {
	return NewError(IoError, "injected commit crash (fault injection)")
}

// close releases the backing store (Engine.Close).
func (p *pager) close() error {
	return p.store.close()
}
