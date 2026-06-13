package jed

// Block-device pager — the storage seam (spec/design/storage.md §2) for a file-backed database
// (spec/design/pager.md). It owns the open file for the handle's life so pages can be read on demand
// and the incremental commit (P6.1) can write them without re-opening the file each time. Pure os —
// no cgo, no FFI (CLAUDE.md §2), memory-safe.
//
// P6.4a (this slice) routes the whole-image load and the commit through readBlock/writeBlock with no
// residency change — the loader still assembles the full image (readAll) and builds the whole tree.
// The bounded buffer pool + lazy node loading that make the resident set bounded (P6.4b) read through
// this same readBlock.

import (
	"encoding/binary"
	"errors"
	"io"
	"os"
)

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

// pager is a file-backed block device: fixed-size pages addressed by index, over an open file kept
// for the handle's life. One page at a time (storage.md §2); the demand-paging buffer pool (P6.4b)
// faults pages in through readBlock.
type pager struct {
	f        *os.File
	pageSize uint32
	// allocatedPages is the number of pages physically allocated on disk — the file length in pages,
	// which the chunked preallocation (reserve) runs ahead of the committed high-water. A commit whose
	// pages all fall below this never grows the file (storage.md §9). Distinct from the committed
	// logical pageCount the meta records: the slack pages in [pageCount, allocatedPages) are
	// unreferenced trailing zeros (no byte-contract impact — past the high-water).
	allocatedPages uint32
}

// pagerFromFile adopts an already-open (read+write) file as the backing, reading the page size from
// its meta header (offset 8, format.md). The host layer (file.go) opens the file — mapping a missing
// path to 58P01 — and hands it here. A header too short or a zero page size is XX001.
func pagerFromFile(f *os.File) (*pager, error) {
	var header [12]byte
	if _, err := f.ReadAt(header[:], 0); err != nil {
		if errors.Is(err, io.EOF) || errors.Is(err, io.ErrUnexpectedEOF) {
			return nil, NewError(DataCorrupted, "database file smaller than a meta header")
		}
		return nil, ioError(err)
	}
	pageSize := binary.BigEndian.Uint32(header[8:12])
	if pageSize == 0 {
		return nil, NewError(DataCorrupted, "zero page size in meta header")
	}
	// The allocation high-water is the current file length in pages — already past the committed
	// pageCount if a prior session preallocated slack (reused for free on this session's growth).
	info, err := f.Stat()
	if err != nil {
		return nil, ioError(err)
	}
	return &pager{
		f:              f,
		pageSize:       pageSize,
		allocatedPages: uint32(info.Size() / int64(pageSize)),
	}, nil
}

// readBlock reads one page (block index) — random access, the demand-paging read path (P6.4b).
func (p *pager) readBlock(index uint32) ([]byte, error) {
	buf := make([]byte, p.pageSize)
	if _, err := p.f.ReadAt(buf, int64(index)*int64(p.pageSize)); err != nil {
		return nil, ioError(err)
	}
	return buf, nil
}

// writeBlock writes one page (bytes) at block index. Overwrites in place — persist always reserves
// the high-water first, so the target is already-allocated space (a reused free page, or a
// preallocated slot past the old high-water). bytes is one page wide.
func (p *pager) writeBlock(index uint32, bytes []byte) error {
	if _, err := p.f.WriteAt(bytes, int64(index)*int64(p.pageSize)); err != nil {
		return ioError(err)
	}
	return nil
}

// reserve ensures the file has at least minPages physically-allocated pages, growing it in fixed
// chunks (preallocChunkPages) of real, durably-allocated zero blocks when short. persist calls it
// before each commit's body write with the new committed high-water, so that write — and almost
// every commit's — lands entirely in already-allocated space and its fdatasync (pager.sync) pays no
// metadata journaling (spec/design/pager.md §7). The growth itself is a full fsync (f.Sync): the
// block allocation + the new file size must be durable before commits rely on writing into the
// region (else the body fdatasync would have to flush that metadata, defeating the point).
// Crash-safe: the preallocated pages are unreferenced zeros past the committed pageCount, so a crash
// before the next commit publishes simply ignores them.
func (p *pager) reserve(minPages uint32) error {
	if minPages <= p.allocatedPages {
		return nil
	}
	chunk := preallocChunkPages(p.pageSize)
	target := ((minPages + chunk - 1) / chunk) * chunk
	zeros := make([]byte, int(target-p.allocatedPages)*int(p.pageSize))
	if _, err := p.f.WriteAt(zeros, int64(p.allocatedPages)*int64(p.pageSize)); err != nil {
		return ioError(err)
	}
	if err := p.f.Sync(); err != nil { // the allocation must be durable before in-region commits
		return ioError(err)
	}
	p.allocatedPages = target
	return nil
}

// sync is the metadata-free durability barrier (fdatasync). Called twice per commit — body pages,
// then the meta — to honour the body-before-meta write-ordering rule (format.md, file.go persist).
// fdatasync (not fsync) so an overwrite into the preallocated region (reserve) flushes only the data,
// never a file-size/inode-timestamp metadata journal (spec/design/pager.md §7). The fdatasync syscall
// is platform-specific, so it lives in pager_datasync_*.go (Linux uses syscall.Fdatasync, pure Go,
// no cgo — CLAUDE.md §2; other platforms fall back to a full Sync).
func (p *pager) sync() error {
	if err := datasync(p.f); err != nil {
		return ioError(err)
	}
	return nil
}

// close closes the open file (Database.Close).
func (p *pager) close() error {
	return p.f.Close()
}
