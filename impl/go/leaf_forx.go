package jed

// PROTOTYPE (spec/design/leaf-write-cost.md §4, option b): a base-plus-offsets region that stores
// one value as an exception instead of widening every offset for it. With JED_LEAF_FORX=on a fixed
// column eligible for base plus offsets may also take region kind 6:
//
//	null bitmap | base (w) | offset width bw (u8) | exception row (u16) | exception body (w) | N offsets (bw)
//
// The exception row is the one holding the column's single largest present value (or, when that is
// not unique or saves less, its single smallest); its offset is zero and its body is stored whole. Its
// saving over the plain region is N·(w − bw) − (2w + 3), against kind 1's N·(w − bw₁) − (w + 1); the
// region takes the larger, kind 1 on a tie. A leaf's size depends on this choice, so it changes the
// split points and bytes of files with such columns. Off by default.

import (
	"os"
	"sync/atomic"
)

var leafFORX = os.Getenv("JED_LEAF_FORX") == "on"

// forxWritten counts kind 6 regions written (tests).
var forxWritten atomic.Int64

// regionFORX is the region kind byte of a base-plus-offsets region with one exception.
const regionFORX byte = 6

// forExc is the offset width of the best one-exception layout for the present values r spans, and
// whether the excepted value is the largest; ok false when no single value can be excepted.
func forExc(w int, r forRange) (bw int, top bool, ok bool) {
	if !leafFORX || !r.two {
		return 0, false, false
	}
	bw = w
	if r.hi2 < r.hi {
		bw, top, ok = forWidth(w, r.lo, r.hi2), true, true
	}
	if r.lo2 > r.lo {
		if b := forWidth(w, r.lo2, r.hi); !ok || b < bw {
			bw, top, ok = b, false, true
		}
	}
	return bw, top, ok
}

// forExcSaving is the bytes a kind 6 region saves over the plain one (0 when not admitted).
func forExcSaving(n, w int, r forRange) int {
	bw, _, ok := forExc(w, r)
	if !ok || bw >= w {
		return 0
	}
	if s := n*(w-bw) - (2*w + 3); s > 0 {
		return s
	}
	return 0
}

// forRemoved reports whether removing present value s from the values r spans may change what r
// records, so r must be measured again.
func (r *forRange) forRemoved(s uint64) bool {
	return s == r.lo || s == r.hi || (leafFORX && (s == r.lo2 || s == r.hi2))
}

// encodeFORX writes a kind 6 region of n values (encodeRegion's vb and nulls) whose present values
// span r, for which forExc admitted an exception.
func encodeFORX(w, n int, vb [][]byte, nulls []bool, r forRange) []byte {
	bw, top, _ := forExc(w, r)
	base, excepted := r.lo2, r.lo
	if top {
		base, excepted = r.lo, r.hi
	}
	exc := -1
	for i := 0; i < n; i++ {
		if !nulls[i] && beUint(vb[i]) == excepted {
			exc = i
			break
		}
	}
	forxWritten.Add(1)
	out := make([]byte, 1, 1+(n+7)/8+w+1+2+w+n*bw)
	out[0] = regionFORX
	bm := len(out)
	out = append(out, make([]byte, (n+7)/8)...)
	for i, isNull := range nulls {
		if isNull {
			out[bm+i/8] |= 0x80 >> (i % 8)
		}
	}
	at := len(out)
	out = append(out, make([]byte, w+1+2+w+n*bw)...)
	putBEUint(out[at:at+w], base)
	out[at+w] = byte(bw)
	out[at+w+1], out[at+w+2] = byte(exc>>8), byte(exc)
	copy(out[at+w+3:at+2*w+3], vb[exc])
	at += 2*w + 3
	for i := 0; i < n; i++ {
		if !nulls[i] && i != exc {
			putBEUint(out[at+i*bw:at+(i+1)*bw], beUint(vb[i])-base)
		}
	}
	return out
}

// parseFORX rebuilds a kind 6 region's dense slots: payload[p:end] follows the kind byte.
func parseFORX(payload []byte, p, end, n, w int) (paxRegion, error) {
	bad := newError(DataCorrupted, "PAX leaf narrow region with an exception is malformed")
	bm := p
	p += (n + 7) / 8
	if p+2*w+3 > end {
		return paxRegion{}, bad
	}
	bitmap := payload[bm:p]
	base := beUint(payload[p : p+w])
	bw := int(payload[p+w])
	exc := int(payload[p+w+1])<<8 | int(payload[p+w+2])
	excBody := payload[p+w+3 : p+2*w+3]
	p += 2*w + 3
	if (bw != 0 && bw != 1 && bw != 2 && bw != 4) || bw >= w || p+n*bw != end || exc >= n ||
		bitmap[exc/8]&(0x80>>(exc%8)) != 0 || (bw > 0 && beUint(payload[p+exc*bw:p+(exc+1)*bw]) != 0) {
		return paxRegion{}, bad
	}
	body := make([]byte, n*w)
	for i := 0; i < n; i++ {
		if bitmap[i/8]&(0x80>>(i%8)) != 0 {
			continue
		}
		if i == exc {
			copy(body[i*w:(i+1)*w], excBody)
			continue
		}
		putBEUint(body[i*w:(i+1)*w], base+beUint(payload[p+i*bw:p+(i+1)*bw]))
	}
	return paxRegion{width: w, bitmap: bitmap, body: body}, nil
}
