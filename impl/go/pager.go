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

// pager is a file-backed block device: fixed-size pages addressed by index, over an open file kept
// for the handle's life. One page at a time (storage.md §2); the demand-paging buffer pool (P6.4b)
// faults pages in through readBlock.
type pager struct {
	f        *os.File
	pageSize uint32
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
	return &pager{f: f, pageSize: pageSize}, nil
}

// blockCount is the number of whole pages the backing currently holds (fileLen / pageSize).
func (p *pager) blockCount() (uint32, error) {
	info, err := p.f.Stat()
	if err != nil {
		return 0, ioError(err)
	}
	return uint32(info.Size() / int64(p.pageSize)), nil
}

// readBlock reads one page (block index) — random access, the demand-paging read path (P6.4b).
func (p *pager) readBlock(index uint32) ([]byte, error) {
	buf := make([]byte, p.pageSize)
	if _, err := p.f.ReadAt(buf, int64(index)*int64(p.pageSize)); err != nil {
		return nil, ioError(err)
	}
	return buf, nil
}

// writeBlock writes one page (bytes) at block index. Extends the file when index is the high-water,
// overwrites in place when reusing a free page (P6.2). bytes is one page wide.
func (p *pager) writeBlock(index uint32, bytes []byte) error {
	if _, err := p.f.WriteAt(bytes, int64(index)*int64(p.pageSize)); err != nil {
		return ioError(err)
	}
	return nil
}

// sync is the durability barrier (fsync). Called twice per commit — body pages, then the meta — to
// honour the body-before-meta write-ordering rule (format.md, file.go persist).
func (p *pager) sync() error {
	if err := p.f.Sync(); err != nil {
		return ioError(err)
	}
	return nil
}

// close closes the open file (Database.Close).
func (p *pager) close() error {
	return p.f.Close()
}

// readAll assembles the whole image, page by page through readBlock — the P6.4a load path (routes the
// whole-image load through the seam without changing residency; P6.4b reads only the reachable pages,
// on demand, instead).
func (p *pager) readAll() ([]byte, error) {
	count, err := p.blockCount()
	if err != nil {
		return nil, err
	}
	image := make([]byte, 0, int(count)*int(p.pageSize))
	for i := uint32(0); i < count; i++ {
		b, err := p.readBlock(i)
		if err != nil {
			return nil, err
		}
		image = append(image, b...)
	}
	return image, nil
}
