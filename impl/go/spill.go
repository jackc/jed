package jed

// External merge sort with spill-to-disk for ORDER BY (spec/design/spill.md). A sorter accumulates
// pushed rows up to a work-memory budget; when a file-backed database exceeds it, the sorter
// stable-sorts the in-memory run and spills it to a temporary file, then k-way-merges all runs at
// finish, reproducing the in-memory stable sort byte-for-byte (spill.md §4/§6).
//
// Not a §8 byte contract (spill.md §6): spill changes WHEN rows are resident, never WHAT a query
// observes (results + cost are invariant — the sort is unmetered, cost.md §3). So the run file's
// bytes are a per-core internal self-describing row codec, round-tripped only within one core during
// one query while the database file is unchanged — not the §8 on-disk record format. Stdlib I/O only
// (no dependency — CLAUDE.md §14; pure Go, no cgo — §13).

import (
	"bufio"
	"container/heap"
	"encoding/binary"
	"io"
	"os"
	"sort"
)

// DefaultWorkMem is the default work-memory budget, in bytes (256 MiB) — the OpenOptions.WorkMem
// default (spec/design/spill.md §2, api.md §2.1). Matches the buffer-pool default so a RAM-sized
// ORDER BY stays fully in memory under the default; a host bounds a hostile/large sort by lowering
// it. A handle setting, never stored in the file.
const defaultWorkMem = 256 * 1024 * 1024

// valueBytes is a cheap, deterministic estimate of a value's resident bytes (spill.md §2): a fixed
// base plus the variable-width payload. It need not be exact — it only decides spill timing, which
// is invisible to results and cost.
func valueBytes(v Value) int {
	const base = 24
	switch v.Kind {
	case ValText, ValBytea, ValUuid:
		return base + len(v.str())
	case ValDecimal:
		if v.decimal() != nil {
			_, _, g := v.decimal().ToCodec()
			return base + len(g)*2
		}
		return base
	case ValUnfetched:
		if v.unfetched() != nil {
			return base + len(v.unfetched().Comp)
		}
		return base
	default:
		return base
	}
}

func rowBytes(row storedRow) int {
	n := 8
	for _, v := range row {
		n += valueBytes(v)
	}
	return n
}

// cmpRows is the stable comparator over the ORDER BY keys: the first non-equal key decides; a full
// tie is 0 (the SliceStable keeps input order — spill.md §6).
func cmpRows(keys []orderSlot, a, b storedRow) int {
	for _, k := range keys {
		if c := keyCmp(a[k.idx], b[k.idx], k.descending, k.nullsFirst); c != 0 {
			return c
		}
	}
	return 0
}

// sorter is the external merge sorter (spec/design/spill.md §4). Push rows, then finish to read them
// back in ORDER BY order. Bounds resident memory to budget bytes by spilling sorted runs; an
// in-memory database (spillDir == "") or unlimited budget keeps everything resident and just
// stable-sorts at the end.
type sorter struct {
	keys     []orderSlot
	budget   int    // 0 ⇒ unlimited (never spill)
	spillDir string // "" ⇒ never spill (in-memory database)
	buf      []storedRow
	bufBytes int
	runs     []string // spilled run file paths, in input order (run 0 = first chunk — spill.md §6)
	total    int
}

func newSorter(keys []orderSlot, budget int, spillDir string) *sorter {
	return &sorter{keys: keys, budget: budget, spillDir: spillDir}
}

func (s *sorter) canSpill() bool { return s.spillDir != "" && s.budget > 0 }

// push adds one row, spilling the current run when the in-memory buffer exceeds the budget.
func (s *sorter) push(row storedRow) error {
	s.total++
	if s.canSpill() {
		s.bufBytes += rowBytes(row)
	}
	s.buf = append(s.buf, row)
	if s.canSpill() && s.bufBytes > s.budget {
		return s.spillRun()
	}
	return nil
}

func (s *sorter) sortBuf() {
	sort.SliceStable(s.buf, func(i, j int) bool { return cmpRows(s.keys, s.buf[i], s.buf[j]) < 0 })
}

// spillRun stable-sorts the in-memory buffer and writes it as one sorted run file, then clears it.
func (s *sorter) spillRun() error {
	s.sortBuf()
	f, err := os.CreateTemp(s.spillDir, "jed-spill-*.tmp")
	if err != nil {
		return ioError(err)
	}
	path := f.Name()
	keep := false
	defer func() {
		if !keep {
			_ = f.Close()
			_ = os.Remove(path)
		}
	}()
	w := bufio.NewWriter(f)
	spillWriteU64(w, uint64(len(s.buf)))
	for _, row := range s.buf {
		spillWriteRow(w, row)
	}
	if err := w.Flush(); err != nil {
		_ = f.Close()
		return ioError(err)
	}
	if err := f.Close(); err != nil {
		return ioError(err)
	}
	s.runs = append(s.runs, path)
	keep = true
	s.buf = s.buf[:0]
	s.bufBytes = 0
	return nil
}

// finish returns the rows in ORDER BY order. With no spilled run this is the unchanged in-memory
// stable sort (the dominant RAM-sized fast path); otherwise it stable-sorts the final partial buffer
// and k-way-merges it with the runs.
func (s *sorter) finish() (*sortedRows, error) {
	s.sortBuf()
	if len(s.runs) == 0 {
		return &sortedRows{mem: s.buf}, nil
	}
	// Sources: each spilled run, then the final in-memory buffer last (the latest input positions →
	// the highest source index, the tie-break that reproduces input order — spill.md §6).
	sources := make([]*mergeSource, 0, len(s.runs)+1)
	for _, path := range s.runs {
		src, err := openRunSource(path)
		if err != nil {
			for _, o := range sources {
				o.close()
			}
			return nil, err
		}
		sources = append(sources, src)
	}
	sources = append(sources, &mergeSource{mem: s.buf})
	h := &mergeHeap{keys: s.keys}
	for i, src := range sources {
		row, ok, err := src.next()
		if err != nil {
			for _, o := range sources {
				o.close()
			}
			return nil, err
		}
		if ok {
			h.items = append(h.items, &mergeItem{row: row, source: i})
		}
	}
	heap.Init(h)
	return &sortedRows{merge: &merger{sources: sources, heap: h}}, nil
}

// sortedRows is the sorted output stream (spec/design/spill.md §4). The window/projection loop pulls
// rows one at a time, so neither the input nor the output is re-materialized in the spill case.
type sortedRows struct {
	mem    []storedRow // set for the no-spill case
	memPos int
	merge  *merger // set for the spill case
}

// next returns the next row in sort order, or ok=false at the end.
func (r *sortedRows) next() (storedRow, bool, error) {
	if r.merge != nil {
		return r.merge.next()
	}
	if r.memPos >= len(r.mem) {
		return nil, false, nil
	}
	row := r.mem[r.memPos]
	r.memPos++
	return row, true, nil
}

// close releases any spill run files still open (a LIMIT can stop the merge before every run is
// drained — spill.md §4). A no-op for the in-memory case.
func (r *sortedRows) close() {
	if r.merge != nil {
		for _, s := range r.merge.sources {
			s.close()
		}
	}
}

// merger is the k-way merge over the run/buffer sources (spec/design/spill.md §4).
type merger struct {
	sources []*mergeSource
	heap    *mergeHeap
}

func (m *merger) next() (storedRow, bool, error) {
	if m.heap.Len() == 0 {
		return nil, false, nil
	}
	it := heap.Pop(m.heap).(*mergeItem)
	row, ok, err := m.sources[it.source].next()
	if err != nil {
		return nil, false, err
	}
	if ok {
		heap.Push(m.heap, &mergeItem{row: row, source: it.source})
	}
	return it.row, true, nil
}

// mergeSource is one merge input: a spilled run file (read back lazily, one row at a time) or the
// final in-memory buffer.
type mergeSource struct {
	isFile    bool
	f         *os.File
	r         *bufio.Reader
	path      string
	remaining uint64
	mem       []storedRow
	memIdx    int
}

func openRunSource(path string) (*mergeSource, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, ioError(err)
	}
	r := bufio.NewReader(f)
	remaining, err := spillReadU64(r)
	if err != nil {
		_ = f.Close()
		_ = os.Remove(path)
		return nil, ioError(err)
	}
	return &mergeSource{isFile: true, f: f, r: r, path: path, remaining: remaining}, nil
}

func (s *mergeSource) next() (storedRow, bool, error) {
	if !s.isFile {
		if s.memIdx >= len(s.mem) {
			return nil, false, nil
		}
		row := s.mem[s.memIdx]
		s.memIdx++
		return row, true, nil
	}
	if s.remaining == 0 {
		s.close() // exhausted — close + delete the run file eagerly
		return nil, false, nil
	}
	s.remaining--
	row, err := spillReadRow(s.r)
	if err != nil {
		return nil, false, ioError(err)
	}
	return row, true, nil
}

func (s *mergeSource) close() {
	if s.isFile && s.f != nil {
		_ = s.f.Close()
		_ = os.Remove(s.path)
		s.f = nil
	}
}

// mergeItem is a heap entry: the current head row of a source. The heap is a MIN-heap by the order
// keys, ties broken by the lowest source index — exactly input order, reproducing the in-memory
// stable sort (spec/design/spill.md §6).
type mergeItem struct {
	row    storedRow
	source int
}

type mergeHeap struct {
	items []*mergeItem
	keys  []orderSlot
}

func (h *mergeHeap) Len() int { return len(h.items) }
func (h *mergeHeap) Less(i, j int) bool {
	a, b := h.items[i], h.items[j]
	if c := cmpRows(h.keys, a.row, b.row); c != 0 {
		return c < 0
	}
	return a.source < b.source
}
func (h *mergeHeap) Swap(i, j int) { h.items[i], h.items[j] = h.items[j], h.items[i] }
func (h *mergeHeap) Push(x any)    { h.items = append(h.items, x.(*mergeItem)) }
func (h *mergeHeap) Pop() any {
	old := h.items
	n := len(old)
	it := old[n-1]
	h.items = old[:n-1]
	return it
}

// ---- per-core self-describing run codec (spill.md §4) ------------------------------------------

func spillWriteU32(w *bufio.Writer, n uint32) {
	var b [4]byte
	binary.LittleEndian.PutUint32(b[:], n)
	_, _ = w.Write(b[:])
}

func spillWriteU64(w *bufio.Writer, n uint64) {
	var b [8]byte
	binary.LittleEndian.PutUint64(b[:], n)
	_, _ = w.Write(b[:])
}

func spillWriteBytes(w *bufio.Writer, b []byte) {
	spillWriteU32(w, uint32(len(b)))
	_, _ = w.Write(b)
}

func spillWriteRow(w *bufio.Writer, row storedRow) {
	spillWriteU32(w, uint32(len(row)))
	for _, v := range row {
		spillWriteValue(w, v)
	}
}

func spillWriteValue(w *bufio.Writer, v Value) {
	switch v.Kind {
	case ValNull:
		_ = w.WriteByte(0)
	case ValInt:
		_ = w.WriteByte(1)
		spillWriteU64(w, uint64(v.Int))
	case ValBool:
		_ = w.WriteByte(2)
		if v.boolVal() {
			_ = w.WriteByte(1)
		} else {
			_ = w.WriteByte(0)
		}
	case ValText:
		_ = w.WriteByte(3)
		spillWriteBytes(w, []byte(v.str()))
	case ValDecimal:
		_ = w.WriteByte(4)
		neg, scale, groups := v.decimal().ToCodec()
		if neg {
			_ = w.WriteByte(1)
		} else {
			_ = w.WriteByte(0)
		}
		spillWriteU32(w, scale)
		spillWriteU32(w, uint32(len(groups)))
		for _, g := range groups {
			var gb [2]byte
			binary.LittleEndian.PutUint16(gb[:], g)
			_, _ = w.Write(gb[:])
		}
	case ValBytea:
		_ = w.WriteByte(5)
		spillWriteBytes(w, []byte(v.str()))
	case ValUuid:
		_ = w.WriteByte(6)
		_, _ = w.Write([]byte(v.str())) // exactly 16 bytes
	case ValTimestamp:
		_ = w.WriteByte(7)
		spillWriteU64(w, uint64(v.Int))
	case ValTimestamptz:
		_ = w.WriteByte(8)
		spillWriteU64(w, uint64(v.Int))
	case ValInterval:
		// Interval — tag 12 (tags 9/10/11 are the Unfetched forms below); months, days, micros.
		_ = w.WriteByte(12)
		spillWriteU32(w, uint32(v.interval().Months))
		spillWriteU32(w, uint32(v.interval().Days))
		spillWriteU64(w, uint64(v.interval().Micros))
	case ValComposite:
		// Composite — tag 15: field count then each field value, recursive (spec/design/composite.md).
		// Internal merge-sort scratch format only, so the recursion needs no type context.
		_ = w.WriteByte(15)
		spillWriteU32(w, uint32(len(*v.composite())))
		for _, f := range *v.composite() {
			spillWriteValue(w, f)
		}
	case ValArray:
		// Array — tag 16: ndim, then per-dimension (length, lower bound), then each element value,
		// recursive (spec/design/array.md). Internal merge-sort scratch format; the full shape
		// round-trips (multidim + custom bounds).
		_ = w.WriteByte(16)
		a := v.arrayVal()
		spillWriteU32(w, uint32(a.Ndim()))
		for d := 0; d < a.Ndim(); d++ {
			spillWriteU32(w, uint32(a.Dims[d]))
			spillWriteU32(w, uint32(a.Lbounds[d]))
		}
		for _, el := range a.Elements {
			spillWriteValue(w, el)
		}
	case ValRange:
		// Range — tag 18: the flags byte (EMPTY/LB_INF/UB_INF/LB_INC/UB_INC) then each present bound
		// value, recursive (spec/design/ranges.md §4). Internal merge-sort scratch format only, so the
		// recursion needs no element-type context — the bound Values round-trip themselves. A range
		// column can ride a spilling sort as a carried (non-key) column even before range ORDER BY
		// lands (R3), so it must spill faithfully now.
		rv := v.rangeVal()
		var flags byte
		if rv.Empty {
			flags |= 0x01
		}
		if rv.Lower == nil {
			flags |= 0x02
		}
		if rv.Upper == nil {
			flags |= 0x04
		}
		if rv.LowerInc {
			flags |= 0x08
		}
		if rv.UpperInc {
			flags |= 0x10
		}
		_ = w.WriteByte(18)
		_ = w.WriteByte(flags)
		if !rv.Empty {
			if rv.Lower != nil {
				spillWriteValue(w, *rv.Lower)
			}
			if rv.Upper != nil {
				spillWriteValue(w, *rv.Upper)
			}
		}
	case ValJson:
		// json — tag 19: the verbatim text (spec/design/json.md). Internal merge-sort scratch format
		// only; a json/jsonb column can ride a spilling sort as a carried column, so it must spill
		// faithfully.
		_ = w.WriteByte(19)
		spillWriteBytes(w, []byte(v.str()))
	case ValJsonb:
		// jsonb — tag 20: the canonical text (jsonbOut → jsonbIn round-trips exactly, since the
		// output is canonical), spec/design/json.md.
		_ = w.WriteByte(20)
		spillWriteBytes(w, []byte(jsonbOut(v.jsonb())))
	case ValJsonPath:
		// jsonpath is literal-only (non-storable), so it never rides a spilling sort.
		panic("BUG: a jsonpath value never reaches the spill codec")
	case ValUnfetched:
		// An untouched large-value reference rides along to the output unread (spill.md §4); spill
		// it opaquely so it round-trips, never resolving it. The same pass-through covers an
		// inline-deferred value (lazy-record.md §5b) — tag 21.
		switch v.unfetched().Form {
		case 0x00:
			_ = w.WriteByte(21)
			spillWriteBytes(w, v.unfetched().Comp)
		case tagExternal:
			_ = w.WriteByte(9)
			spillWriteU32(w, v.unfetched().FirstPage)
			spillWriteU32(w, v.unfetched().StoredLen)
		case tagInlineComp:
			_ = w.WriteByte(10)
			spillWriteU32(w, v.unfetched().RawLen)
			spillWriteBytes(w, v.unfetched().Comp)
		case tagExternalComp:
			_ = w.WriteByte(11)
			spillWriteU32(w, v.unfetched().FirstPage)
			spillWriteU32(w, v.unfetched().StoredLen)
			spillWriteU32(w, v.unfetched().RawLen)
		}
	}
}

func spillReadU8(r *bufio.Reader) (byte, error) { return r.ReadByte() }
func spillReadU32(r *bufio.Reader) (uint32, error) {
	var b [4]byte
	if _, err := io.ReadFull(r, b[:]); err != nil {
		return 0, err
	}
	return binary.LittleEndian.Uint32(b[:]), nil
}

func spillReadU64(r *bufio.Reader) (uint64, error) {
	var b [8]byte
	if _, err := io.ReadFull(r, b[:]); err != nil {
		return 0, err
	}
	return binary.LittleEndian.Uint64(b[:]), nil
}

func spillReadBytes(r *bufio.Reader) ([]byte, error) {
	n, err := spillReadU32(r)
	if err != nil {
		return nil, err
	}
	b := make([]byte, n)
	if _, err := io.ReadFull(r, b); err != nil {
		return nil, err
	}
	return b, nil
}

func spillReadRow(r *bufio.Reader) (storedRow, error) {
	ncols, err := spillReadU32(r)
	if err != nil {
		return nil, err
	}
	row := make(storedRow, ncols)
	for i := range row {
		v, err := spillReadValue(r)
		if err != nil {
			return nil, err
		}
		row[i] = v
	}
	return row, nil
}

func spillReadValue(r *bufio.Reader) (Value, error) {
	tag, err := spillReadU8(r)
	if err != nil {
		return Value{}, err
	}
	switch tag {
	case 0:
		return NullValue(), nil
	case 1:
		n, err := spillReadU64(r)
		return IntValue(int64(n)), err
	case 2:
		b, err := spillReadU8(r)
		return BoolValue(b != 0), err
	case 3:
		b, err := spillReadBytes(r)
		return TextValue(string(b)), err
	case 4:
		neg, err := spillReadU8(r)
		if err != nil {
			return Value{}, err
		}
		scale, err := spillReadU32(r)
		if err != nil {
			return Value{}, err
		}
		ng, err := spillReadU32(r)
		if err != nil {
			return Value{}, err
		}
		groups := make([]uint16, ng)
		var gb [2]byte
		for i := range groups {
			if _, err := io.ReadFull(r, gb[:]); err != nil {
				return Value{}, err
			}
			groups[i] = binary.LittleEndian.Uint16(gb[:])
		}
		return DecimalValue(decimalFromCodec(neg != 0, scale, groups)), nil
	case 5:
		b, err := spillReadBytes(r)
		return ByteaValue(b), err
	case 6:
		var u [16]byte
		if _, err := io.ReadFull(r, u[:]); err != nil {
			return Value{}, err
		}
		return UuidValue(u[:]), nil
	case 7:
		n, err := spillReadU64(r)
		return TimestampValue(int64(n)), err
	case 8:
		n, err := spillReadU64(r)
		return TimestamptzValue(int64(n)), err
	// Tags 9/10/11/21 reload a deferred value's pointer fields. The run file cannot carry runtime
	// handles, so the reloaded Unfetched keeps the NIL SENTINEL handles (types == nil, the Go zero
	// value) — it rides the sort output UNREAD by contract (spill.md §4), and touching one stays
	// the loud pre-B4 poison panic (resolveUnfetchedSelf; bplus-reshape.md §5).
	case 9:
		first, err := spillReadU32(r)
		if err != nil {
			return Value{}, err
		}
		length, err := spillReadU32(r)
		return Value{Kind: ValUnfetched, ref: &Unfetched{Form: tagExternal, FirstPage: first, StoredLen: length}}, err
	case 10:
		raw, err := spillReadU32(r)
		if err != nil {
			return Value{}, err
		}
		comp, err := spillReadBytes(r)
		return Value{Kind: ValUnfetched, ref: &Unfetched{Form: tagInlineComp, RawLen: raw, Comp: comp}}, err
	case 11:
		first, err := spillReadU32(r)
		if err != nil {
			return Value{}, err
		}
		stored, err := spillReadU32(r)
		if err != nil {
			return Value{}, err
		}
		raw, err := spillReadU32(r)
		return Value{Kind: ValUnfetched, ref: &Unfetched{Form: tagExternalComp, FirstPage: first, StoredLen: stored, RawLen: raw}}, err
	case 21:
		body, err := spillReadBytes(r)
		return Value{Kind: ValUnfetched, ref: &Unfetched{Form: 0x00, Comp: body}}, err
	case 12:
		months, err := spillReadU32(r)
		if err != nil {
			return Value{}, err
		}
		days, err := spillReadU32(r)
		if err != nil {
			return Value{}, err
		}
		micros, err := spillReadU64(r)
		return IntervalValue(Interval{Months: int32(months), Days: int32(days), Micros: int64(micros)}), err
	case 15:
		n, err := spillReadU32(r)
		if err != nil {
			return Value{}, err
		}
		fields := make([]Value, n)
		for i := range fields {
			f, err := spillReadValue(r)
			if err != nil {
				return Value{}, err
			}
			fields[i] = f
		}
		return CompositeValue(fields), nil
	case 16:
		ndim, err := spillReadU32(r)
		if err != nil {
			return Value{}, err
		}
		dims := make([]int, ndim)
		lbounds := make([]int32, ndim)
		n := 1
		for d := 0; d < int(ndim); d++ {
			ln, err := spillReadU32(r)
			if err != nil {
				return Value{}, err
			}
			lb, err := spillReadU32(r)
			if err != nil {
				return Value{}, err
			}
			dims[d] = int(ln)
			lbounds[d] = int32(lb)
			n *= int(ln)
		}
		elems := make([]Value, n)
		for i := range elems {
			el, err := spillReadValue(r)
			if err != nil {
				return Value{}, err
			}
			elems[i] = el
		}
		return arrayValueOf(&ArrayVal{Dims: dims, Lbounds: lbounds, Elements: elems}), nil
	case 18:
		flags, err := spillReadU8(r)
		if err != nil {
			return Value{}, err
		}
		if flags&0x01 != 0 {
			return RangeValue(emptyRangeVal()), nil
		}
		lbInf := flags&0x02 != 0
		ubInf := flags&0x04 != 0
		var lower, upper *Value
		if !lbInf {
			lo, err := spillReadValue(r)
			if err != nil {
				return Value{}, err
			}
			lower = &lo
		}
		if !ubInf {
			hi, err := spillReadValue(r)
			if err != nil {
				return Value{}, err
			}
			upper = &hi
		}
		return RangeValue(&RangeVal{
			Empty:    false,
			Lower:    lower,
			Upper:    upper,
			LowerInc: flags&0x08 != 0,
			UpperInc: flags&0x10 != 0,
		}), nil
	case 19:
		b, err := spillReadBytes(r)
		if err != nil {
			return Value{}, err
		}
		return JsonValue(string(b)), nil
	case 20:
		b, err := spillReadBytes(r)
		if err != nil {
			return Value{}, err
		}
		node, err := jsonbIn(string(b))
		if err != nil {
			return Value{}, io.ErrUnexpectedEOF
		}
		return JsonbValue(node), nil
	default:
		return Value{}, io.ErrUnexpectedEOF
	}
}
