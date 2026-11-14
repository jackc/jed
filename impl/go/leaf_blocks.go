package jed

// PROTOTYPE (spec/design/leaf-write-cost.md §4c, option e): a base-plus-offsets column in fixed-size
// blocks. With JED_LEAF_BLOCKS=B (rows per block), a fixed column eligible for base plus offsets may
// also take region kind 8:
//
//	null bitmap | exception flag (u8) | [exception row (u16), exception body (w)]
//	| K × (base w, width u8, data offset u16) | the blocks' offsets, in row order
//
// Block k holds rows [k·B, min((k+1)·B, N)); K = ⌈N/B⌉. Row i's block is i / B, so a reader finds
// it with no index: the value is base + the `width`-byte offset at data offset + (i mod B)·width. A
// block's width is the narrowest of 0, 1, 2, 4, 8 below w that holds its present values' span, or w
// (raw). A NULL's offset is zero.
//
// With JED_LEAF_BLOCKS_EXC=on the region may also store one value whole, as the exception region
// does (leaf_forx.go): the column's single largest present value, or its single smallest when that
// saves more, leaves its block's span and its offset is zero. Size:
//
//	1 + [2 + w] + Σ_blocks (w + 3 + len·width)
//
// and the region takes kind 8 only when that is smaller than every other kind. Off by default.

import (
	"encoding/binary"
	"math/bits"
	"os"
	"strconv"
	"sync/atomic"
)

var leafBlocks = func() int {
	b, err := strconv.Atoi(os.Getenv("JED_LEAF_BLOCKS"))
	if err != nil || b < 2 {
		return 0
	}
	return b
}()

var leafBlocksExc = os.Getenv("JED_LEAF_BLOCKS_EXC") == "on"

// blocksWritten counts kind 8 regions written (tests).
var blocksWritten atomic.Int64

// regionBlocks is the region kind byte of a base-plus-offsets region in fixed blocks.
const regionBlocks byte = 8

// leafPositional reports whether a base-plus-offsets column's size depends on where each value is,
// not only on the set of values: runs and blocks. Edits that insert, remove or replace a row then
// measure the column again.
func leafPositional() bool { return leafRuns != leafRunsOff || leafBlocks > 0 }

// blockWidth is the offset width of a block whose present values r spans, in a w-byte column: the
// narrowest of 0, 1, 2, 4 below w, or w.
func blockWidth(w int, r forRange) int {
	if !r.any {
		return 0
	}
	return forWidth(w, r.lo, r.hi)
}

// blocksAcc measures a column in blocks as rows arrive in order.
type blocksAcc struct {
	w, b   int
	n      int
	done   int        // bytes of the completed blocks
	blocks []forRange // each block's span (with its second extremes), completed and current
	lens   []int
	all    forRange // the column's span, for the exception
}

func newBlocksAcc(w int) *blocksAcc {
	return &blocksAcc{w: w, b: leafBlocks}
}

func (a *blocksAcc) clone() *blocksAcc {
	if a == nil {
		return nil
	}
	out := *a
	out.blocks = append([]forRange(nil), a.blocks...)
	out.lens = append([]int(nil), a.lens...)
	return &out
}

func (a *blocksAcc) push(v uint64, present bool) {
	if a.n%a.b == 0 {
		if k := len(a.blocks); k > 0 {
			a.done += a.blockCost(a.blocks[k-1], a.lens[k-1])
		}
		a.blocks = append(a.blocks, forRange{})
		a.lens = append(a.lens, 0)
	}
	k := len(a.blocks) - 1
	if present {
		a.blocks[k].add(v)
		a.all.add(v)
	}
	a.lens[k]++
	a.n++
}

func (a *blocksAcc) blockCost(r forRange, n int) int {
	return a.w + 3 + n*blockWidth(a.w, r)
}

// size is the kind 8 region's bytes after the kind byte and the null bitmap, without an exception.
func (a *blocksAcc) size() int {
	total := 1 + a.done
	if k := len(a.blocks); k > 0 {
		total += a.blockCost(a.blocks[k-1], a.lens[k-1])
	}
	return total
}

// exc is the region size with one exception, when one makes it smaller: the column's single largest
// present value (top), or its single smallest when that gives a smaller region.
func (a *blocksAcc) exc() (size int, top bool, ok bool) {
	if !leafBlocksExc || !a.all.two {
		return 0, false, false
	}
	base := a.size()
	// with is the region size when the column's extreme leaves the block holding it.
	with := func(top bool) int {
		for k, r := range a.blocks {
			if !r.any || (top && r.hi != a.all.hi) || (!top && r.lo != a.all.lo) {
				continue
			}
			var without forRange
			if r.two {
				without = forRange{lo: r.lo, hi: r.hi2, any: true}
				if !top {
					without = forRange{lo: r.lo2, hi: r.hi, any: true}
				}
			}
			return base - a.blockCost(r, a.lens[k]) + a.blockCost(without, a.lens[k]) + 2 + a.w
		}
		panic("BUG: a column's extreme is in no block")
	}
	if a.all.hi2 < a.all.hi {
		size, top, ok = with(true), true, true
	}
	if a.all.lo2 > a.all.lo {
		if s := with(false); !ok || s < size {
			size, top, ok = s, false, true
		}
	}
	if !ok || size >= base {
		return 0, false, false
	}
	return size, top, true
}

// saving is the bytes kind 8 saves over the plain region (0 when it does not, or is off).
func (a *blocksAcc) saving() int {
	if a == nil || leafBlocks == 0 {
		return 0
	}
	size := a.size()
	if s, _, ok := a.exc(); ok {
		size = s
	}
	if s := a.n*a.w - size; s > 0 {
		return s
	}
	return 0
}

// encodeBlocks writes a kind 8 region of n values (encodeRegion's vb and nulls).
func encodeBlocks(w, n int, vb [][]byte, nulls []bool, a *blocksAcc) []byte {
	blocksWritten.Add(1)
	excRow := -1
	if _, top, ok := a.exc(); ok {
		want := a.all.lo
		if top {
			want = a.all.hi
		}
		for i := 0; i < n; i++ {
			if !nulls[i] && beUint(vb[i]) == want {
				excRow = i
				break
			}
		}
	}
	out := make([]byte, 1, 1+(n+7)/8+a.n*w)
	out[0] = regionBlocks
	bm := len(out)
	out = append(out, make([]byte, (n+7)/8)...)
	for i, isNull := range nulls {
		if isNull {
			out[bm+i/8] |= 0x80 >> (i % 8)
		}
	}
	if excRow >= 0 {
		out = append(out, 1, byte(excRow>>8), byte(excRow))
		out = append(out, vb[excRow]...)
	} else {
		out = append(out, 0)
	}
	b := a.b
	k := (n + b - 1) / b
	hdrAt := len(out)
	out = append(out, make([]byte, k*(w+3))...)
	off := 0
	for blk := 0; blk < k; blk++ {
		lo, hi := blk*b, min((blk+1)*b, n)
		var r forRange
		for i := lo; i < hi; i++ {
			if !nulls[i] && i != excRow {
				r.add(beUint(vb[i]))
			}
		}
		width := blockWidth(w, r)
		base := uint64(0)
		if r.any {
			base = r.lo
		}
		h := out[hdrAt+blk*(w+3):]
		putBEUint(h[0:w], base)
		h[w] = byte(width)
		h[w+1], h[w+2] = byte(off>>8), byte(off)
		at := len(out)
		out = append(out, make([]byte, (hi-lo)*width)...)
		if width > 0 {
			for i := lo; i < hi; i++ {
				if !nulls[i] && i != excRow {
					putBEUint(out[at+(i-lo)*width:at+(i-lo+1)*width], beUint(vb[i])-base)
				}
			}
		}
		off += (hi - lo) * width
	}
	return out
}

// blocksRegion is a kind 8 region's parts, as an in-place reader uses them.
type blocksRegion struct {
	n, w, b, k int
	shift      int // log2(b) when b is a power of two, else -1
	bitmap     []byte
	excRow     int
	excBody    []byte
	hdr        []byte
	data       []byte
}

// parseBlocksRegion validates a kind 8 region (payload[p:end] follows the kind byte).
func parseBlocksRegion(payload []byte, p, end, n, w int) (blocksRegion, error) {
	bad := newError(DataCorrupted, "PAX leaf region in blocks is malformed")
	br := blocksRegion{n: n, w: w, b: leafBlocks, excRow: -1}
	if br.b == 0 || p+(n+7)/8+1 > end {
		return br, bad
	}
	br.k = (n + br.b - 1) / br.b
	br.shift = -1
	if br.b&(br.b-1) == 0 {
		br.shift = bits.TrailingZeros(uint(br.b))
	}
	br.bitmap = payload[p : p+(n+7)/8]
	p += (n + 7) / 8
	flag := payload[p]
	p++
	switch flag {
	case 0:
	case 1:
		if p+2+w > end {
			return br, bad
		}
		br.excRow = int(payload[p])<<8 | int(payload[p+1])
		br.excBody = payload[p+2 : p+2+w]
		p += 2 + w
		if br.excRow >= n || br.bitmap[br.excRow/8]&(0x80>>(br.excRow%8)) != 0 {
			return br, bad
		}
	default:
		return br, bad
	}
	if p+br.k*(w+3) > end {
		return br, bad
	}
	br.hdr = payload[p : p+br.k*(w+3)]
	p += br.k * (w + 3)
	br.data = payload[p:end]
	off := 0
	for blk := 0; blk < br.k; blk++ {
		width, o := br.block(blk)
		if o != off || (width != 0 && width != 1 && width != 2 && width != 4 && width != w) || width > w {
			return br, bad
		}
		off += (min((blk+1)*br.b, n) - blk*br.b) * width
	}
	if off != len(br.data) {
		return br, bad
	}
	return br, nil
}

// block is block k's offset width and data offset.
func (br *blocksRegion) block(k int) (width, off int) {
	h := br.hdr[k*(br.w+3):]
	return int(h[br.w]), int(h[br.w+1])<<8 | int(h[br.w+2])
}

// value is row i's value body, read in place.
func (br *blocksRegion) value(i int) (v uint64, null bool) {
	if br.bitmap[i/8]&(0x80>>(i%8)) != 0 {
		return 0, true
	}
	if i == br.excRow {
		return beUint(br.excBody), false
	}
	var k int
	if br.shift >= 0 {
		k = i >> br.shift
	} else {
		k = i / br.b
	}
	h := br.hdr[k*(br.w+3):]
	base := beUintN(h, br.w)
	width := int(h[br.w])
	if width == 0 {
		return base, false
	}
	at := (int(h[br.w+1])<<8 | int(h[br.w+2])) + (i-k*br.b)*width
	return base + beUintN(br.data[at:], width), false
}

// beUintN reads an n-byte big-endian unsigned integer from the start of b with one load where it
// can (the in-place readers of leaf-write-cost.md §4b).
func beUintN(b []byte, n int) uint64 {
	switch n {
	case 1:
		return uint64(b[0])
	case 2:
		return uint64(binary.BigEndian.Uint16(b))
	case 4:
		return uint64(binary.BigEndian.Uint32(b))
	case 8:
		return binary.BigEndian.Uint64(b)
	}
	return beUint(b[:n])
}

// parseBlocks rebuilds a kind 8 region's dense slots, as the reader does for kind 1.
func parseBlocks(payload []byte, p, end, n, w int) (paxRegion, error) {
	br, err := parseBlocksRegion(payload, p, end, n, w)
	if err != nil {
		return paxRegion{}, err
	}
	body := make([]byte, n*w)
	for i := 0; i < n; i++ {
		if v, null := br.value(i); !null {
			putBEUint(body[i*w:(i+1)*w], v)
		}
	}
	return paxRegion{width: w, bitmap: br.bitmap, body: body}, nil
}
