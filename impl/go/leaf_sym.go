package jed

import (
	"encoding/binary"
	"math/bits"
	"os"
	"sort"
	"sync"
	"sync/atomic"
)

// PROTOTYPE: two more variable-width region encodings, measured in
// spec/design/leaf-encodings-experiment.md §8 and described in spec/design/leaf-structural.md.
//
// leafSym (JED_LEAF_SYM=on, with JED_LEAF_NARROW=on) lets a leaf write a variable-width column as a
// symbol-table region: up to 255 byte strings of 2 to 8 bytes, each standing for one code byte, with
// every other byte standing for itself. The region's flags byte is regionSym; then
//
//	esc u8, K u8, K × (code u8, length u8), the K symbols,
//	a raw bitmap (ceil(N/8), MSB-first), N end offsets, the stored bodies.
//
// A value whose raw bit is clear is an inline-plain value: its body is its content payload P(v) in
// codes, and a reader rebuilds the header from the column type. A code byte is a symbol; the esc byte
// is followed by one literal byte; any other byte is itself. A literal equal to esc or to a symbol's
// code is written escaped. A value whose raw bit is set stores its span as it is (a compressed or
// external form), or nothing when it is NULL.
//
// A symbol table's size is only known by building it, so the table travels with the leaf: it is
// trained from the leaf's rows when the leaf first outgrows its plain size, later rows are measured
// and written under that same table, and a split hands it to the halves. Under a fixed table the
// region's size is a sum over its values, which is what split planning needs. A leaf that fits
// without the table is written without it.
var leafSym = os.Getenv("JED_LEAF_SYM") == "on" && leafNarrow

// leafArr (JED_LEAF_ARR=on, with JED_LEAF_NARROW=on) lets a leaf write a column of i16/i32/i64 arrays
// as counts plus bit-packed offsets from the region's smallest element. The region's flags byte is
// regionArr; then N count bytes (255 = NULL, 0 = the empty array), the smallest element at the
// element width, a u8 bit width, and every element's offset packed MSB-first. It applies only when
// every value is NULL, empty, or an inline one-dimensional array with lower bound 1, no NULL element
// and at most 254 elements, and only when smaller.
var leafArr = os.Getenv("JED_LEAF_ARR") == "on" && leafNarrow

const (
	regionSym byte = 3
	regionArr byte = 4
)

const symMaxLen = 8

// symTable is one column's symbol table. Immutable once built.
type symTable struct {
	syms  [][]byte
	codes []byte // codes[k] is symbol k's code byte
	esc   byte
	// special marks esc and every code: a literal of that value is written escaped.
	special [256]bool
	// m is the encoder's matcher, built on first use (a table read from a page is mostly only decoded).
	m     *symMatcher
	mOnce sync.Once
	// the decoder's view: dlen[c] is the length of the symbol with code c (0: not a code), dsym its
	// bytes little-endian, so one 8-byte store writes it.
	dlen [256]uint8
	dsym [256]uint64
	size int // the table's bytes on the page
}

// symCand is one symbol as the matcher tests it: its bytes little-endian and a mask of its length.
type symCand struct {
	val, mask uint64
	k         uint16
	n         uint8
}

// symMatcher is a table's symbols by first byte, longest first.
type symMatcher struct {
	first [256][]symCand
}

func (tb *symTable) matcher() *symMatcher {
	tb.mOnce.Do(tb.index)
	return tb.m
}

func (tb *symTable) index() {
	m := &symMatcher{}
	tb.m = m
	order := make([]int, len(tb.syms))
	for i := range order {
		order[i] = i
	}
	sort.SliceStable(order, func(a, b int) bool { return len(tb.syms[order[a]]) > len(tb.syms[order[b]]) })
	for _, k := range order {
		sym := tb.syms[k]
		var v [8]byte
		copy(v[:], sym)
		c := symCand{val: binary.LittleEndian.Uint64(v[:]), mask: ^uint64(0) >> (64 - 8*len(sym)), k: uint16(k), n: uint8(len(sym))}
		m.first[sym[0]] = append(m.first[sym[0]], c)
	}
}

// seal assigns the codes and fills the derived fields.
func (tb *symTable) seal(esc byte, codes []byte) {
	tb.esc, tb.codes = esc, codes
	tb.special[esc] = true
	tb.size = 2 + 2*len(tb.syms)
	for k, s := range tb.syms {
		c := codes[k]
		tb.special[c] = true
		tb.dlen[c] = uint8(len(s))
		var v [8]byte
		copy(v[:], s)
		tb.dsym[c] = binary.LittleEndian.Uint64(v[:])
		tb.size += len(s)
	}
}

// match returns the unit at p (256+k for symbol k, else the literal byte) and its length.
func (m *symMatcher) match(p []byte, at int) (unit, n int) {
	cands := m.first[p[at]]
	if len(cands) == 0 {
		return int(p[at]), 1
	}
	var w uint64
	left := len(p) - at
	if left >= 8 {
		w = binary.LittleEndian.Uint64(p[at:])
	} else {
		var v [8]byte
		copy(v[:], p[at:])
		w = binary.LittleEndian.Uint64(v[:])
	}
	for i := range cands {
		c := &cands[i]
		if w&c.mask == c.val && int(c.n) <= left {
			return 256 + int(c.k), int(c.n)
		}
	}
	return int(p[at]), 1
}

func (tb *symTable) unitBytes(u int) []byte {
	if u < 256 {
		return []byte{byte(u)}
	}
	return tb.syms[u-256]
}

// encLen is the coded length of payload p.
func (tb *symTable) encLen(p []byte) int {
	total := 0
	m := tb.matcher()
	for at := 0; at < len(p); {
		u, n := m.match(p, at)
		total++
		if u < 256 && tb.special[u] {
			total++
		}
		at += n
	}
	return total
}

func (tb *symTable) appendEnc(out, p []byte) []byte {
	m := tb.matcher()
	for at := 0; at < len(p); {
		u, n := m.match(p, at)
		switch {
		case u >= 256:
			out = append(out, tb.codes[u-256])
		case tb.special[u]:
			out = append(out, tb.esc, byte(u))
		default:
			out = append(out, byte(u))
		}
		at += n
	}
	return out
}

func (tb *symTable) appendTable(out []byte) []byte {
	out = append(out, tb.esc, byte(len(tb.syms)))
	for k, s := range tb.syms {
		out = append(out, tb.codes[k], byte(len(s)))
	}
	for _, s := range tb.syms {
		out = append(out, s...)
	}
	return out
}

// parseSymTable reads a table and returns it with the bytes after it.
func parseSymTable(b []byte) (*symTable, []byte, error) {
	bad := func() (*symTable, []byte, error) {
		return nil, nil, newError(DataCorrupted, "PAX leaf symbol table is malformed")
	}
	if len(b) < 2 {
		return bad()
	}
	k := int(b[1])
	if k > 255 || len(b) < 2+2*k {
		return bad()
	}
	tb := &symTable{}
	codes := make([]byte, k)
	var used [256]bool
	used[b[0]] = true
	at := 2 + 2*k
	for i := 0; i < k; i++ {
		c, l := b[2+2*i], int(b[3+2*i])
		if used[c] || l < 1 || l > symMaxLen || at+l > len(b) {
			return bad()
		}
		used[c] = true
		codes[i] = c
		tb.syms = append(tb.syms, b[at:at+l])
		at += l
	}
	tb.seal(b[0], codes)
	return tb, b[at:], nil
}

// trainSym builds a table from a column's inline payloads, or nil when no symbol repays its entry.
// Five passes: parse with the current table, count units and adjacent pairs, and keep the candidates
// (pair concatenations and existing symbols) that save the most bytes. Ties break by the symbol's
// bytes, so the table is a function of the payloads in order.
func trainSym(payloads [][]byte) *symTable {
	var freq [256]int
	total := 0
	for _, p := range payloads {
		total += len(p)
		for _, c := range p {
			freq[c]++
		}
	}
	if total < 16 {
		return nil
	}
	// Code bytes: the values the payloads never use, rarest first; at least 16, taking the rarest
	// used values when fewer are free. The first is the escape.
	order := make([]int, 256)
	for i := range order {
		order[i] = i
	}
	sort.SliceStable(order, func(a, b int) bool { return freq[order[a]] < freq[order[b]] })
	ncodes := 0
	for ncodes < 256 && freq[order[ncodes]] == 0 {
		ncodes++
	}
	if ncodes < 16 {
		ncodes = 16
	}
	maxSyms := min(ncodes-1, 255)

	tb := &symTable{}
	type cand struct {
		s    string
		gain int
	}
	c1 := make([]int, 256+255)
	// Pair counts: an open-addressed table keyed by (unit, unit), cleared each pass.
	const slots = 1 << 14
	keys := make([]uint32, slots)
	counts := make([]int32, slots)
	var used []uint32
	var cs []cand
	for gen := 0; gen < 5; gen++ {
		tb.index()
		m := tb.m
		for i := range c1 {
			c1[i] = 0
		}
		for _, h := range used {
			keys[h], counts[h] = 0, 0
		}
		used = used[:0]
		full := false
		for _, p := range payloads {
			prev := -1
			for at := 0; at < len(p); {
				u, n := m.match(p, at)
				c1[u]++
				if prev >= 0 && !full {
					k := uint32(prev)<<16 | uint32(u) | 1<<31 // never 0
					h := (k * 2654435761) >> 18
					for keys[h] != 0 && keys[h] != k {
						h = (h + 1) & (slots - 1)
					}
					if keys[h] == 0 {
						keys[h] = k
						used = append(used, h)
						full = len(used) >= slots*3/4 // later pairs are not counted
					}
					counts[h]++
				}
				prev = u
				at += n
			}
		}
		cs = cs[:0]
		add := func(s []byte, count int) {
			if len(s) < 2 {
				return
			}
			if g := count*(len(s)-1) - (len(s) + 2); g > 0 {
				cs = append(cs, cand{string(s), g})
			}
		}
		for u := 256; u < 256+len(tb.syms); u++ {
			add(tb.syms[u-256], c1[u])
		}
		var cat [2 * symMaxLen]byte
		for _, h := range used {
			k := keys[h] &^ (1 << 31)
			a, b := tb.unitBytes(int(k>>16)), tb.unitBytes(int(k&0xffff))
			if len(a)+len(b) > symMaxLen {
				continue
			}
			n := copy(cat[:], a)
			n += copy(cat[n:], b)
			add(cat[:n], int(counts[h]))
		}
		// One candidate per distinct string (its largest gain), then the best maxSyms.
		sort.Slice(cs, func(a, b int) bool {
			if cs[a].s != cs[b].s {
				return cs[a].s < cs[b].s
			}
			return cs[a].gain > cs[b].gain
		})
		w := 0
		for i, c := range cs {
			if i == 0 || c.s != cs[w-1].s {
				cs[w] = c
				w++
			}
		}
		cs = cs[:w]
		sort.Slice(cs, func(a, b int) bool {
			if cs[a].gain != cs[b].gain {
				return cs[a].gain > cs[b].gain
			}
			return cs[a].s < cs[b].s
		})
		if len(cs) > maxSyms {
			cs = cs[:maxSyms]
		}
		tb.syms = tb.syms[:0]
		for _, c := range cs {
			tb.syms = append(tb.syms, []byte(c.s))
		}
	}
	if len(tb.syms) == 0 {
		return nil
	}
	codes := make([]byte, len(tb.syms))
	for k := range codes {
		codes[k] = byte(order[1+k])
	}
	tb.seal(byte(order[0]), codes)
	return tb
}

// symEligible reports whether a column may be written as a symbol-table region: a variable-width
// column whose inline-plain value is a header then its content payload. Integer arrays take the
// typed encoding instead.
func symEligible(ty colType) bool {
	if !leafSym || !isSpillable(ty) || arrElemWidth(ty) != 0 {
		return false
	}
	_, fixed := fixedValueWidth(ty)
	return !fixed
}

// arrElemWidth is the element width of a column leafArr applies to, or 0.
func arrElemWidth(ty colType) int {
	if !leafArr || ty.Elem == nil || ty.Composite || ty.RangeElem != nil {
		return 0
	}
	e := *ty.Elem
	if e.Composite || e.Elem != nil || e.RangeElem != nil {
		return 0
	}
	switch e.Scalar {
	case scalarInt16:
		return 2
	case scalarInt32:
		return 4
	case scalarInt64:
		return 8
	}
	return 0
}

// symSaving is the bytes a symbol-table region saves over the plain one: gain is the sum, over the
// inline-plain values, of header + payload − coded length.
func symSaving(n, gain int, tb *symTable) int {
	if tb == nil {
		return 0
	}
	if s := gain - tb.size - (n+7)/8; s > 0 {
		return s
	}
	return 0
}

// inlineHeader appends the inline-plain header of a payload of length n in a column of type ty, as
// this process writes it.
func inlineHeader(out []byte, ty colType, n int) []byte {
	if stringPayload(ty) {
		if leafBareString {
			return append(out, tagInlineBare)
		}
		return append(out, 0x00, byte(n>>8), byte(n))
	}
	return append(out, 0x00)
}

// spanItem classifies a stored span of a column of type ty: NULL, an inline-plain value with its
// content payload, or any other form.
func spanItem(ty colType, span []byte) dictItem {
	if len(span) == 0 {
		return dictItem{kind: dictNull}
	}
	if stringPayload(ty) {
		return spanDictItem(span)
	}
	if span[0] == 0x00 {
		return dictItem{kind: dictShared, payload: span[1:]}
	}
	return dictItem{kind: dictOwn}
}

// arrAcc accumulates a typed-array region's facts.
type arrAcc struct {
	nulls, empties, arrays int // values by kind
	elems                  int // elements over all arrays
	r                      forRange
	bad                    bool // a value the typed encoding cannot hold
}

// add takes one value: its kind and, for an inline-plain value, its array body.
func (a *arrAcc) add(it dictItem, w int) {
	switch it.kind {
	case dictNull:
		a.nulls++
	case dictOwn:
		a.bad = true
	default:
		elems, ok := arrElems(it.payload, w)
		switch {
		case !ok:
			a.bad = true
		case len(elems) == 0:
			a.empties++
		default:
			a.arrays++
			a.elems += len(elems) / w
			for at := 0; at < len(elems); at += w {
				a.r.add(beUint(elems[at : at+w]))
			}
		}
	}
}

// arrElems returns the element bytes of an array body the typed encoding holds (empty for the empty
// array): one dimension, lower bound 1, no NULL element, at most 254 elements of width w.
func arrElems(body []byte, w int) ([]byte, bool) {
	if len(body) == 2 && body[0] == 0 && body[1] == 0 {
		return nil, true
	}
	if len(body) < 10 || body[0] != 1 || body[1] != 0 {
		return nil, false
	}
	m := int(binary.BigEndian.Uint32(body[2:]))
	if m < 1 || m > 254 || binary.BigEndian.Uint32(body[6:]) != 1 || len(body) != 10+m*w {
		return nil, false
	}
	return body[10:], true
}

func arrBits(r forRange) int {
	if !r.any {
		return 0
	}
	return bits.Len64(r.hi - r.lo)
}

// arrSaving is the bytes the typed encoding saves in a region of n values of element width w (dw is
// the directory entry width).
func arrSaving(n, w, dw int, a arrAcc) int {
	if a.bad {
		return 0
	}
	plain := dw*n + 3*a.empties + 11*a.arrays + a.elems*w
	typed := n + w + 1 + (a.elems*arrBits(a.r)+7)/8
	if plain > typed {
		return plain - typed
	}
	return 0
}

// colAcc accumulates one item column's content facts: its dictionary, its gain under a symbol table,
// and its typed-array facts.
type colAcc struct {
	dict dictAcc
	f    colFacts
}

// colFacts is the copyable part of colAcc, which split planning records at every boundary.
type colFacts struct {
	d, dup int
	gain   int
	arr    arrAcc
}

func (a *colAcc) add(shape leafShape, j int, it dictItem, tb *symTable) {
	c := shape.itemCols[j]
	header := inlineHeaderLen(shape.colTypes[c])
	if shape.itemDict[j] {
		a.dict.add(it, header)
		a.f.d, a.f.dup = a.dict.d, a.dict.dup
	}
	if tb != nil && it.kind == dictShared {
		a.f.gain += header + len(it.payload) - tb.encLen(it.payload)
	}
	if w := shape.itemArr[j]; w != 0 {
		a.f.arr.add(it, w)
	}
}

// itemSaving is the bytes item column j saves in a leaf of n records with these facts. tb is the
// column's symbol table, nil when the leaf is written without one.
func itemSaving(shape leafShape, j, n int, f colFacts, tb *symTable) int {
	if w := shape.itemArr[j]; w != 0 {
		return arrSaving(n, w, 2, f.arr)
	}
	s := 0
	if shape.itemDict[j] {
		s = dictSaving(n, f.d, f.dup)
	}
	if t := symSaving(n, f.gain, tb); t > s {
		s = t
	}
	return s
}

// trainTabs trains a table for every symbol-table column from the rows' items (items[i][j] is row i's
// item for shape.itemCols[j]). The result is indexed by column; nil when the shape has no such column.
func trainTabs(shape leafShape, items [][]dictItem) []*symTable {
	if !shape.hasSym {
		return nil
	}
	tabs := make([]*symTable, len(shape.colTypes))
	payloads := make([][]byte, 0, len(items))
	for j, c := range shape.itemCols {
		if !shape.itemSym[j] {
			continue
		}
		payloads = payloads[:0]
		for i := range items {
			if it := items[i][j]; it.kind == dictShared {
				payloads = append(payloads, it.payload)
			}
		}
		tabs[c] = trainSym(payloads)
	}
	return tabs
}

func tabFor(tabs []*symTable, c int) *symTable {
	if tabs == nil {
		return nil
	}
	return tabs[c]
}

// lazyRegion is a symbol-table or typed-array region of a packed leaf, kept as its page bytes and
// rebuilt into the plain form (end offsets and tagged spans) the first time a value of the column is
// read.
type lazyRegion struct {
	kind byte
	ty   colType
	raw  []byte // the region after its flags byte
	n    int
	dw   int
	// regionSym: the table's bytes on the page, and the table once something needs it.
	tabSize int
	tab     atomic.Pointer[symTable]
	mat     atomic.Pointer[plainRegion]
}

// plainRegion is a rebuilt region: N u32 end offsets and the spans.
type plainRegion struct {
	ends []byte
	body []byte
	err  error
}

// symTableSize is the length of the table at the head of b, checked against b's length.
func symTableSize(b []byte) (int, error) {
	if len(b) >= 2 {
		k := int(b[1])
		if size := 2 + 2*k; len(b) >= size {
			for i := 0; i < k; i++ {
				size += int(b[3+2*i])
			}
			if size <= len(b) {
				return size, nil
			}
		}
	}
	return 0, newError(DataCorrupted, "PAX leaf symbol table is malformed")
}

func (r *lazyRegion) table() (*symTable, error) {
	if tb := r.tab.Load(); tb != nil {
		return tb, nil
	}
	tb, _, err := parseSymTable(r.raw)
	if err != nil {
		return nil, err
	}
	if tb.size != r.tabSize {
		return nil, newError(DataCorrupted, "PAX leaf symbol table is malformed")
	}
	r.tab.CompareAndSwap(nil, tb)
	return r.tab.Load(), nil
}

func (r *lazyRegion) plain() *plainRegion {
	if m := r.mat.Load(); m != nil {
		return m
	}
	m := &plainRegion{}
	if r.kind == regionSym {
		m.ends, m.body, m.err = r.decodeSym()
	} else {
		m.ends, m.body, m.err = r.decodeArr()
	}
	if r.mat.CompareAndSwap(nil, m) {
		return m
	}
	return r.mat.Load()
}

func (r *lazyRegion) decodeSym() (ends, body []byte, err error) {
	bad := newError(DataCorrupted, "PAX leaf symbol-table region is malformed")
	tb, err := r.table()
	if err != nil {
		return nil, nil, err
	}
	b := r.raw[tb.size:]
	bm := (r.n + 7) / 8
	if len(b) < bm+r.dw*r.n {
		return nil, nil, bad
	}
	rawBits, dir, enc := b[:bm], b[bm:bm+r.dw*r.n], b[bm+r.dw*r.n:]
	ends = make([]byte, 0, 4*r.n)
	body = make([]byte, 0, 2*len(enc)+16)
	lo := 0
	for i := 0; i < r.n; i++ {
		hi := int(paxDirEnd(dir, i, r.dw))
		if hi < lo || hi > len(enc) {
			return nil, nil, bad
		}
		v := enc[lo:hi]
		lo = hi
		if rawBits[i/8]&(0x80>>(i%8)) != 0 {
			body = append(body, v...)
			ends = appendU32(ends, uint32(len(body)))
			continue
		}
		// Room for the widest header and for every code as a whole 8-byte store.
		start := len(body)
		if need := start + 3 + 8*len(v) + 8; cap(body) < need {
			grown := make([]byte, start, need+need/2)
			copy(grown, body)
			body = grown
		}
		hdr := 1
		if stringPayload(r.ty) && !leafBareString {
			hdr = 3
		}
		out := body[:cap(body)]
		o := start + hdr
		for p := 0; p < len(v); p++ {
			c := v[p]
			if l := tb.dlen[c]; l != 0 {
				binary.LittleEndian.PutUint64(out[o:], tb.dsym[c])
				o += int(l)
			} else if c == tb.esc {
				p++
				if p == len(v) {
					return nil, nil, bad
				}
				out[o] = v[p]
				o++
			} else {
				out[o] = c
				o++
			}
		}
		plen := o - start - hdr
		if hdr == 3 && plen > 0xffff {
			return nil, nil, bad
		}
		inlineHeader(out[:start], r.ty, plen)
		body = out[:o]
		ends = appendU32(ends, uint32(len(body)))
	}
	if lo != len(enc) {
		return nil, nil, bad
	}
	return ends, body, nil
}

func (r *lazyRegion) decodeArr() (ends, body []byte, err error) {
	bad := newError(DataCorrupted, "PAX leaf typed-array region is malformed")
	w := arrElemWidthOf(r.ty)
	if w == 0 || len(r.raw) < r.n+w+1 {
		return nil, nil, bad
	}
	counts := r.raw[:r.n]
	base := beUint(r.raw[r.n : r.n+w])
	bw := int(r.raw[r.n+w])
	data := r.raw[r.n+w+1:]
	total := 0
	for _, c := range counts {
		if c != 255 {
			total += int(c)
		}
	}
	if bw > 8*w || len(data) != (total*bw+7)/8 {
		return nil, nil, bad
	}
	ends = make([]byte, 0, 4*r.n)
	body = make([]byte, 0, 11*r.n+total*w)
	bit := 0
	for _, c := range counts {
		switch c {
		case 255:
		case 0:
			body = append(body, 0x00, 0, 0)
		default:
			body = append(body, 0x00, 1, 0)
			body = appendU32(body, uint32(c))
			body = appendU32(body, 1)
			for k := 0; k < int(c); k++ {
				at := len(body)
				body = append(body, make([]byte, w)...)
				putBEUint(body[at:], base+getBits(data, bit, bw))
				bit += bw
			}
		}
		ends = appendU32(ends, uint32(len(body)))
	}
	return ends, body, nil
}

// arrElemWidthOf is arrElemWidth without the switch: a reader decodes whatever the page holds.
func arrElemWidthOf(ty colType) int {
	if ty.Elem == nil {
		return 0
	}
	switch ty.Elem.Scalar {
	case scalarInt16:
		return 2
	case scalarInt32:
		return 4
	case scalarInt64:
		return 8
	}
	return 0
}

// getBits reads bw bits at bit offset at, MSB-first.
func getBits(b []byte, at, bw int) uint64 {
	var v uint64
	for i := 0; i < bw; i++ {
		p := at + i
		v = v<<1 | uint64(b[p/8]>>(7-p%8)&1)
	}
	return v
}

// putBits writes the low bw bits of v at bit offset at, MSB-first, into zeroed bytes.
func putBits(b []byte, at, bw int, v uint64) {
	for i := 0; i < bw; i++ {
		if v>>(bw-1-i)&1 != 0 {
			p := at + i
			b[p/8] |= 0x80 >> (p % 8)
		}
	}
}

// packedItemSaving is the bytes a packed leaf's item columns save, read from the page: a region
// stored as a symbol table or typed array saves its plain size less its stored size; any other is
// measured as a dictionary.
func packedItemSaving(shape leafShape, l *paxLeaf, n int) int {
	total := 0
	for j, c := range shape.itemCols {
		r := &l.regions[c]
		if r.lazy != nil {
			m := r.lazy.plain()
			if m.err != nil {
				return 0
			}
			total += 1 + l.dw*n + len(m.body) - r.stored
			continue
		}
		if !shape.itemDict[j] {
			continue
		}
		var acc dictAcc
		header := inlineHeaderLen(shape.colTypes[c])
		for i := 0; i < n; i++ {
			span, err := l.value(c, i)
			if err != nil {
				return 0
			}
			acc.add(spanDictItem(span), header)
		}
		total += dictSaving(n, acc.d, acc.dup)
	}
	return total
}

// packedTabs is the symbol tables a packed leaf's page carries, by column; nil when it has none.
func packedTabs(l *paxLeaf) []*symTable {
	var tabs []*symTable
	for c := range l.regions {
		if lz := l.regions[c].lazy; lz != nil && lz.kind == regionSym {
			tb, err := lz.table()
			if err != nil {
				continue
			}
			if tabs == nil {
				tabs = make([]*symTable, len(l.regions))
			}
			tabs[c] = tb
		}
	}
	return tabs
}

// peek answers, from the page bytes alone, whether value i is NULL and, when it is not, whether it is
// stored raw (raw is then its span: a compressed or external form). A typed-array value and a coded
// symbol-table value are inline-plain, so neither can own an overflow chain.
func (r *lazyRegion) peek(i int) (null bool, raw []byte, ok bool) {
	if r.kind == regionArr {
		return i < len(r.raw) && r.raw[i] == 255, nil, false
	}
	b := r.raw[r.tabSize:]
	bm := (r.n + 7) / 8
	if len(b) < bm+r.dw*r.n || b[i/8]&(0x80>>(i%8)) == 0 {
		return false, nil, false
	}
	dir, enc := b[bm:bm+r.dw*r.n], b[bm+r.dw*r.n:]
	lo := 0
	if i > 0 {
		lo = int(paxDirEnd(dir, i-1, r.dw))
	}
	hi := int(paxDirEnd(dir, i, r.dw))
	if lo > hi || hi > len(enc) {
		return false, nil, false // malformed: reading the value reports it
	}
	return lo == hi, enc[lo:hi], lo != hi
}

// chainSpan returns the stored span of value (record i, column c) for a walk that looks for overflow
// chains, and skip when the value cannot own one: a NULL, or an inline value of a symbol-table or
// typed-array region (which the walk then never rebuilds).
func (l *paxLeaf) chainSpan(c, i int) (span []byte, skip bool, err error) {
	if lz := l.regions[c].lazy; lz != nil {
		_, raw, ok := lz.peek(i)
		return raw, !ok, nil
	}
	if l.isNull(c, i) {
		return nil, true, nil
	}
	span, err = l.value(c, i)
	return span, false, err
}
