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

// rowSpool is a repeatable row stream: resident until its rows exceed work_mem, then backed by one
// scratch file. While resident each row reserves its row_bytes against the statement's query-memory
// account (memory.md §6.6); a spill releases the whole resident charge, and release returns what is
// left when the spool is discarded (Go has no destructor — every owner calls it at its spec'd point).
type rowSpool struct {
	budget int
	dir    string
	rows   []storedRow
	bytes  int
	count  int64
	file   *os.File
	writer *bufio.Writer
	charge *stateCharge
}

func newRowSpool(db *engine) *rowSpool {
	return &rowSpool{budget: db.session.workMem, dir: db.spillDir, charge: newStateCharge(db.session.queryAccount())}
}

// push appends row. While resident it reserves its row_bytes. The spool spills when its residency
// exceeds work_mem or when the account rejects the reservation — the rejected row is never charged
// and goes to scratch with the resident rows (memory.md §6.6).
func (s *rowSpool) push(row storedRow) error {
	if s.file == nil {
		bytes := rowBytes(row)
		rejected := !s.charge.tryReserveDirect(int64(bytes))
		s.bytes += bytes
		if rejected || s.bytes > s.budget {
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
			// The rows left memory: the spool's whole resident charge goes with them.
			s.charge.releaseAll()
		}
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

// each replays every row in push order without consuming the spool.
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

// release returns the resident charge — the spool is discarded at this stage boundary (memory.md
// §6.6). It keeps its rows; close removes them.
func (s *rowSpool) release() {
	if s != nil {
		s.charge.releaseAll()
	}
}

// close discards the spool: its scratch file, rows, and remaining charge. Idempotent.
func (s *rowSpool) close() {
	if s == nil {
		return
	}
	if s.file != nil {
		_ = s.file.Close()
		_ = os.Remove(s.file.Name())
		s.file = nil
	}
	s.rows = nil
	s.charge.releaseAll()
}

// output consumes the spool into a pull stream that takes over its resident charge, returned when
// the stream is released or closed (the emission completed — memory.md §6.6).
func (s *rowSpool) output() (*sortedRows, error) {
	if err := s.flush(); err != nil {
		return nil, err
	}
	charge := s.charge
	s.charge = nil
	if s.file == nil {
		rows := s.rows
		s.rows = nil
		return &sortedRows{mem: rows, charge: charge}, nil
	}
	r := &spoolOutput{spool: s, reader: bufio.NewReader(io.NewSectionReader(s.file, 0, 1<<63-1))}
	return &sortedRows{stream: r, charge: charge}, nil
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

// stateMap is a keyed state map (groups, dedup sets) over logical key tuples: resident until its
// entries exceed work_mem (never, without a scratch directory or budget), then a newest-first disk
// chain per partition. Residency is the logical schedule ENTRY + key_bytes(key) + key_bytes(value),
// and the same number is charged (memory.md §6.6): a new entry reserves it, a replaced value its
// growth (or releases its shrinkage); a spill releases the whole resident charge.
type stateMap struct {
	dir     string
	budget  int
	bytes   int64
	entries map[string]storedRow
	disk    *diskBuckets
	charge  *stateCharge
}

func newStateMap(db *engine) *stateMap {
	return newStateMapCharged(db, newStateCharge(db.session.queryAccount()))
}

func newStateMapCharged(db *engine, charge *stateCharge) *stateMap {
	return &stateMap{dir: db.spillDir, budget: db.session.workMem, entries: make(map[string]storedRow), charge: charge}
}

func (m *stateMap) canSpill() bool { return m.budget > 0 && m.dir != "" }

// release returns the resident charge — the map is discarded at this stage boundary.
func (m *stateMap) release() {
	if m != nil {
		m.charge.releaseAll()
	}
}

// close discards the map: its scratch file, entries, and remaining charge. Idempotent.
func (m *stateMap) close() {
	if m == nil {
		return
	}
	if m.disk != nil {
		m.disk.close()
		m.disk = nil
	}
	m.entries = nil
	m.charge.releaseAll()
}

func (m *stateMap) get(key []Value) (storedRow, bool, error) {
	k := distinctRowKey(key)
	if m.disk != nil {
		return m.disk.lookup(hashJoinFNV1a([]byte(k)), []byte(k))
	}
	r, ok := m.entries[k]
	return r, ok, nil
}

func (m *stateMap) put(key []Value, value storedRow) error {
	k := distinctRowKey(key)
	if m.disk != nil {
		return m.disk.append(hashJoinFNV1a([]byte(k)), []byte(k), value)
	}
	if !m.canSpill() && !m.charge.active() {
		m.entries[k] = value
		return nil
	}
	old, had := m.entries[k]
	m.entries[k] = value
	var delta int64
	if had {
		delta = memKeyBytes(value) - memKeyBytes(old)
	} else {
		delta = memoryEntry + memKeyBytes(key) + memKeyBytes(value)
	}
	// A growth the account rejects spills a spill-capable map, the new entry with it, uncharged
	// (memory.md §6.6); a map that cannot spill returns the 54P05 for the caller's costFirst.
	rejected := false
	if delta > 0 {
		if m.canSpill() {
			rejected = !m.charge.tryReserveDirect(delta)
		} else if err := m.charge.reserveDirect(delta); err != nil {
			return err
		}
	} else {
		m.charge.release(-delta)
	}
	m.bytes += delta
	if !m.canSpill() || (!rejected && m.bytes <= int64(m.budget)) {
		return nil
	}
	d, err := newDiskBuckets(m.dir)
	if err != nil {
		return err
	}
	m.disk = d
	// The entries left memory: the map's whole resident charge goes with them.
	m.charge.releaseAll()
	// Map iteration affects private disk placement only; lookup compares full keys and output
	// order is carried by a separate first-occurrence spool.
	for ek, v := range m.entries {
		if err = d.append(hashJoinFNV1a([]byte(ek)), []byte(ek), v); err != nil {
			return err
		}
	}
	m.entries = nil
	m.bytes = 0
	return nil
}

// insert adds key as a membership entry, reporting whether it was new. A rejected reservation spills
// a spill-capable map and returns 54P05 from one that cannot spill, for the caller's costFirst.
func (m *stateMap) insert(key []Value) (bool, error) {
	_, had, err := m.get(key)
	if err != nil || had {
		return false, err
	}
	return true, m.put(key, nil)
}

// hashRows is an append-only row table per full hash: resident until its rows exceed work_mem,
// then forward disk chains preserving insertion order. Each resident row reserves ENTRY +
// row_bytes(row) (memory.md §6.6); a spill releases the whole resident charge.
type hashRows struct {
	dir    string
	budget int
	bytes  int64
	mem    map[uint64][]storedRow
	disk   *diskBuckets
	charge *stateCharge
}

func newHashRows(db *engine) *hashRows {
	return &hashRows{dir: db.spillDir, budget: db.session.workMem, mem: make(map[uint64][]storedRow), charge: newStateCharge(db.session.queryAccount())}
}

// release returns the resident charge — the table is discarded at this stage boundary.
func (h *hashRows) release() {
	if h != nil {
		h.charge.releaseAll()
	}
}

// close discards the table: its scratch file, rows, and remaining charge. Idempotent.
func (h *hashRows) close() {
	if h == nil {
		return
	}
	if h.disk != nil {
		h.disk.close()
		h.disk = nil
	}
	h.mem = nil
	h.charge.releaseAll()
}

func (h *hashRows) push(hash uint64, row storedRow) error {
	if h.disk != nil {
		return h.disk.append(hash, nil, row)
	}
	// A row the account rejects spills the table, the row with it, uncharged (memory.md §6.6).
	bytes := memHashRowBytes(row)
	rejected := !h.charge.tryReserveDirect(bytes)
	h.bytes += bytes
	h.mem[hash] = append(h.mem[hash], row)
	if !rejected && h.bytes <= int64(h.budget) {
		return nil
	}
	d, err := newDiskBuckets(h.dir)
	if err != nil {
		return err
	}
	h.disk = d
	// The rows left memory: the table's whole resident charge goes with them.
	h.charge.releaseAll()
	for hh, rows := range h.mem {
		for _, r := range rows {
			if err = d.append(hh, nil, r); err != nil {
				return err
			}
		}
	}
	h.mem = nil
	h.bytes = 0
	return nil
}

// each visits the rows pushed under hash, in insertion order.
func (h *hashRows) each(hash uint64, fn func(storedRow) error) error {
	if h.disk != nil {
		return h.disk.bucket(hash, func(_ []byte, row storedRow) error { return fn(row) })
	}
	for _, r := range h.mem[hash] {
		if err := fn(r); err != nil {
			return err
		}
	}
	return nil
}

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

func (d *diskBuckets) header(pos uint64) (bucketRecord, error) {
	var b [24]byte
	if _, err := d.file.ReadAt(b[:], int64(pos-1)); err != nil {
		return bucketRecord{}, ioError(err)
	}
	return bucketRecord{prev: binary.LittleEndian.Uint64(b[:8]), next: binary.LittleEndian.Uint64(b[8:16]), hash: binary.LittleEndian.Uint64(b[16:24])}, nil
}
