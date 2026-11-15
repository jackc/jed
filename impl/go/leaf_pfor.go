package jed

// Format v54 (spec/fileformat/format.md "Leaf node", spec/design/leaf-write-cost.md §8–§9): a
// base-plus-offsets region with exceptions. A fixed column eligible for base plus offsets may take
// region kind 6:
//
//	null bitmap | base (w) | offset width bw (u8) | E (u8) | E exception rows (u16, ascending)
//	| E exception bodies (w) | N offsets (bw)
//
// A layout is a width bw narrower than kind 1's and a base among the present values; its
// exceptions are the present values outside [base, base + 2^(8·bw) − 1], at most 16, each stored
// whole with offset zero. The region takes the layout of least size (E + 1)·(w + 2) + N·bw, the
// narrower width and then the lower base on a tie, when it saves more than kind 1. The exceptions
// are some of the 16 smallest and 16 largest present values, so a column's size facts are its 17
// smallest and 17 largest (pforAcc). A reader reads it in place: row i is its exception's body
// when i is an exception row (a bucket mask rejects most rows before the search), else the base plus
// offset i.

import (
	"encoding/binary"
	"sync/atomic"
)

// regionPFOR is the region kind byte of a base-plus-offsets region with exceptions.
const regionPFOR byte = 6

// pforMaxK is the most values a kind 6 region stores whole.
const pforMaxK = 16

// leafPFOR is the exception cap this process writes with: pforMaxK, as format v54 requires. Tests
// lower it to measure other caps, or set 0 to compare a workload with the regions kind 1 alone gives.
var leafPFOR = pforMaxK

// pforWritten counts kind 6 regions written, and pforExcWritten their exceptions (tests).
var pforWritten, pforExcWritten atomic.Int64

// pforAcc is a column's kind 6 size facts: its k+1 smallest and k+1 largest present values.
type pforAcc struct {
	k  int      // the cap
	lo []uint64 // ascending: the k+1 smallest present values
	hi []uint64 // ascending: the k+1 largest
	np int      // present values
	// The least exception count for each offset width 0, 1, 2, 4 and the lowest base giving it, a
	// function of lo and hi alone (not of the row count), so a saving query is four products.
	cached bool
	ce     [4]int
	cb     [4]uint64
	cok    [4]bool
	// full says lo and hi each hold k+1 values and enough values lie between them that a value in
	// [skipLo, skipHi] = [lo[k], hi[0]] changes nothing; buf backs lo and hi.
	full           bool
	skipLo, skipHi uint64
	buf            [2 * (pforMaxK + 1)]uint64
}

func newPforAcc() *pforAcc {
	if leafPFOR == 0 {
		return nil
	}
	a := &pforAcc{k: min(leafPFOR, pforMaxK)}
	a.point()
	return a
}

// point makes lo and hi views of buf.
func (a *pforAcc) point() {
	k1 := a.k + 1
	a.lo, a.hi = a.buf[:len(a.lo):k1], a.buf[k1:k1+len(a.hi):2*k1]
}

func (a *pforAcc) clone() *pforAcc {
	if a == nil {
		return nil
	}
	out := *a
	out.point()
	return &out
}

func (a *pforAcc) add(v uint64) {
	a.np++
	if a.full && v >= a.skipLo && v <= a.skipHi {
		return // neither among the k+1 smallest nor the k+1 largest
	}
	a.addSlow(v)
}

func (a *pforAcc) addSlow(v uint64) {
	k1 := a.k + 1
	if a.np <= 2*k1+1 {
		a.cached = false // whether more values lie beyond hi can still change
	} else {
		a.full = true
	}
	if len(a.lo) < k1 || v < a.lo[k1-1] {
		i := len(a.lo)
		if i < k1 {
			a.lo = append(a.lo, v)
		} else {
			i = k1 - 1 // the largest kept value drops out
		}
		for i > 0 && a.lo[i-1] > v {
			a.lo[i] = a.lo[i-1]
			i--
		}
		a.lo[i] = v
		a.cached = false
	}
	if len(a.hi) < k1 {
		a.hi = append(a.hi, v)
		i := len(a.hi) - 1
		for i > 0 && a.hi[i-1] > v {
			a.hi[i] = a.hi[i-1]
			i--
		}
		a.hi[i] = v
		a.cached = false
	} else if v > a.hi[0] {
		i := 0 // the smallest kept value drops out
		for i+1 < k1 && a.hi[i+1] < v {
			a.hi[i] = a.hi[i+1]
			i++
		}
		a.hi[i] = v
		a.cached = false
	}
	if a.full {
		a.skipLo, a.skipHi = a.lo[k1-1], a.hi[0]
	}
}

// removed reports whether removing present value v may change the facts, so the column must be
// measured again: while few values are present, or when v is one of the kept extremes.
func (a *pforAcc) removed(v uint64) bool {
	if a.np <= 2*(a.k+1) {
		return true
	}
	return v <= a.lo[len(a.lo)-1] || v >= a.hi[0]
}

// pforPlan is a kind 6 layout: offsets of width bw from base, e exceptions.
type pforPlan struct {
	bw   int
	base uint64
	e    int
	ok   bool
}

// pforWidths are the offset widths a layout may take, indexed as pforAcc's cache.
var pforWidths = [4]int{0, 1, 2, 4}

// plan is the smallest kind 6 layout of a w-byte column of n rows: a width narrower than kind 1's,
// the narrower width and then the lower base on a tie.
func (a *pforAcc) plan(n, w int) (best pforPlan, size int) {
	if a == nil || a.np == 0 {
		return pforPlan{}, 0
	}
	if !a.cached {
		a.measure()
	}
	k1bw := forWidth(w, a.lo[0], a.hi[len(a.hi)-1])
	for x, bw := range pforWidths {
		if bw >= k1bw {
			break
		}
		if !a.cok[x] {
			continue
		}
		s := (a.ce[x]+1)*(w+2) + n*bw
		if !best.ok || s < size {
			best, size = pforPlan{bw: bw, base: a.cb[x], e: a.ce[x], ok: true}, s
		}
	}
	return best, size
}

// measure fills the per-width cache: for each width, the least exception count over bases (at
// most k) and the lowest base giving it. A base is one of the k+1 smallest values (more below it
// would be more than k exceptions); the values above its window are counted from hi, whose
// boundary moves only up as the base rises.
func (a *pforAcc) measure() {
	a.cached = true
	for x, bw := range pforWidths {
		a.cok[x] = false
		r := uint64(0)
		if bw > 0 {
			r = uint64(1)<<(8*bw) - 1
		}
		idx := 0 // hi[idx:] are the kept values above the window
		for j := 0; j < len(a.lo); j++ {
			if j > 0 && a.lo[j] == a.lo[j-1] {
				continue
			}
			base := a.lo[j]
			top := base + r
			if top < base {
				top = ^uint64(0)
			}
			for idx < len(a.hi) && a.hi[idx] <= top {
				idx++
			}
			if idx == 0 && a.np > len(a.hi) {
				continue // more than k above
			}
			e := j + len(a.hi) - idx
			if e > a.k {
				continue
			}
			if !a.cok[x] || e < a.ce[x] {
				a.ce[x], a.cb[x], a.cok[x] = e, base, true
			}
		}
	}
}

// saving is the bytes a kind 6 region saves over the plain one (0 when it does not).
func (a *pforAcc) saving(n, w int) int {
	p, size := a.plan(n, w)
	if !p.ok {
		return 0
	}
	if s := n*w - size; s > 0 {
		return s
	}
	return 0
}

// forRemoved reports whether removing present value s from the values r spans may change what r
// records, so r must be measured again.
func (r *forRange) forRemoved(s uint64) bool {
	if r.pf != nil && r.pf.removed(s) {
		return true
	}
	return s == r.lo || s == r.hi || (leafBlocksExc && (s == r.lo2 || s == r.hi2))
}

// encodePFOR writes a kind 6 region of n values (encodeRegion's vb and nulls) under plan p.
func encodePFOR(w, n int, vb [][]byte, nulls []bool, p pforPlan) []byte {
	pforWritten.Add(1)
	r := uint64(0)
	if p.bw > 0 {
		r = uint64(1)<<(8*p.bw) - 1
	}
	exc := make([]int, 0, p.e)
	for i := 0; i < n; i++ {
		if !nulls[i] {
			if v := beUint(vb[i]); v < p.base || v-p.base > r {
				exc = append(exc, i)
			}
		}
	}
	if len(exc) != p.e {
		panic("BUG: a kind 6 plan's exception count differs from the encoded one")
	}
	pforExcWritten.Add(int64(len(exc)))
	out := make([]byte, 1, 1+(n+7)/8+(len(exc)+1)*(w+2)+n*p.bw)
	out[0] = regionPFOR
	bm := len(out)
	out = append(out, make([]byte, (n+7)/8)...)
	for i, isNull := range nulls {
		if isNull {
			out[bm+i/8] |= 0x80 >> (i % 8)
		}
	}
	out = append(out, make([]byte, w)...)
	putBEUint(out[len(out)-w:], p.base)
	out = append(out, byte(p.bw), byte(len(exc)))
	for _, i := range exc {
		out = append(out, byte(i>>8), byte(i))
	}
	for _, i := range exc {
		out = append(out, vb[i]...)
	}
	at := len(out)
	out = append(out, make([]byte, n*p.bw)...)
	k := 0
	for i := 0; i < n; i++ {
		if k < len(exc) && exc[k] == i {
			k++
			continue
		}
		if !nulls[i] {
			putBEUint(out[at+i*p.bw:at+(i+1)*p.bw], beUint(vb[i])-p.base)
		}
	}
	return out
}

// parsePFOR validates a kind 6 region and returns it as views of the page: payload[p:end] follows
// the kind byte. Nothing is rebuilt but the bucket mask.
func parsePFOR(payload []byte, p, end, n, w int) (paxRegion, error) {
	bad := newError(DataCorrupted, "PAX leaf narrow region with exceptions is malformed")
	bm := p
	p += (n + 7) / 8
	if p+w+2 > end {
		return paxRegion{}, bad
	}
	bitmap := payload[bm:p]
	base := beUint(payload[p : p+w])
	bw := int(payload[p+w])
	e := int(payload[p+w+1])
	p += w + 2
	if (bw != 0 && bw != 1 && bw != 2 && bw != 4) || bw >= w || e < 1 || e > pforMaxK || p+e*(w+2)+n*bw != end {
		return paxRegion{}, bad
	}
	rows := payload[p : p+2*e]
	bodies := payload[p+2*e : p+e*(w+2)]
	offs := payload[p+e*(w+2) : end]
	shift := 0
	for (n-1)>>shift >= 64 {
		shift++
	}
	var mask uint64
	prev := -1
	for k := 0; k < e; k++ {
		i := int(binary.BigEndian.Uint16(rows[2*k:]))
		if i <= prev || i >= n || bitmap[i/8]&(0x80>>(i%8)) != 0 {
			return paxRegion{}, bad
		}
		for _, b := range offs[i*bw : (i+1)*bw] {
			if b != 0 {
				return paxRegion{}, bad
			}
		}
		prev = i
		mask |= 1 << (i >> shift)
	}
	return paxRegion{
		width: w, bitmap: bitmap, isFOR: true, forBW: bw, forBase: base, forOff: offs,
		pfRows: rows, pfBodies: bodies, pfMask: mask, pfShift: uint8(shift),
	}, nil
}

// pfExc is row i's exception body in a kind 6 region, nil when row i is not one.
func (r *paxRegion) pfExc(i int) []byte {
	rows := r.pfRows
	lo, hi := 0, len(rows)/2
	for lo < hi {
		m := int(uint(lo+hi) >> 1)
		if int(binary.BigEndian.Uint16(rows[2*m:])) < i {
			lo = m + 1
		} else {
			hi = m
		}
	}
	if lo < len(rows)/2 && int(binary.BigEndian.Uint16(rows[2*lo:])) == i {
		return r.pfBodies[lo*r.width : (lo+1)*r.width]
	}
	return nil
}
