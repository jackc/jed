package jed

import (
	"encoding/binary"
	"math/bits"
	"os"
	"slices"
	"strconv"
	"sync"
	"sync/atomic"
	"time"
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

// leafSymCol (JED_LEAF_SYM=column, with JED_LEAF_NARROW=on) keeps one symbol table per column of a
// table instead of one per leaf. The table is trained once, when the table's tree first has
// symColLeaves leaves (or a third level), from its rows in key order up to symColSample bytes of
// payload over the eligible columns; it is written once to an overflow chain of its own, which the
// table's catalog entry names (symChainRef), and never replaced. A region written with it has the flags byte regionSymCol and the layout of regionSym
// without the table. The saving is a sum over the leaf's values less the raw bitmap, so a leaf's
// size is a function of its rows and its table's tables. As with per-leaf tables, a leaf that fits
// as plain regions is written plain: an append rewrites its half-empty last leaf at every commit,
// and coding it then buys no page.
var leafSymCol = os.Getenv("JED_LEAF_SYM") == "column" && leafNarrow

var (
	symColLeaves = envInt("JED_LEAF_SYM_COL_LEAVES", 32)
	symColSample = envInt("JED_LEAF_SYM_COL_SAMPLE", 512<<10)
)

func envInt(name string, def int) int {
	if n, err := strconv.Atoi(os.Getenv(name)); err == nil && n > 0 {
		return n
	}
	return def
}

// leafArr (JED_LEAF_ARR=on, with JED_LEAF_NARROW=on) lets a leaf write a column of i16/i32/i64 arrays
// as counts plus bit-packed offsets from the region's smallest element. The region's flags byte is
// regionArr; then N count bytes (255 = NULL, 0 = the empty array), the smallest element at the
// element width, a u8 bit width, and every element's offset packed MSB-first. It applies only when
// every value is NULL, empty, or an inline one-dimensional array with lower bound 1, no NULL element
// and at most 254 elements, and only when smaller.
var leafArr = os.Getenv("JED_LEAF_ARR") == "on" && leafNarrow

const (
	regionSym    byte = 3
	regionArr    byte = 4
	regionSymCol byte = 5
)

const symMaxLen = 8

// symStats counts training work, for the experiment harness.
var symStats struct {
	trains, trainNanos, trainBytes atomic.Int64
	// leaf encodes: leaves, their rows, those written with a symbol-table region, and the payload
	// bytes coded for writing (enc) and for measuring (len).
	leaves, rows, codedLeaves, encBytes, lenBytes atomic.Int64
}

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
	// trained is the payload bytes the table was trained from (0: read from a page, unknown). A table
	// with no symbols is kept to carry it; tabFor hides such a table.
	trained int
}

// symCand is one symbol as the matcher tests it: its bytes little-endian and a mask of its length.
type symCand struct {
	val, mask uint64
	k         uint16
	n         uint8
}

// symMatcher finds the longest symbol at a position. Symbols are at least two bytes, so they are
// grouped by their first two: an open-addressed table maps those two bytes to a run of cands, longest
// first. (A table of some two hundred symbols has many per first byte and few per pair.)
type symMatcher struct {
	keys  [symMatchSlots]uint16
	lo    [symMatchSlots]uint8
	hi    [symMatchSlots]uint8 // 0: an empty slot (a run ends past its start, so never at 0)
	cands [255]symCand
	// single[c] is one more than the index of the one-byte symbol c, 0 when there is none (only a
	// table trained FSST-style has them).
	single [256]uint16
	order  [255]symOrder // build scratch
}

const symMatchSlots = 1024

type symOrder struct {
	key uint16 // the first two bytes, little-endian
	n   uint8
	k   uint16
}

func symMatchHash(key uint16) uint32 { return (uint32(key) * 2654435761) >> 22 }

// build fills the matcher from symbols given as little-endian bytes and lengths.
func (m *symMatcher) build(le []uint64, n []uint8) {
	m.hi = [symMatchSlots]uint8{}
	m.single = [256]uint16{}
	order := m.order[:0]
	for k, v := range le {
		if n[k] == 1 {
			m.single[byte(v)] = uint16(k) + 1
			continue
		}
		order = append(order, symOrder{key: uint16(v), n: n[k], k: uint16(k)})
	}
	slices.SortFunc(order, func(a, b symOrder) int {
		if a.key != b.key {
			return int(a.key) - int(b.key)
		}
		if a.n != b.n {
			return int(b.n) - int(a.n)
		}
		return int(a.k) - int(b.k)
	})
	for i := 0; i < len(order); {
		j := i
		for j < len(order) && order[j].key == order[i].key {
			o := order[j]
			m.cands[j] = symCand{val: le[o.k], mask: ^uint64(0) >> (64 - 8*int(o.n)), k: o.k, n: o.n}
			j++
		}
		h := symMatchHash(order[i].key)
		for m.hi[h] != 0 {
			h = (h + 1) & (symMatchSlots - 1)
		}
		m.keys[h], m.lo[h], m.hi[h] = order[i].key, uint8(i), uint8(j)
		i = j
	}
}

func (tb *symTable) matcher() *symMatcher {
	tb.mOnce.Do(tb.index)
	return tb.m
}

func (tb *symTable) index() {
	m := &symMatcher{}
	le := make([]uint64, len(tb.syms))
	n := make([]uint8, len(tb.syms))
	for k, sym := range tb.syms {
		var v [8]byte
		copy(v[:], sym)
		le[k], n[k] = binary.LittleEndian.Uint64(v[:]), uint8(len(sym))
	}
	m.build(le, n)
	tb.m = m
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
	c := p[at]
	left := len(p) - at
	if left < 2 {
		return m.one(c), 1
	}
	key := uint16(c) | uint16(p[at+1])<<8
	for h := symMatchHash(key); m.hi[h] != 0; h = (h + 1) & (symMatchSlots - 1) {
		if m.keys[h] != key {
			continue
		}
		var w uint64
		if left >= 8 {
			w = binary.LittleEndian.Uint64(p[at:])
		} else {
			var v [8]byte
			copy(v[:], p[at:])
			w = binary.LittleEndian.Uint64(v[:])
		}
		for i := int(m.lo[h]); i < int(m.hi[h]); i++ {
			cd := &m.cands[i]
			if w&cd.mask == cd.val && int(cd.n) <= left {
				return 256 + int(cd.k), int(cd.n)
			}
		}
		break
	}
	return m.one(c), 1
}

// one is the unit for the single byte c: its one-byte symbol, else the literal.
func (m *symMatcher) one(c byte) int {
	if k := m.single[c]; k != 0 {
		return 255 + int(k)
	}
	return int(c)
}

func (tb *symTable) unitBytes(u int) []byte {
	if u < 256 {
		return []byte{byte(u)}
	}
	return tb.syms[u-256]
}

// encLen is the coded length of payload p.
func (tb *symTable) encLen(p []byte) int {
	symStats.lenBytes.Add(int64(len(p)))
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
	symStats.encBytes.Add(int64(len(p)))
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

// symTrain is the trainer's working memory, reused across trainings.
type symTrain struct {
	keys   [symPairSlots]uint32 // pair counts: an open-addressed table keyed by (unit, unit)
	counts [symPairSlots]int32
	used   []uint32
	c1     [256 + 255]int32
	cands  []symGain
	le     [255]uint64 // the current symbols, little-endian, and their lengths
	n      [255]uint8
	k      int
	m      symMatcher
	seen   [512]uint64 // the symbols chosen this pass (bytes and length), for duplicates
	seenN  [512]uint8
}

const symPairSlots = 1 << 16

// symGain is a candidate symbol: its bytes big-endian and left-aligned (so integer order is byte
// order), its length, and the bytes it would save.
type symGain struct {
	be   uint64
	gain int32
	n    uint8
}

var symTrainPool = sync.Pool{New: func() any { return &symTrain{} }}

// symPasses is the number of training passes.
var symPasses = func() int {
	if n, _ := strconv.Atoi(os.Getenv("JED_LEAF_SYM_PASSES")); n > 0 {
		return n
	}
	return 5
}()

// trainSym builds a table from a column's inline payloads, or nil when no symbol repays its entry.
// Each pass parses the payloads with the current table, counts symbols and adjacent pairs, and keeps
// the candidates (pair concatenations and existing symbols) that save the most bytes. Ties break by
// the symbol's bytes, so the table is a function of the payloads in order.
func trainSym(payloads [][]byte, classic bool) *symTable {
	start := time.Now()
	defer func() {
		symStats.trains.Add(1)
		symStats.trainNanos.Add(int64(time.Since(start)))
	}()
	var freq [256]int
	total := 0
	for _, p := range payloads {
		total += len(p)
		for _, c := range p {
			freq[c]++
		}
	}
	symStats.trainBytes.Add(int64(total))
	if total < 16 {
		return &symTable{trained: total}
	}
	// Code bytes: the values the payloads never use, rarest first; at least 16, taking the rarest
	// used values when fewer are free. The first is the escape.
	var order [256]uint8
	for i := range order {
		order[i] = uint8(i)
	}
	slices.SortStableFunc(order[:], func(a, b uint8) int { return freq[a] - freq[b] })
	ncodes := 0
	for ncodes < 256 && freq[order[ncodes]] == 0 {
		ncodes++
	}
	if ncodes < 16 {
		ncodes = 16
	}
	maxSyms := min(ncodes-1, 255)
	if classic {
		// FSST as published: every code may be a symbol, single bytes included, and a literal no
		// symbol covers is escaped. A candidate is ranked by the bytes it covers.
		maxSyms = 255
	}

	t := symTrainPool.Get().(*symTrain)
	defer symTrainPool.Put(t)
	t.k = 0
	unit := func(u uint32) (uint64, int) {
		if u < 256 {
			return uint64(u), 1
		}
		return t.le[u-256], int(t.n[u-256])
	}
	for gen := 0; gen < symPasses; gen++ {
		t.m.build(t.le[:t.k], t.n[:t.k])
		for i := range t.c1 {
			t.c1[i] = 0
		}
		for _, h := range t.used {
			t.keys[h], t.counts[h] = 0, 0
		}
		t.used = t.used[:0]
		full := false
		for _, p := range payloads {
			prev := -1
			for at := 0; at < len(p); {
				u, n := t.m.match(p, at)
				t.c1[u]++
				if prev >= 0 && !full {
					k := uint32(prev)<<16 | uint32(u) | 1<<31 // never 0
					h := (k * 2654435761) >> 16
					for t.keys[h] != 0 && t.keys[h] != k {
						h = (h + 1) & (symPairSlots - 1)
					}
					if t.keys[h] == 0 {
						t.keys[h] = k
						t.used = append(t.used, h)
						full = len(t.used) >= symPairSlots*3/4 // later pairs are not counted
					}
					t.counts[h]++
				}
				prev = u
				at += n
			}
		}
		t.cands = t.cands[:0]
		add := func(le uint64, n int, count int32) {
			if classic {
				if count > 0 {
					t.cands = append(t.cands, symGain{be: bits.ReverseBytes64(le), gain: count * int32(n), n: uint8(n)})
				}
				return
			}
			if n < 2 {
				return
			}
			if g := int(count)*(n-1) - (n + 2); g > 0 {
				t.cands = append(t.cands, symGain{be: bits.ReverseBytes64(le), gain: int32(g), n: uint8(n)})
			}
		}
		for k := 0; k < t.k; k++ {
			add(t.le[k], int(t.n[k]), t.c1[256+k])
		}
		if classic {
			for u := 0; u < 256; u++ {
				add(uint64(u), 1, t.c1[u])
			}
		}
		for _, h := range t.used {
			k := t.keys[h] &^ (1 << 31)
			a, an := unit(k >> 16)
			b, bn := unit(k & 0xffff)
			if an+bn > symMaxLen {
				continue
			}
			add(a|b<<(8*an), an+bn, t.counts[h])
		}
		// Largest gain first, ties by the symbol's bytes; a string seen twice keeps its larger gain.
		slices.SortFunc(t.cands, func(a, b symGain) int {
			if a.gain != b.gain {
				return int(b.gain) - int(a.gain)
			}
			if a.be != b.be {
				if a.be < b.be {
					return -1
				}
				return 1
			}
			return int(a.n) - int(b.n)
		})
		for i := range t.seenN {
			t.seenN[i] = 0
		}
		t.k = 0
		for _, c := range t.cands {
			if t.k == maxSyms {
				break
			}
			h := (c.be*0x9e3779b97f4a7c15 + uint64(c.n)) >> 55
			dup := false
			for t.seenN[h] != 0 {
				if t.seen[h] == c.be && t.seenN[h] == c.n {
					dup = true
					break
				}
				h = (h + 1) & 511
			}
			if dup {
				continue
			}
			t.seen[h], t.seenN[h] = c.be, c.n
			t.le[t.k], t.n[t.k] = bits.ReverseBytes64(c.be), c.n
			t.k++
		}
	}
	if t.k == 0 {
		return &symTable{trained: total}
	}
	tb := &symTable{trained: total}
	buf := make([]byte, 0, t.k*symMaxLen)
	codes := make([]byte, t.k)
	for k := 0; k < t.k; k++ {
		var v [8]byte
		binary.LittleEndian.PutUint64(v[:], t.le[k])
		at := len(buf)
		buf = append(buf, v[:t.n[k]]...)
		tb.syms = append(tb.syms, buf[at:len(buf):len(buf)])
		codes[k] = order[1+k]
	}
	tb.seal(order[0], codes)
	return tb
}

// symEligible reports whether a column may be written as a symbol-table region: a variable-width
// column whose inline-plain value is a header then its content payload. Integer arrays take the
// typed encoding instead.
func symEligible(ty colType) bool {
	if !(leafSym || leafSymCol) || !isSpillable(ty) || arrElemWidth(ty) != 0 {
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

// symGainMax bounds what coding an inline value of n payload bytes can save: a code stands for at
// most symMaxLen bytes, so the value keeps at least ceil(n/8) bytes.
func symGainMax(header, n int) int { return header + n - (n+symMaxLen-1)/symMaxLen }

// symCanBeat reports whether a symbol-table region could save more than other bytes, given the bound
// over its inline values: the leaf's raw bitmap and, for a per-leaf table, the table come off first.
// Coding a region is most of the cost of measuring or writing a leaf, so it is skipped when the
// dictionary is already known to win (short repeated strings).
func symCanBeat(n, bound, other int, tb *symTable, colTab bool) bool {
	bound -= (n + 7) / 8
	if !colTab {
		bound -= tb.size
	}
	return bound > other
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
// column's per-leaf symbol table, nil when the leaf is written without one; coded says whether the
// leaf is written with its per-column tables (it does not fit as plain regions).
func itemSaving(shape leafShape, j, n int, f colFacts, tb *symTable, coded bool) int {
	if w := shape.itemArr[j]; w != 0 {
		return arrSaving(n, w, 2, f.arr)
	}
	s := 0
	if shape.itemDict[j] {
		s = dictSaving(n, f.d, f.dup)
	}
	if coded && shape.itemTab[j] != nil {
		// A per-column table: no table bytes in the leaf.
		if t := f.gain - (n+7)/8; t > s {
			s = t
		}
		return s
	}
	if t := symSaving(n, f.gain, tb); t > s {
		s = t
	}
	return s
}

// symRetrainPct is how much a column's payload must have grown since its table was trained, in
// percent, before an overflowing leaf trains it again (JED_LEAF_SYM_RETRAIN). The default never
// does: a table lasts the leaf's life, which measured no larger than retraining at every overflow
// (leaf-structural.md §10). A table read from a page has no recorded size and is trained again.
var symRetrainPct = func() int {
	switch v := os.Getenv("JED_LEAF_SYM_RETRAIN"); v {
	case "", "never":
		return 1 << 30
	default:
		n, _ := strconv.Atoi(v)
		return n
	}
}()

// trainTabs trains tables for the symbol-table columns from the rows' items (items[i][j] is row i's
// item for shape.itemCols[j]). With no prior tables every column is trained. With prior tables (an
// overflowing leaf's) a column is trained again only when its payload has grown by symRetrainPct
// since its table was trained, and keeps its table otherwise; changed reports whether any was. The
// result is indexed by column; nil when the shape has no such column.
func trainTabs(shape leafShape, items [][]dictItem, prior []*symTable) (tabs []*symTable, changed bool) {
	if !shape.hasSym {
		return nil, false
	}
	tabs = make([]*symTable, len(shape.colTypes))
	payloads := make([][]byte, 0, len(items))
	for j, c := range shape.itemCols {
		if !shape.itemSym[j] {
			continue
		}
		payloads = payloads[:0]
		total := 0
		for i := range items {
			if it := items[i][j]; it.kind == dictShared {
				payloads = append(payloads, it.payload)
				total += len(it.payload)
			}
		}
		if prior != nil && prior[c] != nil {
			if t := prior[c].trained; t != 0 && total*100 < t*(100+symRetrainPct) {
				tabs[c] = prior[c]
				continue
			}
		}
		tabs[c] = trainSym(payloads, false)
		changed = true
	}
	return tabs, changed
}

// tabFor is column c's table, or nil when the leaf has none or training found no symbol.
func tabFor(tabs []*symTable, c int) *symTable {
	if tabs == nil || tabs[c] == nil || len(tabs[c].syms) == 0 {
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
	// col marks a region coded with its column's table (regionSymCol): tab is that table and the
	// region holds none.
	col bool
	mat atomic.Pointer[plainRegion]
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
	b := r.raw[r.tabSize:]
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
		if lz := l.regions[c].lazy; lz != nil && lz.kind == regionSym && !lz.col {
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

// bodyErr is an inline-deferred value's body: Comp, or for a value of a lazy region its content
// payload (a string's) or its body after the tag (any other type), from the rebuilt region.
func (u *Unfetched) bodyErr() ([]byte, error) {
	if u.lazy == nil {
		return u.Comp, nil
	}
	m := u.lazy.plain()
	if m.err != nil {
		return nil, m.err
	}
	lo := uint32(0)
	if u.lazyIdx > 0 {
		lo = paxDirEnd(m.ends, u.lazyIdx-1, 4)
	}
	hi := paxDirEnd(m.ends, u.lazyIdx, 4)
	hdr := uint32(1)
	if stringPayload(u.lazy.ty) && !leafBareString {
		hdr = 3
	}
	if lo+hdr > hi || int(hi) > len(m.body) {
		return nil, newError(DataCorrupted, "PAX leaf value offset out of range")
	}
	return m.body[lo+hdr : hi], nil
}

// body is bodyErr for the callers that only measure or copy the bytes: a malformed region yields no
// bytes here and its error when the value is resolved.
func (u *Unfetched) body() []byte {
	b, _ := u.bodyErr()
	return b
}

// usable is t when it has symbols, else nil.
func (tb *symTable) usable() *symTable {
	if tb == nil || len(tb.syms) == 0 {
		return nil
	}
	return tb
}

// maybeTrainColumns trains this table's per-column symbol tables once, when its tree first has
// symColLeaves leaves or a third level: from its rows in key order, up to symColSample bytes of
// payload over the eligible columns. The store gets new column types carrying the tables; leaves
// written from then on that do not fit as plain regions are coded with them, and leaves already written stay as they are. A column
// whose training finds no symbol gets an empty table, so training is not repeated.
func (s *tableStore) maybeTrainColumns() error {
	if !leafSymCol || s.symTrained || !s.shape.hasSymCol {
		return nil
	}
	root := s.rows.root
	if root == nil || root.isLeaf() {
		return nil
	}
	if len(root.keys)+1 < symColLeaves && (root.children[0].node == nil || root.children[0].node.isLeaf()) {
		return nil
	}
	payloads := make([][][]byte, len(s.shape.itemCols))
	items := make([]dictItem, len(s.shape.itemCols))
	total := 0
	err := s.rows.scanRange(unboundedBound(), s.leafSrc(), func(key []byte, row storedRow) (bool, error) {
		rowItems(s.shape, s.cap, key, row, items)
		for j := range s.shape.itemCols {
			if s.shape.itemSym[j] && items[j].kind == dictShared {
				// A deferred value's bytes are a view of its page; the trainer only reads them.
				payloads[j] = append(payloads[j], items[j].payload)
				total += len(items[j].payload)
			}
		}
		return total < symColSample, nil
	})
	if err != nil {
		return err
	}
	colTypes := append([]colType(nil), s.colTypes...)
	for j, c := range s.shape.itemCols {
		if s.shape.itemSym[j] {
			colTypes[c].sym = trainColumnTable(payloads[j])
			if os.Getenv("JED_LEAF_SYM_LOG") != "" {
				tb, singles, bytes := colTypes[c].sym, 0, 0
				for _, sy := range tb.syms {
					if len(sy) == 1 {
						singles++
					}
				}
				for _, p := range payloads[j] {
					bytes += len(p)
				}
				println("symbol table: column", c, "of", len(colTypes), "sample bytes", bytes, "symbols", len(tb.syms), "one-byte", singles)
			}
		}
	}
	s.setColTypes(colTypes)
	return nil
}

// setColTypes replaces the store's column types (to attach per-column symbol tables) and rederives
// its leaf shape.
func (s *tableStore) setColTypes(colTypes []colType) {
	keyLen := s.shape.keyLen
	s.colTypes = colTypes
	s.shape = leafShapeFor(colTypes)
	s.shape.keyLen = keyLen
	s.symTrained = false
	for _, ty := range colTypes {
		if ty.sym != nil {
			s.symTrained = true
		}
	}
}

// appendColTables appends a table's per-column symbol tables (leafSymCol), the payload of its
// symbol-table chain: a u16 count, then per column with a table its u16 ordinal and the table
// (appendTable; an empty table is two bytes).
func appendColTables(out []byte, colTypes []colType) []byte {
	n := 0
	for _, ty := range colTypes {
		if ty.sym != nil {
			n++
		}
	}
	out = appendU16(out, uint16(n))
	for c, ty := range colTypes {
		if ty.sym == nil {
			continue
		}
		out = appendU16(out, uint16(c))
		if len(ty.sym.syms) == 0 {
			out = append(out, 0, 0)
		} else {
			out = ty.sym.appendTable(out)
		}
	}
	return out
}

// readColTables reads what appendColTables wrote, into a slice by column ordinal (nil: none).
func readColTables(buf []byte, pos *int, ncols int) ([]*symTable, error) {
	n, err := readU16(buf, pos)
	if err != nil {
		return nil, err
	}
	var tabs []*symTable
	for i := 0; i < int(n); i++ {
		c, err := readU16(buf, pos)
		if err != nil {
			return nil, err
		}
		if int(c) >= ncols {
			return nil, newError(DataCorrupted, "symbol table for a column the table does not have")
		}
		tb, rest, err := parseSymTable(buf[*pos:])
		if err != nil {
			return nil, err
		}
		*pos = len(buf) - len(rest)
		if tabs == nil {
			tabs = make([]*symTable, ncols)
		}
		tabs[c] = tb
	}
	return tabs, nil
}

// symChainRef names the overflow chain that holds a table's per-column symbol tables (leafSymCol): its
// first page and its payload length. The payload is appendColTables. The zero value is no chain.
type symChainRef struct {
	first  pageRef
	length uint32
}

// pages is the chain's page count.
func (r symChainRef) pages(pcap pageCap) uint64 {
	if r.first.page == 0 {
		return 0
	}
	slab := uint64(pcap.chain())
	return (uint64(r.length) + slab - 1) / slab
}

func (s *tableStore) symRef() symChainRef {
	return symChainRef{first: s.symChain, length: s.symChainLen}
}

// symRefs is each table's chain reference, in keys order.
func (s *snapshot) symRefs(keys []string) []symChainRef {
	refs := make([]symChainRef, len(keys))
	for i, k := range keys {
		refs[i] = s.stores[k].symRef()
	}
	return refs
}

// markSymChain adds a store's symbol-table chain pages to reached.
func markSymChain(st *tableStore, read func(pageRef) ([]byte, error), pcap pageCap, reached map[uint32]bool) error {
	if st.symChain.page == 0 {
		return nil
	}
	pages, err := chainPages(st.symChain, int(st.symChainLen), read, pcap)
	if err != nil {
		return err
	}
	for _, p := range pages {
		reached[p] = true
	}
	return nil
}

// loadSymChain reads a table's symbol tables from its chain and attaches them to the store.
func (s *tableStore) loadSymChain(ref symChainRef, read func(pageRef) ([]byte, error), pcap pageCap) error {
	var visited []uint32
	payload, err := readOverflowChain(ref.first, int(ref.length), read, pcap, &visited)
	if err != nil {
		return err
	}
	pos := 0
	tabs, err := readColTables(payload, &pos, len(s.colTypes))
	if err != nil {
		return err
	}
	if pos != len(payload) || tabs == nil {
		return newError(DataCorrupted, "symbol-table chain is malformed")
	}
	withTabs := append([]colType(nil), s.colTypes...)
	for c, tb := range tabs {
		withTabs[c].sym = tb
	}
	s.setColTypes(withTabs)
	s.symChain, s.symChainLen = ref.first, ref.length
	return nil
}

// leafOrColTab is the table item column j (column c) is measured under in a leaf written coded: its
// per-leaf table, else its per-column one.
func leafOrColTab(shape leafShape, tabs []*symTable, j, c int) *symTable {
	if tb := tabFor(tabs, c); tb != nil {
		return tb
	}
	return shape.itemTab[j]
}

// trainColumnTable trains a per-column table both ways — literals standing for themselves (trainSym)
// and FSST-style — and keeps the one that codes the sample smaller (the first on a tie). Binary
// values such as jsonb use most byte values, which leaves the first kind few codes; text does better
// with it. The page format is the same either way: an FSST-style table is one whose codes take
// every byte value, with one-byte symbols among them.
func trainColumnTable(payloads [][]byte) *symTable {
	plain := trainSym(payloads, false)
	fsst := trainSym(payloads, true)
	size := func(tb *symTable) int {
		total := 0
		if tb.usable() == nil {
			for _, p := range payloads {
				total += len(p)
			}
			return total
		}
		for _, p := range payloads {
			total += tb.encLen(p)
		}
		return total
	}
	if size(fsst) < size(plain) {
		return fsst
	}
	return plain
}
