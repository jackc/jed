package jed

// The storage-host seam — the byte device a pager composes (spec/design/hosts.md §2/§3). A blockStore
// is the per-platform byte backing for one database file: an opaque, growable byte file addressed by
// byte offset + length, with NO notion of pages, meta slots, or the B-tree (those live in the pager
// above this seam). Keeping the host surface this small is what lets every host — os.File, OPFS, an
// encrypting/replicating wrap, even a pure in-memory slice — be a thin adapter that cannot drift.
//
// This slice extracts the seam and ships the one file host (fileBlockStore); the in-memory, OPFS,
// encrypting, and replicating hosts are the catalog's other rows (hosts.md §4) and are NOT built here.
// The extraction is a pure refactor: the file-specific bits (open, the data-only fdatasync, the
// durable-grow Sync) move out of pager.go into fileBlockStore, while the policy — page math, the 1 MiB
// preallocation chunk, which barrier each step needs, the fault-injection seam — stays in the
// host-independent pager (hosts.md §3). No behavior or byte change.

import "os"

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
// no cgo — CLAUDE.md §2), and a durable-grow zero-write + full Sync. The one host built by the
// BlockStore-extraction slice; OPFS / encrypting / replicating / in-memory are the catalog's other rows.
type fileBlockStore struct {
	f *os.File
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
		if err := s.f.Sync(); err != nil {
			return ioError(err)
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
