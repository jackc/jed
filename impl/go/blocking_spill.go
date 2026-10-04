package jed

// Bounded, repeatable row streams and disk hash partitions used by the blocking
// executor. Scratch I/O is unmetered; expression evaluation remains in its original
// logical phase and input order (spec/design/spill.md).
import (
	"bufio"
	"bytes"
	"encoding/binary"
	"io"
	"os"
)

type rowSpool struct {
	budget int
	dir    string
	rows   []storedRow
	bytes  int
	count  int64
	file   *os.File
	writer *bufio.Writer
}

func newRowSpool(db *engine) *rowSpool {
	return &rowSpool{budget: db.session.workMem, dir: db.spillDir}
}

func (s *rowSpool) push(row storedRow) error {
	if s.file == nil && s.bytes+rowBytes(row) > s.budget {
		f, err := os.CreateTemp(s.dir, "jed-spill-rows-*.tmp")
		if err != nil {
			return ioError(err)
		}
		s.file = f
		s.writer = bufio.NewWriter(f)
		for _, old := range s.rows {
			spillWriteRow(s.writer, old)
		}
		s.rows = nil
		s.bytes = 0
	}
	if s.file != nil {
		spillWriteRow(s.writer, row)
		if s.writer.Buffered() == 0 {
			if err := s.writer.Flush(); err != nil {
				return ioError(err)
			}
		}
	} else {
		s.rows = append(s.rows, row)
		s.bytes += rowBytes(row)
	}
	s.count++
	return nil
}

func (s *rowSpool) flush() error {
	if s.writer != nil {
		if err := s.writer.Flush(); err != nil {
			return ioError(err)
		}
	}
	return nil
}

func (s *rowSpool) each(fn func(storedRow) error) error {
	if err := s.flush(); err != nil {
		return err
	}
	if s.file == nil {
		for _, r := range s.rows {
			if err := fn(r); err != nil {
				return err
			}
		}
		return nil
	}
	r := bufio.NewReader(io.NewSectionReader(s.file, 0, 1<<63-1))
	for i := int64(0); i < s.count; i++ {
		row, err := spillReadRow(r)
		if err != nil {
			return ioError(err)
		}
		if err = fn(row); err != nil {
			return err
		}
	}
	return nil
}

func (s *rowSpool) close() {
	if s.file != nil {
		_ = s.file.Close()
		_ = os.Remove(s.file.Name())
		s.file = nil
	}
	s.rows = nil
}

func (s *rowSpool) output() (*sortedRows, error) {
	if err := s.flush(); err != nil {
		return nil, err
	}
	if s.file == nil {
		rows := s.rows
		s.rows = nil
		return &sortedRows{mem: rows}, nil
	}
	r := &spoolOutput{spool: s, reader: bufio.NewReader(io.NewSectionReader(s.file, 0, 1<<63-1))}
	return &sortedRows{stream: r}, nil
}

type spoolOutput struct {
	spool    *rowSpool
	reader   *bufio.Reader
	position int64
}

func (r *spoolOutput) next() (storedRow, bool, error) {
	if r.position >= r.spool.count {
		return nil, false, nil
	}
	r.position++
	row, err := spillReadRow(r.reader)
	if err != nil {
		return nil, false, ioError(err)
	}
	return row, true, nil
}
func (r *spoolOutput) close() { r.spool.close() }

// Fixed partition metadata and a single descriptor. A skewed partition is walked
// one record at a time; it never becomes a resident bucket/match vector. The two
// links permit original-order join probes and newest-state-first aggregate lookup.
const blockingPartitions = 4096

type diskBuckets struct {
	reader *bufio.Reader
	writer *bufio.Writer
	buffer bytes.Buffer
	file   *os.File
	heads  [blockingPartitions]uint64
	tails  [blockingPartitions]uint64
	end    uint64
}
type bucketRecord struct {
	prev, next, hash uint64
	key              []byte
	row              storedRow
}

func newDiskBuckets(dir string) (*diskBuckets, error) {
	f, err := os.CreateTemp(dir, "jed-spill-hash-*.tmp")
	if err != nil {
		return nil, ioError(err)
	}
	d := &diskBuckets{file: f}
	d.reader = bufio.NewReader(f)
	d.writer = bufio.NewWriter(&d.buffer)
	return d, nil
}

func (d *diskBuckets) close() {
	if d.file != nil {
		_ = d.file.Close()
		_ = os.Remove(d.file.Name())
		d.file = nil
		d.buffer = bytes.Buffer{}
		d.writer = nil
		d.reader = nil
	}
}

func (d *diskBuckets) append(hash uint64, key []byte, row storedRow) error {
	p := hash % blockingPartitions
	d.buffer.Reset()
	body := &d.buffer
	w := d.writer
	w.Reset(body)
	spillWriteU64(w, d.tails[p])
	spillWriteU64(w, 0)
	spillWriteU64(w, hash)
	spillWriteBytes(w, key)
	spillWriteRow(w, row)
	if err := w.Flush(); err != nil {
		return ioError(err)
	}
	pos := d.end + 1
	if _, err := d.file.WriteAt(body.Bytes(), int64(d.end)); err != nil {
		return ioError(err)
	}
	d.end += uint64(body.Len())
	if d.tails[p] != 0 {
		var b [8]byte
		binary.LittleEndian.PutUint64(b[:], pos)
		if _, err := d.file.WriteAt(b[:], int64(d.tails[p]-1+8)); err != nil {
			return ioError(err)
		}
	} else {
		d.heads[p] = pos
	}
	d.tails[p] = pos
	return nil
}

func (d *diskBuckets) read(pos uint64) (bucketRecord, error) {
	r := d.reader
	r.Reset(io.NewSectionReader(d.file, int64(pos-1), int64(d.end-pos+1)))
	var v bucketRecord
	var err error
	if v.prev, err = spillReadU64(r); err != nil {
		return v, ioError(err)
	}
	if v.next, err = spillReadU64(r); err != nil {
		return v, ioError(err)
	}
	if v.hash, err = spillReadU64(r); err != nil {
		return v, ioError(err)
	}
	if v.key, err = spillReadBytes(r); err != nil {
		return v, ioError(err)
	}
	if v.row, err = spillReadRow(r); err != nil {
		return v, ioError(err)
	}
	return v, nil
}

func (d *diskBuckets) lookup(hash uint64, key []byte) (storedRow, bool, error) {
	for pos := d.tails[hash%blockingPartitions]; pos != 0; {
		header, err := d.header(pos)
		if err != nil {
			return nil, false, err
		}
		if header.hash != hash {
			pos = header.prev
			continue
		}
		v, err := d.read(pos)
		if err != nil {
			return nil, false, err
		}
		if v.hash == hash && bytes.Equal(v.key, key) {
			return v.row, true, nil
		}
		pos = v.prev
	}
	return nil, false, nil
}

func (d *diskBuckets) bucket(hash uint64, fn func([]byte, storedRow) error) error {
	for pos := d.heads[hash%blockingPartitions]; pos != 0; {
		header, err := d.header(pos)
		if err != nil {
			return err
		}
		if header.hash != hash {
			pos = header.next
			continue
		}
		v, err := d.read(pos)
		if err != nil {
			return err
		}
		if v.hash == hash {
			if err := fn(v.key, v.row); err != nil {
				return err
			}
		}
		pos = v.next
	}
	return nil
}

type boundedMap struct {
	dir           string
	budget, bytes int
	entries       map[string]storedRow
	disk          *diskBuckets
}

func newBoundedMap(db *engine) *boundedMap {
	return &boundedMap{dir: db.spillDir, budget: db.session.workMem, entries: make(map[string]storedRow)}
}

func (m *boundedMap) close() {
	if m.disk != nil {
		m.disk.close()
	}
	m.entries = nil
}

func (m *boundedMap) get(key string) (storedRow, bool, error) {
	if m.disk != nil {
		return m.disk.lookup(hashJoinFNV1a([]byte(key)), []byte(key))
	}
	r, ok := m.entries[key]
	return r, ok, nil
}

func (m *boundedMap) put(key string, row storedRow) error {
	if m.disk == nil {
		if old, ok := m.entries[key]; ok {
			m.bytes -= len(key) + rowBytes(old) + 32
		}
		m.bytes += len(key) + rowBytes(row) + 32
		m.entries[key] = row
		if m.budget <= 0 || m.dir == "" || m.bytes <= m.budget {
			return nil
		}
		d, err := newDiskBuckets(m.dir)
		if err != nil {
			return err
		}
		m.disk = d
		// Map iteration affects private disk placement only; lookup compares full keys
		// and output order is carried by a separate first-occurrence spool.
		for k, v := range m.entries {
			if err = d.append(hashJoinFNV1a([]byte(k)), []byte(k), v); err != nil {
				return err
			}
		}
		m.entries = nil
		m.bytes = 0
		return nil
	}
	return m.disk.append(hashJoinFNV1a([]byte(key)), []byte(key), row)
}

func (d *diskBuckets) header(pos uint64) (bucketRecord, error) {
	var b [24]byte
	if _, err := d.file.ReadAt(b[:], int64(pos-1)); err != nil {
		return bucketRecord{}, ioError(err)
	}
	return bucketRecord{prev: binary.LittleEndian.Uint64(b[:8]), next: binary.LittleEndian.Uint64(b[8:16]), hash: binary.LittleEndian.Uint64(b[16:24])}, nil
}
