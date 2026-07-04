package jed

// The storage-host seam — the byte device a pager composes (spec/design/hosts.md §2/§3). A blockStore
// is the per-platform byte backing for one database file: an opaque, growable byte file addressed by
// byte offset + length, with NO notion of pages, meta slots, or the B-tree (those live in the pager
// above this seam). Keeping the host surface this small is what lets every host — os.File, OPFS, an
// encrypting/replicating wrap, even a pure in-memory slice — be a thin adapter that cannot drift.
//
// This seam first shipped the file host (fileBlockStore); the B+tree reshape's B3 slice adds the pure
// memoryBlockStore host so later work can route in-memory and temp stores through the same pager. The
// file-specific bits (open, the data-only fdatasync, the durable-grow Sync) live in fileBlockStore,
// while the policy — page math, the 1 MiB preallocation chunk, which barrier each step needs, the
// fault-injection seam — stays in the host-independent pager (hosts.md §3).

import (
	"errors"
	"io"
	"os"
)

// blockStore is the byte backing for one database file (spec/design/hosts.md §1/§2). The pager
// converts a page index to a byte offset (off = index × pageSize) and drives this device; the host
// knows only offsets and lengths. The first five methods are the spec's §2 surface; close is a
// lifecycle method (a host owning an OS handle must be able to release it), not part of the data
// contract.
type blockStore interface {
	// readAt reads length bytes at byte off. A short read past size() is the host's error (58030).
	readAt(off int64, length int) ([]byte, error)
	// writeAt stages a write of p at byte off — staged, not durable until sync (or setSize's grow).
	// Positioned: it must not move a shared cursor (hosts.md §2.1).
	writeAt(off int64, p []byte) error
	// sync is the data-only durability barrier (fdatasync): every prior in-region writeAt becomes
	// durable WITHOUT a file-size/inode metadata journal (hosts.md §2.1, spec/design/pager.md §7). A
	// host lacking a data-only barrier may implement it as a full fsync (correct, just slower).
	sync() error
	// size is the current file length in bytes.
	size() (int64, error)
	// setSize durably grows (real zero blocks + a full fsync) or truncates to n — the metadata barrier
	// (hosts.md §2.1). After it returns, bytes in [old, n) read back as zero AND the allocation is
	// durable, so a later in-region writeAt + data-only sync need not flush a file-growth journal.
	setSize(n int64) error
	// close releases the backing (the OS file handle). Lifecycle, not part of the §2 data surface.
	close() error
}

// fileBlockStore is the file storage host (spec/design/hosts.md §4): an *os.File with positioned
// ReadAt/WriteAt (pread/pwrite — no shared cursor), a data-only fdatasync barrier (datasync, pure Go,
// no cgo — CLAUDE.md §2), and a durable-grow zero-write + full Sync.
type fileBlockStore struct {
	f *os.File
	// noSync makes the fdatasync barriers (sync, and the durable-grow full Sync) no-ops — the fsync=off
	// host setting (api.md §2.1). The commit writes the same bytes in the same order; only the flush to
	// the platter is skipped. DEV/TESTING only: durable across a process crash (the OS page cache still
	// flushes) but NOT across an OS crash / power loss. Default false (fsync on).
	noSync bool
}

func (s *fileBlockStore) readAt(off int64, length int) ([]byte, error) {
	buf := make([]byte, length)
	if _, err := s.f.ReadAt(buf, off); err != nil {
		return nil, ioError(err)
	}
	return buf, nil
}

func (s *fileBlockStore) writeAt(off int64, p []byte) error {
	if _, err := s.f.WriteAt(p, off); err != nil {
		return ioError(err)
	}
	return nil
}

func (s *fileBlockStore) sync() error {
	if s.noSync {
		return nil // fsync=off (api.md §2.1): skip the durability barrier — dev/testing only.
	}
	// datasync (fdatasync), not a full Sync: an overwrite into the preallocated region flushes only
	// data, never a file-size/inode-timestamp metadata journal (spec/design/pager.md §7). The
	// platform split lives in blockstore_datasync_*.go (Linux syscall.Fdatasync, pure Go, no cgo;
	// a full Sync fallback elsewhere).
	if err := datasync(s.f); err != nil {
		return ioError(err)
	}
	return nil
}

func (s *fileBlockStore) size() (int64, error) {
	info, err := s.f.Stat()
	if err != nil {
		return 0, ioError(err)
	}
	return info.Size(), nil
}

func (s *fileBlockStore) setSize(n int64) error {
	info, err := s.f.Stat()
	if err != nil {
		return ioError(err)
	}
	cur := info.Size()
	if n > cur {
		// Grow with real zero blocks, then a full Sync: the allocation + new size must be durable
		// before a later in-region commit relies on it (else the per-commit data-only sync would have
		// to flush that metadata, defeating the durable-commit win — spec/design/pager.md §7).
		zeros := make([]byte, n-cur)
		if _, err := s.f.WriteAt(zeros, cur); err != nil {
			return ioError(err)
		}
		if !s.noSync { // fsync=off skips the durable-grow barrier too (dev/testing — no OS-crash durability).
			if err := s.f.Sync(); err != nil {
				return ioError(err)
			}
		}
	} else if n < cur {
		if err := s.f.Truncate(n); err != nil { // truncate; no barrier needed
			return ioError(err)
		}
	}
	return nil
}

func (s *fileBlockStore) close() error {
	return s.f.Close()
}

// memoryBlockStore is the pure in-memory storage host (bplus-reshape.md B3): a growable byte slice
// with the same positioned-read/write and zero-fill growth semantics as a file host, but with no
// durability work to do. It is the block-device building block for both in-memory databases (B3) and,
// since the temp-blockstore slice, per-domain session-local TEMP-table stores (newTempStorage,
// spec/design/temp-tables.md §6) — each rides the same pager + packed-leaf read path, with
// within-session compaction reclaiming its copy-on-write orphans (a temp store is never reopened).
type memoryBlockStore struct {
	buf []byte
}

// newMemoryBlockStore copies image so the caller's buffer cannot observe later writes.
func newMemoryBlockStore(image []byte) *memoryBlockStore {
	buf := make([]byte, len(image))
	copy(buf, image)
	return &memoryBlockStore{buf: buf}
}

func (s *memoryBlockStore) readAt(off int64, length int) ([]byte, error) {
	if off < 0 || length < 0 || off+int64(length) > int64(len(s.buf)) {
		return nil, ioError(io.ErrUnexpectedEOF)
	}
	out := make([]byte, length)
	copy(out, s.buf[off:off+int64(length)])
	return out, nil
}

func (s *memoryBlockStore) writeAt(off int64, p []byte) error {
	if off < 0 {
		return ioError(errors.New("negative offset"))
	}
	end := off + int64(len(p))
	if end > int64(len(s.buf)) {
		s.buf = append(s.buf, make([]byte, end-int64(len(s.buf)))...)
	}
	copy(s.buf[off:end], p)
	return nil
}

func (s *memoryBlockStore) sync() error { return nil }

func (s *memoryBlockStore) size() (int64, error) { return int64(len(s.buf)), nil }

func (s *memoryBlockStore) setSize(n int64) error {
	if n < 0 {
		return ioError(errors.New("negative size"))
	}
	if n > int64(len(s.buf)) {
		s.buf = append(s.buf, make([]byte, n-int64(len(s.buf)))...)
	} else if n < int64(len(s.buf)) {
		s.buf = s.buf[:n]
	}
	return nil
}

func (s *memoryBlockStore) close() error { return nil }
