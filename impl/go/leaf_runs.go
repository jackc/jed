package jed

// PROTOTYPE (spec/design/leaf-write-cost.md §4, option d): a base-plus-offsets column split into
// runs, each with its own base and offset width. With JED_LEAF_RUNS=search or rank, a fixed column
// eligible for base plus offsets may also take region kind 7:
//
//	null bitmap | run count R (u16) | [rank: run-start bitmap ⌈N/8⌉, then ⌈N/64⌉ u16 counts]
//	| R × (start row u16, data offset u16, base w, width u8) | the runs' offsets, in row order
//
// Run r covers rows [start[r], start[r+1]); record i of it is base + offset, the offset `width`
// bytes at data offset + (i − start)·width. A NULL's offset is zero. A run's width is one of 0, 1,
// 2, 4 or 8 bytes below w, or w itself (raw values). The index lets a reader find row i's run
// without decoding the region: "search" binary-searches the run starts; "rank" adds a bit per row
// (set where a run starts) and, per 64 rows, the number of runs started before them, so row i's run
// is a count plus one popcount.
//
// The runs are the cheapest division of the rows, found by dynamic programming over the rows in
// order (runsAcc); among equal costs the narrowest last run, then the earliest start. Its size is
//
//	F + Σ_runs (w + 5 + len·width)        F = 2, plus ⌈N/8⌉ + 2⌈N/64⌉ with the rank index
//
// and the region takes kind 7 only when that is smaller than every other kind's. Off by default.

import (
	"math/bits"
	"os"
	"sync/atomic"
)

// runsWritten counts kind 7 regions written (tests).
var runsWritten atomic.Int64

const (
	leafRunsOff = iota
	leafRunsSearch
	leafRunsRank
)

var leafRuns = func() int {
	switch os.Getenv("JED_LEAF_RUNS") {
	case "search":
		return leafRunsSearch
	case "rank":
		return leafRunsRank
	}
	return leafRunsOff
}()

// regionRuns is the region kind byte of a base-plus-offsets region in runs.
const regionRuns byte = 7

// runsFixed is F, the bytes a kind 7 region of n rows spends before its run headers.
func runsFixed(n int) int {
	f := 2
	if leafRuns == leafRunsRank {
		f += (n+7)/8 + 2*((n+63)/64)
	}
	return f
}

// runsWidths is the offset widths a run of a w-byte column may take, ascending.
func runsWidths(w int) []int {
	ws := make([]int, 0, 5)
	for _, b := range []int{0, 1, 2, 4, 8} {
		if b < w {
			ws = append(ws, b)
		}
	}
	return append(ws, w)
}

// deque is a slice-backed double-ended queue of row indexes.
type deque struct {
	q    []int
	head int
}

func (d *deque) empty() bool  { return d.head == len(d.q) }
func (d *deque) front() int   { return d.q[d.head] }
func (d *deque) back() int    { return d.q[len(d.q)-1] }
func (d *deque) popFront()    { d.head++ }
func (d *deque) popBack()     { d.q = d.q[:len(d.q)-1] }
func (d *deque) push(i int)   { d.q = append(d.q, i) }
func (d *deque) clone() deque { return deque{q: append([]int(nil), d.q[d.head:]...)} }

// runsAcc is the cheapest division into runs of the rows pushed so far, kept as rows arrive: dp[j]
// is the cheapest run bytes for the first j rows. For each width, the runs that may end at the next
// row start at or after lo (the values from lo on fit the width), and cand holds those starts in
// increasing order of dp[s] − s·width, so the cheapest is at the front.
type runsAcc struct {
	w        int
	ws       []int
	vals     []uint64
	present  []bool
	dp       []int
	choiceS  []int32
	choiceB  []uint8
	lo       []int
	minQ     []deque
	maxQ     []deque
	cand     []deque
	presentN int
}

func newRunsAcc(w int) *runsAcc {
	ws := runsWidths(w)
	return &runsAcc{w: w, ws: ws, dp: []int{0}, choiceS: []int32{0}, choiceB: []uint8{0},
		lo: make([]int, len(ws)), minQ: make([]deque, len(ws)), maxQ: make([]deque, len(ws)), cand: make([]deque, len(ws))}
}

func (a *runsAcc) clone() *runsAcc {
	if a == nil {
		return nil
	}
	out := *a
	out.vals = append([]uint64(nil), a.vals...)
	out.present = append([]bool(nil), a.present...)
	out.dp = append([]int(nil), a.dp...)
	out.choiceS = append([]int32(nil), a.choiceS...)
	out.choiceB = append([]uint8(nil), a.choiceB...)
	out.lo = append([]int(nil), a.lo...)
	out.minQ, out.maxQ, out.cand = make([]deque, len(a.ws)), make([]deque, len(a.ws)), make([]deque, len(a.ws))
	for k := range a.ws {
		out.minQ[k], out.maxQ[k], out.cand[k] = a.minQ[k].clone(), a.maxQ[k].clone(), a.cand[k].clone()
	}
	return &out
}

// push appends one row: its value, or a NULL.
func (a *runsAcc) push(v uint64, present bool) {
	j := len(a.vals)
	a.vals = append(a.vals, v)
	a.present = append(a.present, present)
	if present {
		a.presentN++
	}
	hdr := a.w + 5
	best, bestS, bestB := -1, 0, 0
	for k, b := range a.ws {
		// A run may start at row j.
		key := func(s int) int { return a.dp[s] - s*b }
		c := &a.cand[k]
		for !c.empty() && key(c.back()) > key(j) {
			c.popBack()
		}
		c.push(j)
		if b < a.w {
			if present {
				mn, mx := &a.minQ[k], &a.maxQ[k]
				for !mn.empty() && a.vals[mn.back()] >= v {
					mn.popBack()
				}
				mn.push(j)
				for !mx.empty() && a.vals[mx.back()] <= v {
					mx.popBack()
				}
				mx.push(j)
				limit := uint64(1)<<(8*b) - 1
				if b == 0 {
					limit = 0
				}
				for a.vals[mx.front()]-a.vals[mn.front()] > limit {
					a.lo[k]++
					for mn.front() < a.lo[k] {
						mn.popFront()
					}
					for mx.front() < a.lo[k] {
						mx.popFront()
					}
				}
				for c.front() < a.lo[k] {
					c.popFront()
				}
			}
		}
		s := c.front()
		cost := key(s) + (j+1)*b + hdr
		if best < 0 || cost < best {
			best, bestS, bestB = cost, s, b
		}
	}
	a.dp = append(a.dp, best)
	a.choiceS = append(a.choiceS, int32(bestS))
	a.choiceB = append(a.choiceB, uint8(bestB))
}

// size is the kind 7 region's bytes after the kind byte and the null bitmap.
func (a *runsAcc) size() int {
	n := len(a.vals)
	return runsFixed(n) + a.dp[n]
}

// saving is the bytes kind 7 saves over the plain region (0 when it does not, or is off).
func (a *runsAcc) saving() int {
	if a == nil || leafRuns == leafRunsOff {
		return 0
	}
	if s := len(a.vals)*a.w - a.size(); s > 0 {
		return s
	}
	return 0
}

// runs returns the division: each run's first row and width, in row order.
func (a *runsAcc) runs() (starts []int, widths []int) {
	for j := len(a.vals); j > 0; {
		starts = append(starts, int(a.choiceS[j]))
		widths = append(widths, int(a.choiceB[j]))
		j = int(a.choiceS[j])
	}
	for l, r := 0, len(starts)-1; l < r; l, r = l+1, r-1 {
		starts[l], starts[r] = starts[r], starts[l]
		widths[l], widths[r] = widths[r], widths[l]
	}
	return starts, widths
}

// newRunsRange is a forRange that also keeps runs, for a column of width w, in runs mode.
func newRunsRange(w int) forRange {
	var r forRange
	if leafRuns == leafRunsOff && leafBlocks == 0 {
		return r
	}
	r.pos = &forPos{}
	if leafRuns != leafRunsOff {
		r.pos.rd = newRunsAcc(w)
	}
	if leafBlocks > 0 {
		r.pos.bk = newBlocksAcc(w) // PROTOTYPE (leaf_blocks.go)
	}
	return r
}

// encodeRuns writes a kind 7 region of n values (encodeRegion's vb and nulls) from their runs.
func encodeRuns(w, n int, vb [][]byte, nulls []bool, a *runsAcc) []byte {
	runsWritten.Add(1)
	starts, widths := a.runs()
	r := len(starts)
	out := make([]byte, 1, 1+(n+7)/8+a.size())
	out[0] = regionRuns
	bm := len(out)
	out = append(out, make([]byte, (n+7)/8)...)
	for i, isNull := range nulls {
		if isNull {
			out[bm+i/8] |= 0x80 >> (i % 8)
		}
	}
	out = append(out, byte(r>>8), byte(r))
	if leafRuns == leafRunsRank {
		sb := len(out)
		out = append(out, make([]byte, (n+7)/8)...)
		for _, s := range starts {
			out[sb+s/8] |= 0x80 >> (s % 8)
		}
		count := 0
		for k := 0; k < (n+63)/64; k++ {
			for count < r && starts[count] < 64*k {
				count++
			}
			out = append(out, byte(count>>8), byte(count))
		}
	}
	hdrAt := len(out)
	out = append(out, make([]byte, r*(w+5))...)
	off := 0
	for k := 0; k < r; k++ {
		lo, hi := starts[k], n
		if k+1 < r {
			hi = starts[k+1]
		}
		base, any := uint64(0), false
		for i := lo; i < hi; i++ {
			if !nulls[i] {
				if v := beUint(vb[i]); !any || v < base {
					base, any = v, true
				}
			}
		}
		h := out[hdrAt+k*(w+5):]
		h[0], h[1] = byte(lo>>8), byte(lo)
		h[2], h[3] = byte(off>>8), byte(off)
		putBEUint(h[4:4+w], base)
		h[4+w] = byte(widths[k])
		b := widths[k]
		at := len(out)
		out = append(out, make([]byte, (hi-lo)*b)...)
		for i := lo; i < hi; i++ {
			if !nulls[i] && b > 0 {
				putBEUint(out[at+(i-lo)*b:at+(i-lo+1)*b], beUint(vb[i])-base)
			}
		}
		off += (hi - lo) * b
	}
	return out
}

// runsRegion is a kind 7 region's parts, as a reader that reads values in place uses them.
type runsRegion struct {
	n, w, r  int
	bitmap   []byte
	starts   []byte // rank only
	counts   []byte // rank only
	hdr      []byte
	data     []byte
	rankMode bool
}

// parseRunsRegion validates a kind 7 region (payload[p:end] follows the kind byte).
func parseRunsRegion(payload []byte, p, end, n, w int) (runsRegion, error) {
	bad := newError(DataCorrupted, "PAX leaf region in runs is malformed")
	rr := runsRegion{n: n, w: w, rankMode: leafRuns == leafRunsRank}
	if p+(n+7)/8+2 > end {
		return rr, bad
	}
	rr.bitmap = payload[p : p+(n+7)/8]
	p += (n + 7) / 8
	rr.r = int(payload[p])<<8 | int(payload[p+1])
	p += 2
	if rr.rankMode {
		sl, cl := (n+7)/8, 2*((n+63)/64)
		if p+sl+cl > end {
			return rr, bad
		}
		rr.starts, rr.counts = payload[p:p+sl], payload[p+sl:p+sl+cl]
		p += sl + cl
	}
	if rr.r < 1 || rr.r > n || p+rr.r*(w+5) > end {
		return rr, bad
	}
	rr.hdr = payload[p : p+rr.r*(w+5)]
	p += rr.r * (w + 5)
	rr.data = payload[p:end]
	off := 0
	for k := 0; k < rr.r; k++ {
		s, o, b := rr.run(k)
		hi := n
		if k+1 < rr.r {
			hi, _, _ = rr.run(k + 1)
		}
		if (k == 0 && s != 0) || hi <= s || o != off || (b != 0 && b != 1 && b != 2 && b != 4 && b != 8 && b != w) || b > w {
			return rr, bad
		}
		off += (hi - s) * b
	}
	if off != len(rr.data) {
		return rr, bad
	}
	return rr, nil
}

// run is run k's first row, data offset and width.
func (rr *runsRegion) run(k int) (start, off, width int) {
	h := rr.hdr[k*(rr.w+5):]
	return int(h[0])<<8 | int(h[1]), int(h[2])<<8 | int(h[3]), int(h[4+rr.w])
}

// runOf finds the run holding row i, with the region's index.
func (rr *runsRegion) runOf(i int) int {
	if rr.rankMode {
		count := int(rr.counts[2*(i/64)])<<8 | int(rr.counts[2*(i/64)+1])
		// Starts in rows [64·(i/64), i], one byte at a time.
		for b := (i / 64) * 8; b <= i/8; b++ {
			x := rr.starts[b]
			if b == i/8 {
				x &= 0xFF << (7 - i%8)
			}
			count += bits.OnesCount8(x)
		}
		return count - 1
	}
	lo, hi := 0, rr.r-1
	for lo < hi {
		mid := (lo + hi + 1) / 2
		if s, _, _ := rr.run(mid); s <= i {
			lo = mid
		} else {
			hi = mid - 1
		}
	}
	return lo
}

// value is row i's value body, read in place (null reports a NULL).
func (rr *runsRegion) value(i int) (v uint64, null bool) {
	if rr.bitmap[i/8]&(0x80>>(i%8)) != 0 {
		return 0, true
	}
	k := rr.runOf(i)
	s, o, b := rr.run(k)
	base := beUintN(rr.hdr[k*(rr.w+5)+4:], rr.w)
	if b == 0 {
		return base, false
	}
	at := o + (i-s)*b
	return base + beUintN(rr.data[at:], b), false
}

// parseRuns rebuilds a kind 7 region's dense slots, as the reader does for kind 1.
func parseRuns(payload []byte, p, end, n, w int) (paxRegion, error) {
	rr, err := parseRunsRegion(payload, p, end, n, w)
	if err != nil {
		return paxRegion{}, err
	}
	body := make([]byte, n*w)
	for k := 0; k < rr.r; k++ {
		s, o, b := rr.run(k)
		hi := n
		if k+1 < rr.r {
			hi, _, _ = rr.run(k + 1)
		}
		base := beUint(rr.hdr[k*(w+5)+4 : k*(w+5)+4+w])
		for i := s; i < hi; i++ {
			if rr.bitmap[i/8]&(0x80>>(i%8)) != 0 {
				continue
			}
			v := base
			if b > 0 {
				v += beUint(rr.data[o+(i-s)*b : o+(i-s+1)*b])
			}
			putBEUint(body[i*w:(i+1)*w], v)
		}
	}
	return paxRegion{width: w, bitmap: rr.bitmap, body: body}, nil
}
