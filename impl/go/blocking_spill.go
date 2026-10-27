package jed

// Bounded, repeatable row streams and disk hash partitions used by the blocking
// executor. Scratch I/O is unmetered; expression evaluation remains in its original
// logical phase and input order (spec/design/spill.md).
import (
	"bufio"
	"bytes"
	"encoding/binary"
	"io"
	"math/bits"
)

// rowSpool is a repeatable row stream: resident until its rows exceed work_mem, then backed by one
// scratch file. While resident each row reserves its row_bytes against the statement's query-memory
// account (memory.md §6.6); a spill releases the whole resident charge, and release returns what is
// left when the spool is discarded (Go has no destructor — every owner calls it at its spec'd point).
type rowSpool struct {
	budget int
	spill  spillHost
	rows   []storedRow
	bytes  int
	count  int64
	file   spillFile
	writer *bufio.Writer
	charge *stateCharge
}

func newRowSpool(db *engine) *rowSpool {
	return &rowSpool{budget: db.session.workMem, spill: db.spill, charge: newStateCharge(db.session.queryAccount())}
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
			f, err := s.spill.create("rows")
			if err != nil {
				return err
			}
			s.file = f
			s.writer = bufio.NewWriter(&spillAppender{f: f})
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
	r := bufio.NewReader(spillReader(s.file))
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
		_ = s.file.close()
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
// entries exceed work_mem (never, without a scratch directory or budget), then a disk linear-hashing
// table. Residency is the logical schedule ENTRY + key_bytes(key) + key_bytes(value),
// and the same number is charged (memory.md §6.6): a new entry reserves it, a replaced value its
// growth (or releases its shrinkage); a spill releases the whole resident charge.
type stateMap struct {
	spill   spillHost
	budget  int
	bytes   int64
	entries map[string]storedRow
	disk    *spillTable
	codec   spillCodec
	charge  *stateCharge
}

func newStateMap(db *engine) *stateMap {
	return newStateMapCharged(db, newStateCharge(db.session.queryAccount()))
}

func newStateMapCharged(db *engine, charge *stateCharge) *stateMap {
	return &stateMap{spill: db.spill, budget: db.session.workMem, entries: make(map[string]storedRow), charge: charge}
}

func (m *stateMap) canSpill() bool { return m.budget > 0 && m.spill != nil }

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
		b, ok, err := m.disk.get(k)
		if err != nil || !ok {
			return nil, false, err
		}
		r, err := m.codec.decode(b)
		return r, err == nil, err
	}
	r, ok := m.entries[k]
	return r, ok, nil
}

func (m *stateMap) put(key []Value, value storedRow) error {
	k := distinctRowKey(key)
	if m.disk != nil {
		return m.disk.set(k, m.codec.encode(value, 0))
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
	d, err := newSpillTable(m.spill)
	if err != nil {
		return err
	}
	m.disk = d
	// The entries left memory: the map's whole resident charge goes with them.
	m.charge.releaseAll()
	// Map iteration affects private disk placement only; lookup compares full keys and output
	// order is carried by a separate first-occurrence spool.
	for ek, v := range m.entries {
		if err = d.set(ek, m.codec.encode(v, 0)); err != nil {
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
// then forward disk row chains preserving insertion order, indexed by a disk table from each hash
// to its chain's first and last rows. Each resident row reserves ENTRY +
// row_bytes(row) (memory.md §6.6); a spill releases the whole resident charge.
type hashRows struct {
	spill  spillHost
	budget int
	bytes  int64
	mem    map[uint64][]storedRow
	disk   *spillTable
	codec  spillCodec
	charge *stateCharge
}

func newHashRows(db *engine) *hashRows {
	return &hashRows{spill: db.spill, budget: db.session.workMem, mem: make(map[uint64][]storedRow), charge: newStateCharge(db.session.queryAccount())}
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
		return h.pushDisk(hash, row)
	}
	// A row the account rejects spills the table, the row with it, uncharged (memory.md §6.6).
	bytes := memHashRowBytes(row)
	rejected := !h.charge.tryReserveDirect(bytes)
	h.bytes += bytes
	h.mem[hash] = append(h.mem[hash], row)
	if !rejected && h.bytes <= int64(h.budget) {
		return nil
	}
	d, err := newSpillTable(h.spill)
	if err != nil {
		return err
	}
	h.disk = d
	// The rows left memory: the table's whole resident charge goes with them.
	h.charge.releaseAll()
	for hh, rows := range h.mem {
		for _, r := range rows {
			if err = h.pushDisk(hh, r); err != nil {
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
		return h.eachDisk(hash, fn)
	}
	for _, r := range h.mem[hash] {
		if err := fn(r); err != nil {
			return err
		}
	}
	return nil
}

// A spilled row record is a next-row link (u64, 0 at the end), a u32 length, and the encoded row.
// Offset 0 is the table's first directory segment, so no record starts there.
const spillRowHeader = 12

func spillHashKey(hash uint64) string {
	var k [8]byte
	binary.LittleEndian.PutUint64(k[:], hash)
	return string(k[:])
}

func (h *hashRows) pushDisk(hash uint64, row storedRow) error {
	key := spillHashKey(hash)
	bounds, had, err := h.disk.get(key)
	if err != nil {
		return err
	}
	var first, last int64
	if had {
		first = int64(binary.LittleEndian.Uint64(bounds[0:]))
		last = int64(binary.LittleEndian.Uint64(bounds[8:]))
	}
	rec := h.codec.encode(row, spillRowHeader)
	binary.LittleEndian.PutUint32(rec[8:], uint32(len(rec)-spillRowHeader))
	at, err := h.disk.file.append(rec)
	if err != nil {
		return err
	}
	if had {
		if err := h.disk.writeU64(last, at); err != nil {
			return err
		}
	} else {
		first = at
	}
	var next [16]byte
	binary.LittleEndian.PutUint64(next[0:], uint64(first))
	binary.LittleEndian.PutUint64(next[8:], uint64(at))
	return h.disk.set(key, next[:])
}

func (h *hashRows) eachDisk(hash uint64, fn func(storedRow) error) error {
	bounds, ok, err := h.disk.get(spillHashKey(hash))
	if err != nil || !ok {
		return err
	}
	file := h.disk.file
	var buf []byte
	for at := int64(binary.LittleEndian.Uint64(bounds)); at != 0; {
		// One read takes the header and, usually, the whole row.
		want := min(int64(spillRowHeader+spillPeek*4), file.end()-at)
		if int64(cap(buf)) < want {
			buf = make([]byte, want)
		}
		b := buf[:want]
		if err := file.read(b, at); err != nil {
			return err
		}
		next := int64(binary.LittleEndian.Uint64(b))
		n := int64(binary.LittleEndian.Uint32(b[8:]))
		if spillRowHeader+n > int64(len(b)) {
			b = make([]byte, spillRowHeader+n)
			if err := file.read(b, at); err != nil {
				return err
			}
		}
		row, err := h.codec.decode(b[spillRowHeader : spillRowHeader+n])
		if err != nil {
			return err
		}
		if err := fn(row); err != nil {
			return err
		}
		at = next
	}
	return nil
}

// Appended scratch bytes gather in a buffer written once it fills, so a spilled structure holds one
// bounded I/O buffer however much it spills.
const spillTailBytes = 64 * 1024

// bufferedScratch is a spilled structure's scratch file with its appends buffered. Reads and
// rewrites see buffered bytes as if written.
type bufferedScratch struct {
	file spillFile
	// File bytes [tailStart, tailStart+len(tail)) are still in tail, unwritten.
	tail      []byte
	tailStart int64
}

func newBufferedScratch(spill spillHost) (*bufferedScratch, error) {
	f, err := spill.create("hash")
	if err != nil {
		return nil, err
	}
	return &bufferedScratch{file: f}, nil
}

func (s *bufferedScratch) end() int64 { return s.tailStart + int64(len(s.tail)) }

func (s *bufferedScratch) flush() error {
	if len(s.tail) == 0 {
		return nil
	}
	if _, err := s.file.WriteAt(s.tail, s.tailStart); err != nil {
		return ioError(err)
	}
	s.tailStart += int64(len(s.tail))
	s.tail = s.tail[:0]
	return nil
}

// read fills dst with bytes [at, at+len(dst)), all before end.
func (s *bufferedScratch) read(dst []byte, at int64) error {
	if at >= s.tailStart {
		copy(dst, s.tail[at-s.tailStart:])
		return nil
	}
	if at+int64(len(dst)) > s.tailStart {
		if err := s.flush(); err != nil {
			return err
		}
	}
	if n, err := s.file.ReadAt(dst, at); n < len(dst) {
		if err == nil {
			err = io.ErrUnexpectedEOF
		}
		return ioError(err)
	}
	return nil
}

// overwrite replaces bytes already appended.
func (s *bufferedScratch) overwrite(at int64, b []byte) error {
	if at >= s.tailStart {
		copy(s.tail[at-s.tailStart:], b)
		return nil
	}
	if at+int64(len(b)) > s.tailStart {
		if err := s.flush(); err != nil {
			return err
		}
	}
	if _, err := s.file.WriteAt(b, at); err != nil {
		return ioError(err)
	}
	return nil
}

// append adds b at the end and returns its offset.
func (s *bufferedScratch) append(b []byte) (int64, error) {
	at := s.end()
	if len(s.tail)+len(b) > spillTailBytes {
		if err := s.flush(); err != nil {
			return 0, err
		}
		if len(b) > spillTailBytes {
			if _, err := s.file.WriteAt(b, at); err != nil {
				return 0, ioError(err)
			}
			s.tailStart += int64(len(b))
			return at, nil
		}
	}
	s.tail = append(s.tail, b...)
	return at, nil
}

// reserve adds n zero bytes (n ≥ 8) at the end and returns their offset. Writing the last eight
// extends the file over them; the rest read as zeros.
func (s *bufferedScratch) reserve(n int64) (int64, error) {
	if err := s.flush(); err != nil {
		return 0, err
	}
	at := s.tailStart
	if _, err := s.file.WriteAt(make([]byte, 8), at+n-8); err != nil {
		return 0, ioError(err)
	}
	s.tailStart += n
	return at, nil
}

func (s *bufferedScratch) close() {
	if s.file != nil {
		_ = s.file.close()
		s.file = nil
		s.tail = nil
	}
}

// spillTable is a spilled keyed map's scratch: a linear-hashing table from key bytes to value bytes.
// Placement is private: lookups compare the complete key, and callers keep their own visitation
// order.
//
//	directory: bucket heads (u64 node offsets, 0 = empty) in segments. Segment 0 holds
//	  spillInitialBuckets heads at offset 0; segment k ≥ 1 holds spillInitialBuckets·2^(k-1) heads and
//	  is reserved at the end of the file when a split first needs it. Only the segment offsets are
//	  resident, at most spillMaxSegments of them, so the metadata stays fixed however many keys spill.
//	node: next u64 | value block offset u64 | value capacity u32 | key length u32 | hash u64 |
//	  key bytes | its first value block
//	value block: length u32 | value bytes, in a reserved capacity a replacement reuses when it fits.
//
// Each new key may split one bucket, so chains average at most one node and a lookup reads one head
// and about one node. A skewed bucket (one hash for every key) still streams one node at a time.
// Existing keys rewrite their value block in place, or move it when it outgrows its capacity, so
// repeated updates never grow a chain of stale versions.
const (
	spillInitialBuckets = 4096
	// 4096·2^20 = 2^32 buckets; splitting stops there.
	spillMaxSegments = 21
	spillNodeHeader  = 32
	// A node read also takes this many bytes past the key, so a small first value block arrives with it.
	spillPeek = 64
	// Keys up to this many bytes are read with their node's header.
	spillInlineKey = 4096
)

type spillTable struct {
	file *bufferedScratch
	// The table has base + split buckets; each bucket below split has been split into itself and
	// itself + base. keys counts the keys.
	base, split, keys uint64
	segments          []int64
	// The last lookup, so a get then set of one key (a GROUP BY state update, an insert) hashes and
	// walks its chain once.
	loc  spillLocated
	node []byte
}

// spillLocated is a key's place: its hash, bucket and that bucket's head, and its node (0 while
// absent) with the node's value block offset, capacity, and value once read.
type spillLocated struct {
	valid    bool
	key      string
	hash     uint64
	bucket   uint64
	head     int64
	node     int64
	valueAt  int64
	capacity uint32
	hasValue bool
	value    []byte
}

func newSpillTable(spill spillHost) (*spillTable, error) {
	f, err := newBufferedScratch(spill)
	if err != nil {
		return nil, err
	}
	t := &spillTable{file: f, base: spillInitialBuckets}
	at, err := f.reserve(spillInitialBuckets * 8)
	if err != nil {
		f.close()
		return nil, err
	}
	t.segments = append(make([]int64, 0, spillMaxSegments), at)
	return t, nil
}

func (t *spillTable) close() {
	t.file.close()
	t.loc = spillLocated{}
	t.node = nil
}

// spillHash is FNV-1a over key, avalanched so linear hashing's low-bit bucket addresses spread evenly.
func spillHash(key string) uint64 {
	h := uint64(14695981039346656037)
	for i := 0; i < len(key); i++ {
		h ^= uint64(key[i])
		h *= 1099511628211
	}
	h ^= h >> 30
	h *= 0xbf58476d1ce4e5b9
	h ^= h >> 27
	h *= 0x94d049bb133111eb
	h ^= h >> 31
	return h
}

func (t *spillTable) headAt(bucket uint64) int64 {
	if bucket < spillInitialBuckets {
		return t.segments[0] + int64(bucket)*8
	}
	segment := bits.Len64(bucket / spillInitialBuckets)
	return t.segments[segment] + int64(bucket-spillInitialBuckets<<(segment-1))*8
}

func (t *spillTable) readU64(at int64) (int64, error) {
	var b [8]byte
	if err := t.file.read(b[:], at); err != nil {
		return 0, err
	}
	return int64(binary.LittleEndian.Uint64(b[:])), nil
}

func (t *spillTable) writeU64(at, v int64) error {
	var b [8]byte
	binary.LittleEndian.PutUint64(b[:], uint64(v))
	return t.file.overwrite(at, b[:])
}

func (t *spillTable) address(hash uint64) uint64 {
	if bucket := hash % t.base; bucket >= t.split {
		return bucket
	}
	return hash % (t.base * 2)
}

func (t *spillTable) locate(key string) (*spillLocated, error) {
	loc := &t.loc
	if loc.valid && loc.key == key {
		return loc, nil
	}
	hash := spillHash(key)
	bucket := t.address(hash)
	*loc = spillLocated{key: key, hash: hash, bucket: bucket, value: loc.value[:0]}
	head, err := t.readU64(t.headAt(bucket))
	if err != nil {
		return nil, err
	}
	loc.head = head
	end := t.file.end()
	// A short key's node read takes the header, a key of this length, and often the first value
	// block; a long key is read only when the hash and length match.
	inline := len(key) <= spillInlineKey
	for node := head; node != 0; {
		want := int64(spillNodeHeader)
		if inline {
			want += int64(len(key) + spillPeek)
		}
		want = min(want, end-node)
		if int64(cap(t.node)) < want {
			t.node = make([]byte, want)
		}
		b := t.node[:want]
		if err := t.file.read(b, node); err != nil {
			return nil, err
		}
		if binary.LittleEndian.Uint64(b[24:]) == hash && binary.LittleEndian.Uint32(b[20:]) == uint32(len(key)) {
			var match bool
			if inline {
				match = string(b[spillNodeHeader:spillNodeHeader+len(key)]) == key
			} else {
				stored := make([]byte, len(key))
				if err := t.file.read(stored, node+spillNodeHeader); err != nil {
					return nil, err
				}
				match = string(stored) == key
			}
			if match {
				loc.node = node
				loc.valueAt = int64(binary.LittleEndian.Uint64(b[8:]))
				loc.capacity = binary.LittleEndian.Uint32(b[16:])
				if within := loc.valueAt - node; within+4 <= int64(len(b)) {
					if n := int64(binary.LittleEndian.Uint32(b[within:])); within+4+n <= int64(len(b)) {
						loc.value = append(loc.value, b[within+4:within+4+n]...)
						loc.hasValue = true
					}
				}
				break
			}
		}
		node = int64(binary.LittleEndian.Uint64(b[0:]))
	}
	loc.valid = true
	return loc, nil
}

// get returns key's value, valid until the table's next call.
func (t *spillTable) get(key string) ([]byte, bool, error) {
	loc, err := t.locate(key)
	if err != nil || loc.node == 0 {
		return nil, false, err
	}
	if !loc.hasValue {
		block := make([]byte, 4+int(loc.capacity))
		if err := t.file.read(block, loc.valueAt); err != nil {
			loc.valid = false
			return nil, false, err
		}
		loc.value = append(loc.value[:0], block[4:4+binary.LittleEndian.Uint32(block)]...)
		loc.hasValue = true
	}
	return loc.value, true, nil
}

func spillValueBlock(value []byte, capacity int) []byte {
	block := make([]byte, 4+capacity)
	binary.LittleEndian.PutUint32(block, uint32(len(value)))
	copy(block[4:], value)
	return block
}

// set stores value under key.
func (t *spillTable) set(key string, value []byte) error {
	loc, err := t.locate(key)
	if err != nil {
		return err
	}
	// A failed write leaves the remembered place unknown.
	loc.valid = false
	if loc.node != 0 {
		if len(value) <= int(loc.capacity) {
			if err := t.file.overwrite(loc.valueAt, spillValueBlock(value, len(value))); err != nil {
				return err
			}
		} else {
			// The value outgrew its block: move it to a new one with room to grow.
			capacity := len(value) + len(value)>>1
			at, err := t.file.append(spillValueBlock(value, capacity))
			if err != nil {
				return err
			}
			var pointer [12]byte
			binary.LittleEndian.PutUint64(pointer[:], uint64(at))
			binary.LittleEndian.PutUint32(pointer[8:], uint32(capacity))
			if err := t.file.overwrite(loc.node+8, pointer[:]); err != nil {
				return err
			}
			loc.valueAt, loc.capacity = at, uint32(capacity)
		}
		loc.value = append(loc.value[:0], value...)
		loc.hasValue, loc.valid = true, true
		return nil
	}
	n := spillNodeHeader + len(key) + 4 + len(value)
	if cap(t.node) < n {
		t.node = make([]byte, n)
	}
	node := t.node[:n]
	at := t.file.end()
	binary.LittleEndian.PutUint64(node[0:], uint64(loc.head))
	binary.LittleEndian.PutUint64(node[8:], uint64(at+spillNodeHeader+int64(len(key))))
	binary.LittleEndian.PutUint32(node[16:], uint32(len(value)))
	binary.LittleEndian.PutUint32(node[20:], uint32(len(key)))
	binary.LittleEndian.PutUint64(node[24:], loc.hash)
	copy(node[spillNodeHeader:], key)
	binary.LittleEndian.PutUint32(node[spillNodeHeader+len(key):], uint32(len(value)))
	copy(node[spillNodeHeader+len(key)+4:], value)
	if _, err := t.file.append(node); err != nil {
		return err
	}
	if err := t.writeU64(t.headAt(loc.bucket), at); err != nil {
		return err
	}
	loc.head, loc.node = at, at
	loc.valueAt = at + spillNodeHeader + int64(len(key))
	loc.capacity = uint32(len(value))
	loc.value = append(loc.value[:0], value...)
	loc.hasValue, loc.valid = true, true
	if t.keys++; t.keys > t.base+t.split {
		return t.splitBucket()
	}
	return nil
}

// splitBucket divides bucket split between itself and split + base by the next hash bit, keeping
// each node's relative order and rewriting only the links that change.
func (t *spillTable) splitBucket() error {
	from := t.split
	to := t.base + from
	if to == spillInitialBuckets<<(len(t.segments)-1) {
		if len(t.segments) == spillMaxSegments {
			return nil
		}
		at, err := t.file.reserve(int64(to) * 8)
		if err != nil {
			return err
		}
		t.segments = append(t.segments, at)
	}
	// Bucket addresses move.
	t.loc.valid = false
	original, err := t.readU64(t.headAt(from))
	if err != nil {
		return err
	}
	// Each list's first node, last node, and that last node's link as written.
	var lists [2]struct{ first, last, link int64 }
	var header [spillNodeHeader]byte
	for node := original; node != 0; {
		if err := t.file.read(header[:], node); err != nil {
			return err
		}
		next := int64(binary.LittleEndian.Uint64(header[0:]))
		list := &lists[0]
		if binary.LittleEndian.Uint64(header[24:])%(t.base*2) != from {
			list = &lists[1]
		}
		if list.last == 0 {
			list.first = node
		} else if list.link != node {
			if err := t.writeU64(list.last, node); err != nil {
				return err
			}
		}
		list.last, list.link = node, next
		node = next
	}
	for _, list := range lists {
		if list.last != 0 && list.link != 0 {
			if err := t.writeU64(list.last, 0); err != nil {
				return err
			}
		}
	}
	if lists[0].first != original {
		if err := t.writeU64(t.headAt(from), lists[0].first); err != nil {
			return err
		}
	}
	if lists[1].first != 0 {
		if err := t.writeU64(t.headAt(to), lists[1].first); err != nil {
			return err
		}
	}
	if t.split++; t.split == t.base {
		t.base *= 2
		t.split = 0
	}
	return nil
}

// spillCodec encodes and decodes spilled rows through reused buffers.
type spillCodec struct {
	buf    bytes.Buffer
	writer *bufio.Writer
	source bytes.Reader
	reader *bufio.Reader
}

// encode returns row's spill encoding after prefix zero bytes, valid until the next encode.
func (c *spillCodec) encode(row storedRow, prefix int) []byte {
	c.buf.Reset()
	if c.writer == nil {
		c.writer = bufio.NewWriter(&c.buf)
	}
	c.buf.Write(make([]byte, prefix))
	spillWriteRow(c.writer, row)
	_ = c.writer.Flush()
	return c.buf.Bytes()
}

func (c *spillCodec) decode(b []byte) (storedRow, error) {
	c.source.Reset(b)
	if c.reader == nil {
		c.reader = bufio.NewReader(&c.source)
	} else {
		c.reader.Reset(&c.source)
	}
	row, err := spillReadRow(c.reader)
	if err != nil {
		return nil, ioError(err)
	}
	return row, nil
}
