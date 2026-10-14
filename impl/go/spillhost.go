package jed

// The spill host: the scratch files external sorts, blocking-operator spools and hash partitions,
// and spilled session-local temp domains write (spec/design/spill.md §6, temp-tables.md §6). It is
// the Go form of the TypeScript core's SpillSink seam. Production uses private files in the OS
// temp directory. The deterministic simulator (spec/design/simulation-testing.md §14 S3) substitutes
// an in-memory host whose reads and writes can fail, so a fault can target spill I/O itself.
//
// Scratch bytes are never durable state: no barrier, no reopen. A nil host means spilling is
// unavailable (an in-memory database), and every operator keeps its rows resident.

import (
	"errors"
	"io"
	"os"
)

type spillHost interface {
	// create makes an empty private scratch file. kind names its use ("rows", "hash", "sort",
	// "merge", "temp") for the host's file name; it never changes behavior.
	create(kind string) (spillFile, error)
}

// spillFile is one scratch file: positioned reads and writes, its size, and close, which also
// deletes it. close is idempotent.
type spillFile interface {
	io.ReaderAt
	io.WriterAt
	size() (int64, error)
	truncate(n int64) error
	close() error
}

// osSpillHost is the production spill host: private files under dir, removed by close. A temp
// domain's scratch store, which lives as long as its session, is unlinked as soon as it is created
// where the OS allows an open file to be unlinked (so a crash leaves nothing behind).
type osSpillHost struct{ dir string }

func (h osSpillHost) create(kind string) (spillFile, error) {
	f, err := os.CreateTemp(h.dir, "jed-spill-"+kind+"-*.tmp")
	if err != nil {
		return nil, ioError(err)
	}
	path := f.Name()
	if kind == "temp" && os.Remove(path) == nil {
		path = ""
	}
	return &osSpillFile{f: f, path: path}, nil
}

type osSpillFile struct {
	f *os.File
	// path is the path still to remove on close; "" once unlinked.
	path string
}

func (s *osSpillFile) ReadAt(p []byte, off int64) (int, error)  { return s.f.ReadAt(p, off) }
func (s *osSpillFile) WriteAt(p []byte, off int64) (int, error) { return s.f.WriteAt(p, off) }

func (s *osSpillFile) size() (int64, error) {
	info, err := s.f.Stat()
	if err != nil {
		return 0, err
	}
	return info.Size(), nil
}

func (s *osSpillFile) truncate(n int64) error { return s.f.Truncate(n) }

func (s *osSpillFile) close() error {
	err := s.f.Close()
	if s.path != "" {
		_ = os.Remove(s.path)
		s.path = ""
	}
	if errors.Is(err, os.ErrClosed) {
		return nil
	}
	return err
}

// spillAppender is a sequential writer over a scratch file, for the bufio-framed spill codecs.
type spillAppender struct {
	f   spillFile
	off int64
}

func (a *spillAppender) Write(p []byte) (int, error) {
	n, err := a.f.WriteAt(p, a.off)
	a.off += int64(n)
	return n, err
}

// spillReader is a sequential reader over a scratch file from its start.
func spillReader(f spillFile) io.Reader { return io.NewSectionReader(f, 0, 1<<63-1) }
