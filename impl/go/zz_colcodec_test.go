package jed

// EXPERIMENT (not part of the core's test contract): what do column encodings that give up direct
// indexing actually cost and save? Gated by JED_FORMAT_EXP=1. It builds the format experiment's
// database, takes every leaf's real column regions and keys, encodes each under a set of candidate
// codecs, and measures bytes, encode time, whole-column decode time, in-place aggregate time and the
// time to reach one value.
//
//	JED_FORMAT_EXP=1 JED_FORMAT_EXP_DIR=<dir> go test -run TestColumnCodecExperiment -v -timeout 30m .

import (
	"bytes"
	"compress/flate"
	"context"
	"encoding/binary"
	"fmt"
	"hash/crc32"
	"io"
	"math/bits"
	"os"
	"path/filepath"
	"sort"
	"syscall"
	"testing"
	"time"
	"unsafe"
)

// ---- bit packing (LSB-first, little-endian words; buffers carry 8 bytes of padding) ----------------

func ccPut(buf []byte, pos, nbits int, v uint64) {
	at := pos >> 3
	w := binary.LittleEndian.Uint64(buf[at:])
	w |= v << (pos & 7)
	binary.LittleEndian.PutUint64(buf[at:], w)
}

func ccGet(buf []byte, pos, nbits int) uint64 {
	return (binary.LittleEndian.Uint64(buf[pos>>3:]) >> (pos & 7)) & (1<<nbits - 1)
}

func ccBE(b []byte, w int) uint64 {
	switch w {
	case 1:
		return uint64(b[0])
	case 2:
		return uint64(binary.BigEndian.Uint16(b))
	case 4:
		return uint64(binary.BigEndian.Uint32(b))
	case 8:
		return binary.BigEndian.Uint64(b)
	}
	var v uint64
	for _, x := range b[:w] {
		v = v<<8 | uint64(x)
	}
	return v
}

func ccPutBE(b []byte, w int, v uint64) {
	for i := w - 1; i >= 0; i-- {
		b[i] = byte(v)
		v >>= 8
	}
}

// ---- integer column codecs ----------------------------------------------------------------------------

// ccInt is one integer codec. enc returns the encoded buffer (possibly padded) and its true size, or
// ok=false when the codec does not apply. all materializes every value into out. get returns value i
// (nil: the codec has no direct access, so a point read is all + index). sum is the aggregate computed
// from the encoded form.
type ccInt struct {
	name   string
	direct bool
	enc    func(vals []uint64, w int) (buf []byte, size int, ok bool)
	all    func(buf []byte, n, w int, out []uint64)
	get    func(buf []byte, n, w, i int) uint64
	sum    func(buf []byte, n, w int) uint64
}

func ccMinMax(vals []uint64) (lo, hi uint64) {
	lo, hi = vals[0], vals[0]
	for _, v := range vals {
		if v < lo {
			lo = v
		}
		if v > hi {
			hi = v
		}
	}
	return
}

func ccByteWidth(rng uint64) int {
	switch {
	case rng < 1<<8:
		return 1
	case rng < 1<<16:
		return 2
	case rng < 1<<32:
		return 4
	}
	return 8
}

var ccIntCodecs = []ccInt{
	{name: "plain", direct: true,
		enc: func(vals []uint64, w int) ([]byte, int, bool) {
			b := make([]byte, len(vals)*w)
			for i, v := range vals {
				ccPutBE(b[i*w:], w, v)
			}
			return b, len(b), true
		},
		all: func(b []byte, n, w int, out []uint64) {
			for i := 0; i < n; i++ {
				out[i] = ccBE(b[i*w:], w)
			}
		},
		get: func(b []byte, n, w, i int) uint64 { return ccBE(b[i*w:], w) },
		sum: func(b []byte, n, w int) uint64 {
			var s uint64
			for i := 0; i < n; i++ {
				s += ccBE(b[i*w:], w)
			}
			return s
		},
	},
	// base + fixed 1/2/4-byte offsets.
	{name: "for-bytes", direct: true,
		enc: func(vals []uint64, w int) ([]byte, int, bool) {
			lo, hi := ccMinMax(vals)
			bw := ccByteWidth(hi - lo)
			if bw >= w {
				return nil, 0, false
			}
			b := make([]byte, 9+len(vals)*bw)
			b[0] = byte(bw)
			binary.BigEndian.PutUint64(b[1:], lo)
			for i, v := range vals {
				ccPutBE(b[9+i*bw:], bw, v-lo)
			}
			return b, 1 + w + len(vals)*bw, true
		},
		all: func(b []byte, n, w int, out []uint64) {
			bw, lo := int(b[0]), binary.BigEndian.Uint64(b[1:])
			d := b[9:]
			switch bw {
			case 1:
				for i := 0; i < n; i++ {
					out[i] = lo + uint64(d[i])
				}
			case 2:
				for i := 0; i < n; i++ {
					out[i] = lo + uint64(binary.BigEndian.Uint16(d[2*i:]))
				}
			default:
				for i := 0; i < n; i++ {
					out[i] = lo + uint64(binary.BigEndian.Uint32(d[4*i:]))
				}
			}
		},
		get: func(b []byte, n, w, i int) uint64 {
			bw := int(b[0])
			return binary.BigEndian.Uint64(b[1:]) + ccBE(b[9+i*bw:], bw)
		},
		sum: func(b []byte, n, w int) uint64 {
			bw, lo := int(b[0]), binary.BigEndian.Uint64(b[1:])
			d := b[9:]
			var s uint64
			switch bw {
			case 1:
				for i := 0; i < n; i++ {
					s += uint64(d[i])
				}
			case 2:
				for i := 0; i < n; i++ {
					s += uint64(binary.BigEndian.Uint16(d[2*i:]))
				}
			default:
				for i := 0; i < n; i++ {
					s += uint64(binary.BigEndian.Uint32(d[4*i:]))
				}
			}
			return s + uint64(n)*lo
		},
	},
	// base + bit-packed offsets. Still direct: value i is at bit i·bits.
	{name: "for-bits", direct: true,
		enc: func(vals []uint64, w int) ([]byte, int, bool) {
			lo, hi := ccMinMax(vals)
			nb := bits.Len64(hi - lo)
			if nb > 56 || nb >= 8*w {
				return nil, 0, false
			}
			size := 1 + w + (len(vals)*nb+7)/8
			b := make([]byte, 9+(len(vals)*nb+7)/8+8)
			b[0] = byte(nb)
			binary.BigEndian.PutUint64(b[1:], lo)
			for i, v := range vals {
				ccPut(b[9:], i*nb, nb, v-lo)
			}
			return b, size, true
		},
		all: func(b []byte, n, w int, out []uint64) {
			nb, lo := int(b[0]), binary.BigEndian.Uint64(b[1:])
			d := b[9:]
			mask := uint64(1)<<nb - 1
			for i, pos := 0, 0; i < n; i, pos = i+1, pos+nb {
				out[i] = lo + (binary.LittleEndian.Uint64(d[pos>>3:])>>(pos&7))&mask
			}
		},
		get: func(b []byte, n, w, i int) uint64 {
			nb := int(b[0])
			return binary.BigEndian.Uint64(b[1:]) + ccGet(b[9:], i*nb, nb)
		},
		sum: func(b []byte, n, w int) uint64 {
			nb, lo := int(b[0]), binary.BigEndian.Uint64(b[1:])
			d := b[9:]
			mask := uint64(1)<<nb - 1
			var s uint64
			for i, pos := 0, 0; i < n; i, pos = i+1, pos+nb {
				s += (binary.LittleEndian.Uint64(d[pos>>3:]) >> (pos & 7)) & mask
			}
			return s + uint64(n)*lo
		},
	},
	// first value, then bit-packed (delta − min delta). Value i needs the i deltas before it.
	{name: "delta-bits",
		enc: func(vals []uint64, w int) ([]byte, int, bool) {
			n := len(vals)
			if n < 2 {
				return nil, 0, false
			}
			dlo, dhi := int64(vals[1]-vals[0]), int64(vals[1]-vals[0])
			for i := 2; i < n; i++ {
				d := int64(vals[i] - vals[i-1])
				if d < dlo {
					dlo = d
				}
				if d > dhi {
					dhi = d
				}
			}
			nb := bits.Len64(uint64(dhi - dlo))
			if nb > 56 {
				return nil, 0, false
			}
			size := 1 + w + 8 + ((n-1)*nb+7)/8
			b := make([]byte, 17+((n-1)*nb+7)/8+8)
			b[0] = byte(nb)
			binary.BigEndian.PutUint64(b[1:], vals[0])
			binary.BigEndian.PutUint64(b[9:], uint64(dlo))
			for i := 1; i < n; i++ {
				ccPut(b[17:], (i-1)*nb, nb, uint64(int64(vals[i]-vals[i-1])-dlo))
			}
			return b, size, true
		},
		all: func(b []byte, n, w int, out []uint64) {
			nb, v, dlo := int(b[0]), binary.BigEndian.Uint64(b[1:]), binary.BigEndian.Uint64(b[9:])
			d := b[17:]
			mask := uint64(1)<<nb - 1
			out[0] = v
			for i, pos := 1, 0; i < n; i, pos = i+1, pos+nb {
				v += dlo + (binary.LittleEndian.Uint64(d[pos>>3:])>>(pos&7))&mask
				out[i] = v
			}
		},
		get: func(b []byte, n, w, at int) uint64 {
			nb, v, dlo := int(b[0]), binary.BigEndian.Uint64(b[1:]), binary.BigEndian.Uint64(b[9:])
			d := b[17:]
			mask := uint64(1)<<nb - 1
			for i, pos := 1, 0; i <= at; i, pos = i+1, pos+nb {
				v += dlo + (binary.LittleEndian.Uint64(d[pos>>3:])>>(pos&7))&mask
			}
			return v
		},
		sum: func(b []byte, n, w int) uint64 {
			nb, v, dlo := int(b[0]), binary.BigEndian.Uint64(b[1:]), binary.BigEndian.Uint64(b[9:])
			d := b[17:]
			mask := uint64(1)<<nb - 1
			s := v
			for i, pos := 1, 0; i < n; i, pos = i+1, pos+nb {
				v += dlo + (binary.LittleEndian.Uint64(d[pos>>3:])>>(pos&7))&mask
				s += v
			}
			return s
		},
	},
	// first value, then zigzag LEB128 deltas: variable-size values.
	{name: "delta-varint",
		enc: func(vals []uint64, w int) ([]byte, int, bool) {
			b := make([]byte, 0, len(vals)*2+10)
			b = binary.AppendUvarint(b, vals[0])
			for i := 1; i < len(vals); i++ {
				b = binary.AppendVarint(b, int64(vals[i]-vals[i-1]))
			}
			return b, len(b), true
		},
		all: func(b []byte, n, w int, out []uint64) {
			v, k := binary.Uvarint(b)
			out[0] = v
			for i := 1; i < n; i++ {
				d, m := binary.Varint(b[k:])
				k += m
				v += uint64(d)
				out[i] = v
			}
		},
		get: func(b []byte, n, w, at int) uint64 {
			v, k := binary.Uvarint(b)
			for i := 1; i <= at; i++ {
				d, m := binary.Varint(b[k:])
				k += m
				v += uint64(d)
			}
			return v
		},
	},
	// each value as a LEB128 varint of (value − min): variable-size values, no delta.
	{name: "for-varint",
		enc: func(vals []uint64, w int) ([]byte, int, bool) {
			lo, _ := ccMinMax(vals)
			b := make([]byte, 8, len(vals)*2+10)
			binary.BigEndian.PutUint64(b, lo)
			for _, v := range vals {
				b = binary.AppendUvarint(b, v-lo)
			}
			return b, len(b) - 8 + w, true
		},
		all: func(b []byte, n, w int, out []uint64) {
			lo := binary.BigEndian.Uint64(b)
			k := 8
			for i := 0; i < n; i++ {
				d, m := binary.Uvarint(b[k:])
				k += m
				out[i] = lo + d
			}
		},
		get: func(b []byte, n, w, at int) uint64 {
			lo := binary.BigEndian.Uint64(b)
			k := 8
			var d uint64
			for i := 0; i <= at; i++ {
				var m int
				d, m = binary.Uvarint(b[k:])
				k += m
			}
			return lo + d
		},
	},
	// up to 256 distinct values + one code byte per row.
	{name: "dict8", direct: true,
		enc: func(vals []uint64, w int) ([]byte, int, bool) {
			idx := map[uint64]int{}
			var dict []uint64
			for _, v := range vals {
				if _, ok := idx[v]; !ok {
					if len(dict) == 256 {
						return nil, 0, false
					}
					idx[v] = len(dict)
					dict = append(dict, v)
				}
			}
			b := make([]byte, 2+8*len(dict)+len(vals))
			binary.BigEndian.PutUint16(b, uint16(len(dict)))
			for i, v := range dict {
				binary.BigEndian.PutUint64(b[2+8*i:], v)
			}
			codes := b[2+8*len(dict):]
			for i, v := range vals {
				codes[i] = byte(idx[v])
			}
			return b, 1 + w*len(dict) + len(vals), true
		},
		all: func(b []byte, n, w int, out []uint64) {
			d := int(binary.BigEndian.Uint16(b))
			dict, codes := b[2:], b[2+8*d:]
			for i := 0; i < n; i++ {
				out[i] = binary.BigEndian.Uint64(dict[8*int(codes[i]):])
			}
		},
		get: func(b []byte, n, w, i int) uint64 {
			d := int(binary.BigEndian.Uint16(b))
			return binary.BigEndian.Uint64(b[2+8*int(b[2+8*d+i]):])
		},
	},
	// LZ4 over the plain big-endian slots.
	{name: "lz4",
		enc: func(vals []uint64, w int) ([]byte, int, bool) {
			p := make([]byte, len(vals)*w)
			for i, v := range vals {
				ccPutBE(p[i*w:], w, v)
			}
			c := lz4Compress(p)
			return c, 2 + len(c), true
		},
		all: func(b []byte, n, w int, out []uint64) {
			p, _ := lz4Decompress(b, n*w)
			for i := 0; i < n; i++ {
				out[i] = ccBE(p[i*w:], w)
			}
		},
	},
	// DEFLATE over the plain slots: an entropy-coder reference, not a candidate.
	{name: "flate",
		enc: func(vals []uint64, w int) ([]byte, int, bool) {
			p := make([]byte, len(vals)*w)
			for i, v := range vals {
				ccPutBE(p[i*w:], w, v)
			}
			c := ccDeflate(p)
			return c, 2 + len(c), true
		},
		all: func(b []byte, n, w int, out []uint64) {
			p := ccInflate(b, n*w)
			for i := 0; i < n; i++ {
				out[i] = ccBE(p[i*w:], w)
			}
		},
	},
}

func ccDeflate(p []byte) []byte {
	var z bytes.Buffer
	zw, _ := flate.NewWriter(&z, 6)
	zw.Write(p)
	zw.Close()
	return z.Bytes()
}

func ccInflate(c []byte, n int) []byte {
	out := make([]byte, n)
	io.ReadFull(flate.NewReader(bytes.NewReader(c)), out)
	return out
}

// ---- byte-string column codecs (text, bytea, json, 16-byte fixed) ---------------------------------------

// A byte column's plain image is n u16 end offsets then the bodies. visit returns a checksum over every
// value (its length and first byte), touching each value once; get returns the same for value i.
type ccBytes struct {
	name   string
	direct bool
	enc    func(ends []uint16, body []byte) (buf []byte, size int, ok bool)
	visit  func(buf []byte, n int) uint64
	get    func(buf []byte, n, i int) uint64
}

func ccSpanSum(body []byte, lo, hi int) uint64 {
	if hi == lo {
		return 0
	}
	return uint64(hi-lo) + uint64(body[lo])
}

func ccPlainImage(ends []uint16, body []byte) []byte {
	b := make([]byte, 2*len(ends)+len(body))
	for i, e := range ends {
		binary.BigEndian.PutUint16(b[2*i:], e)
	}
	copy(b[2*len(ends):], body)
	return b
}

func ccPlainVisit(b []byte, n int) uint64 {
	body := b[2*n:]
	var s uint64
	lo := 0
	for i := 0; i < n; i++ {
		hi := int(binary.BigEndian.Uint16(b[2*i:]))
		s += ccSpanSum(body, lo, hi)
		lo = hi
	}
	return s
}

func ccPlainGet(b []byte, n, i int) uint64 {
	lo := 0
	if i > 0 {
		lo = int(binary.BigEndian.Uint16(b[2*(i-1):]))
	}
	return ccSpanSum(b[2*n:], lo, int(binary.BigEndian.Uint16(b[2*i:])))
}

var ccBytesCodecs = []ccBytes{
	{name: "plain", direct: true,
		enc: func(ends []uint16, body []byte) ([]byte, int, bool) {
			b := ccPlainImage(ends, body)
			return b, len(b), true
		},
		visit: ccPlainVisit, get: ccPlainGet,
	},
	// up to 256 distinct values (a u16 directory + blob) and one code byte per row.
	{name: "dict8", direct: true,
		enc: func(ends []uint16, body []byte) ([]byte, int, bool) {
			idx := map[string]int{}
			var dends []uint16
			var dict []byte
			codes := make([]byte, len(ends))
			lo := 0
			for i, e := range ends {
				v := string(body[lo:e])
				lo = int(e)
				c, ok := idx[v]
				if !ok {
					if len(dends) == 256 {
						return nil, 0, false
					}
					c = len(dends)
					idx[v] = c
					dict = append(dict, v...)
					dends = append(dends, uint16(len(dict)))
				}
				codes[i] = byte(c)
			}
			b := make([]byte, 0, 2+2*len(dends)+len(dict)+len(codes))
			b = binary.BigEndian.AppendUint16(b, uint16(len(dends)))
			for _, e := range dends {
				b = binary.BigEndian.AppendUint16(b, e)
			}
			b = append(b, dict...)
			b = append(b, codes...)
			return b, len(b) - 1, true
		},
		visit: func(b []byte, n int) uint64 {
			d := int(binary.BigEndian.Uint16(b))
			dir := b[2:]
			blob := b[2+2*d:]
			codes := blob[len(blob)-n:]
			var s uint64
			for i := 0; i < n; i++ {
				c := int(codes[i])
				lo := 0
				if c > 0 {
					lo = int(binary.BigEndian.Uint16(dir[2*(c-1):]))
				}
				s += ccSpanSum(blob, lo, int(binary.BigEndian.Uint16(dir[2*c:])))
			}
			return s
		},
		get: func(b []byte, n, i int) uint64 {
			d := int(binary.BigEndian.Uint16(b))
			dir := b[2:]
			blob := b[2+2*d:]
			c := int(blob[len(blob)-n+i])
			lo := 0
			if c > 0 {
				lo = int(binary.BigEndian.Uint16(dir[2*(c-1):]))
			}
			return ccSpanSum(blob, lo, int(binary.BigEndian.Uint16(dir[2*c:])))
		},
	},
	// LZ4 over the whole plain image (directory and bodies).
	{name: "lz4",
		enc: func(ends []uint16, body []byte) ([]byte, int, bool) {
			p := ccPlainImage(ends, body)
			c := lz4Compress(p)
			b := binary.BigEndian.AppendUint32(nil, uint32(len(p)))
			return append(b, c...), 2 + len(c), true
		},
		visit: func(b []byte, n int) uint64 {
			p, _ := lz4Decompress(b[4:], int(binary.BigEndian.Uint32(b)))
			return ccPlainVisit(p, n)
		},
	},
	// LZ4 over the bodies only; the directory stays plain, so lengths are direct but bytes are not.
	{name: "lz4-body",
		enc: func(ends []uint16, body []byte) ([]byte, int, bool) {
			c := lz4Compress(body)
			b := make([]byte, 0, 2*len(ends)+len(c))
			for _, e := range ends {
				b = binary.BigEndian.AppendUint16(b, e)
			}
			return append(b, c...), 2 + len(b) + len(c), true
		},
		visit: func(b []byte, n int) uint64 {
			raw := 0
			if n > 0 {
				raw = int(binary.BigEndian.Uint16(b[2*(n-1):]))
			}
			body, _ := lz4Decompress(b[2*n:], raw)
			var s uint64
			lo := 0
			for i := 0; i < n; i++ {
				hi := int(binary.BigEndian.Uint16(b[2*i:]))
				s += ccSpanSum(body, lo, hi)
				lo = hi
			}
			return s
		},
	},
	{name: "flate",
		enc: func(ends []uint16, body []byte) ([]byte, int, bool) {
			p := ccPlainImage(ends, body)
			c := ccDeflate(p)
			b := binary.BigEndian.AppendUint32(nil, uint32(len(p)))
			return append(b, c...), 2 + len(c), true
		},
		visit: func(b []byte, n int) uint64 {
			return ccPlainVisit(ccInflate(b[4:], int(binary.BigEndian.Uint32(b))), n)
		},
	},
}

// ---- key codecs ----------------------------------------------------------------------------------------

// A leaf's keys in full: one blob and n end offsets. all rebuilds every key (returning a checksum);
// find returns the index of probe by whatever search the codec allows.
type ccKey struct {
	name   string
	direct bool
	enc    func(ends []uint32, blob []byte, fixed int) (buf []byte, size int, ok bool)
	all    func(buf []byte, n int, scratch []byte) uint64
	find   func(buf []byte, n int, probe, scratch []byte) int
}

func ccKeyAt(ends []uint32, blob []byte, i int) []byte {
	lo := uint32(0)
	if i > 0 {
		lo = ends[i-1]
	}
	return blob[lo:ends[i]]
}

func ccLCP(a, b []byte) int {
	n := 0
	for n < len(a) && n < len(b) && a[n] == b[n] {
		n++
	}
	return n
}

// ccKeyImage is the working form every key codec reads: u16 prefix length, prefix, u32 count of
// suffix-directory entries (n u16 ends) and the suffix blob.
func ccPrefixImage(ends []uint32, blob []byte) (img []byte, p int) {
	n := len(ends)
	if n > 1 {
		p = ccLCP(ccKeyAt(ends, blob, 0), ccKeyAt(ends, blob, n-1))
	}
	img = binary.BigEndian.AppendUint16(nil, uint16(p))
	img = append(img, blob[:p]...)
	off := 0
	for i := 0; i < n; i++ {
		off += len(ccKeyAt(ends, blob, i)) - p
		img = binary.BigEndian.AppendUint16(img, uint16(off))
	}
	for i := 0; i < n; i++ {
		img = append(img, ccKeyAt(ends, blob, i)[p:]...)
	}
	return img, p
}

func ccPrefixFind(img []byte, n int, probe []byte) int {
	p := int(binary.BigEndian.Uint16(img))
	prefix := img[2 : 2+p]
	if len(probe) < p || !bytes.Equal(probe[:p], prefix) {
		return -1
	}
	rest := probe[p:]
	dir := img[2+p:]
	sfx := dir[2*n:]
	lo, hi := 0, n
	for lo < hi {
		mid := int(uint(lo+hi) >> 1)
		s := 0
		if mid > 0 {
			s = int(binary.BigEndian.Uint16(dir[2*(mid-1):]))
		}
		switch bytes.Compare(sfx[s:binary.BigEndian.Uint16(dir[2*mid:])], rest) {
		case 0:
			return mid
		case -1:
			lo = mid + 1
		default:
			hi = mid
		}
	}
	return -1
}

func ccPrefixAll(img []byte, n int, scratch []byte) uint64 {
	p := int(binary.BigEndian.Uint16(img))
	prefix := img[2 : 2+p]
	dir := img[2+p:]
	sfx := dir[2*n:]
	out := scratch[:0]
	lo := 0
	for i := 0; i < n; i++ {
		hi := int(binary.BigEndian.Uint16(dir[2*i:]))
		out = append(out, prefix...)
		out = append(out, sfx[lo:hi]...)
		lo = hi
	}
	return uint64(len(out))
}

const ccRestart = 16

// ccFrontEnc front-codes keys: each is (shared-with-previous u8/u16, suffix length u8/u16, suffix),
// with a full key every `restart` keys and a u16 offset table of the restarts in front.
func ccFrontEnc(ends []uint32, blob []byte, restart int) []byte {
	n := len(ends)
	var body []byte
	var offs []uint16
	var prev []byte
	for i := 0; i < n; i++ {
		k := ccKeyAt(ends, blob, i)
		shared := 0
		if i%restart == 0 {
			offs = append(offs, uint16(len(body)))
		} else {
			shared = ccLCP(prev, k)
		}
		body = binary.AppendUvarint(body, uint64(shared))
		body = binary.AppendUvarint(body, uint64(len(k)-shared))
		body = append(body, k[shared:]...)
		prev = k
	}
	out := make([]byte, 0, 2*len(offs)+len(body))
	for _, o := range offs {
		out = binary.BigEndian.AppendUint16(out, o)
	}
	return append(out, body...)
}

// ccFrontScan decodes keys from entry `from` (a restart) until stop reports true; key is rebuilt in
// scratch.
func ccFrontScan(body []byte, at, from, n int, scratch []byte, each func(i int, key []byte) bool) {
	key := scratch[:0]
	for i := from; i < n; i++ {
		shared, m := binary.Uvarint(body[at:])
		at += m
		l, m := binary.Uvarint(body[at:])
		at += m
		key = append(key[:shared], body[at:at+int(l)]...)
		at += int(l)
		if each(i, key) {
			return
		}
	}
}

func ccFront(restart int) ccKey {
	nrest := func(n int) int { return (n + restart - 1) / restart }
	return ccKey{
		enc: func(ends []uint32, blob []byte, fixed int) ([]byte, int, bool) {
			b := ccFrontEnc(ends, blob, restart)
			return b, len(b), true
		},
		all: func(b []byte, n int, scratch []byte) uint64 {
			var s uint64
			ccFrontScan(b[2*nrest(n):], 0, 0, n, scratch, func(i int, key []byte) bool {
				s += uint64(len(key))
				return false
			})
			return s
		},
		find: func(b []byte, n int, probe, scratch []byte) int {
			r := nrest(n)
			body := b[2*r:]
			// binary search the restarts: the last one whose full key is <= probe.
			lo, hi := 0, r
			for lo < hi {
				mid := int(uint(lo+hi) >> 1)
				at := int(binary.BigEndian.Uint16(b[2*mid:]))
				_, m := binary.Uvarint(body[at:])
				l, m2 := binary.Uvarint(body[at+m:])
				k := body[at+m+m2 : at+m+m2+int(l)]
				if bytes.Compare(k, probe) <= 0 {
					lo = mid + 1
				} else {
					hi = mid
				}
			}
			if lo == 0 {
				return -1
			}
			found := -1
			ccFrontScan(body, int(binary.BigEndian.Uint16(b[2*(lo-1):])), (lo-1)*restart, n, scratch, func(i int, key []byte) bool {
				c := bytes.Compare(key, probe)
				if c == 0 {
					found = i
				}
				return c >= 0
			})
			return found
		},
	}
}

var ccKeyCodecs = func() []ccKey {
	f16 := ccFront(ccRestart)
	f16.name = "front-16"
	fall := ccFront(1 << 30)
	fall.name = "front-all"
	return []ccKey{
		// what steps 1 to 5 store: shared prefix, suffix directory (none when fixed), suffixes.
		{name: "prefix", direct: true,
			enc: func(ends []uint32, blob []byte, fixed int) ([]byte, int, bool) {
				img, p := ccPrefixImage(ends, blob)
				size := len(img)
				if fixed > 0 {
					size -= 2 * len(ends)
				}
				_ = p
				return img, size, true
			},
			all:  ccPrefixAll,
			find: func(b []byte, n int, probe, scratch []byte) int { return ccPrefixFind(b, n, probe) },
		},
		f16, fall,
		// the prefix image under LZ4.
		{name: "lz4",
			enc: func(ends []uint32, blob []byte, fixed int) ([]byte, int, bool) {
				img, _ := ccPrefixImage(ends, blob)
				c := lz4Compress(img)
				b := binary.BigEndian.AppendUint32(nil, uint32(len(img)))
				return append(b, c...), 2 + len(c), true
			},
			all: func(b []byte, n int, scratch []byte) uint64 {
				img, _ := lz4Decompress(b[4:], int(binary.BigEndian.Uint32(b)))
				return ccPrefixAll(img, n, scratch)
			},
			find: func(b []byte, n int, probe, scratch []byte) int {
				img, _ := lz4Decompress(b[4:], int(binary.BigEndian.Uint32(b)))
				return ccPrefixFind(img, n, probe)
			},
		},
		{name: "flate",
			enc: func(ends []uint32, blob []byte, fixed int) ([]byte, int, bool) {
				img, _ := ccPrefixImage(ends, blob)
				c := ccDeflate(img)
				b := binary.BigEndian.AppendUint32(nil, uint32(len(img)))
				return append(b, c...), 2 + len(c), true
			},
			all: func(b []byte, n int, scratch []byte) uint64 {
				return ccPrefixAll(ccInflate(b[4:], int(binary.BigEndian.Uint32(b))), n, scratch)
			},
			find: func(b []byte, n int, probe, scratch []byte) int {
				return ccPrefixFind(ccInflate(b[4:], int(binary.BigEndian.Uint32(b))), n, probe)
			},
		},
	}
}()

// ---- collection -----------------------------------------------------------------------------------------

const (
	ccKindInt = iota
	ccKindBytes
	ccKindDecimal
	ccKindKey
)

type ccLeaf struct {
	n     int
	vals  []uint64 // int, or a decimal column whose values all share one scale and fit an i64
	ends  []uint16 // bytes
	body  []byte
	kends []uint32 // keys
	kblob []byte
	cur   int // bytes the page spends on this region today
}

type ccColumn struct {
	tree, name string
	kind       int
	w          int // int width; key: the fixed key length or 0
	elemWidth  int // an array of fixed-width integers: the element width, else 0
	leaves     []ccLeaf
	rows       int
}

func ccCollect(t *testing.T, db *Database) (cols []*ccColumn, pages [][]byte) {
	pin, err := db.core.integrityPin(context.Background(), "main", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer pin.release()
	w := &expWalker{paging: pin.paging, pcap: pin.paging.cap(), t: t}
	var names []string
	for name := range pin.snap.tables {
		names = append(names, name)
	}
	sort.Strings(names)
	walk := func(tree string, root *pnode, colTypes []colType, colNames []string) {
		if root == nil {
			return
		}
		key := &ccColumn{tree: tree, name: "(keys)", kind: ccKindKey, w: -1}
		var cs []*ccColumn
		for c, ty := range colTypes {
			col := &ccColumn{tree: tree, name: colNames[c], kind: ccKindBytes}
			if w, fixed := fixedValueWidth(ty); fixed && w <= 8 {
				col.kind, col.w = ccKindInt, w
			} else if !fixed && !ty.Composite && ty.Elem == nil && ty.RangeElem == nil && ty.Scalar == scalarDecimal {
				col.kind, col.w = ccKindDecimal, 8
			} else if ty.Elem != nil {
				if w, fixed := fixedValueWidth(*ty.Elem); fixed && (w == 2 || w == 4 || w == 8) {
					col.elemWidth = w
				}
			}
			cs = append(cs, col)
		}
		w.eachLeaf(root.ref(), colTypes, &expTree{}, func(n int, payload []byte, leaf *paxLeaf) {
			lk := ccLeaf{n: n}
			for i := 0; i < n; i++ {
				k := leaf.key(i)
				lk.kblob = append(lk.kblob, k...)
				lk.kends = append(lk.kends, uint32(len(lk.kblob)))
				if key.w == -1 {
					key.w = len(k)
				} else if key.w != len(k) {
					key.w = 0
				}
			}
			key.leaves = append(key.leaves, lk)
			key.rows += n
			for c := range colTypes {
				r := &leaf.regions[c]
				col := cs[c]
				lc := ccLeaf{n: n}
				col.rows += n
				switch {
				case r.elided:
					col.kind = -1
				case r.width > 0 && r.width <= 8:
					lc.cur = 1 + len(r.bitmap) + len(r.body)
					lc.vals = make([]uint64, n)
					var prev uint64
					for i := 0; i < n; i++ {
						if r.bitmap[i/8]&(0x80>>(i%8)) == 0 {
							prev = ccBE(r.body[i*r.width:], r.width)
						}
						lc.vals[i] = prev
					}
				case r.width > 8:
					lc.cur = 1 + len(r.bitmap) + len(r.body)
					lc.body = bytes.Clone(r.body)
					lc.ends = make([]uint16, n)
					for i := range lc.ends {
						lc.ends[i] = uint16((i + 1) * r.width)
					}
					col.w = r.width
				default:
					lc.cur = 1 + len(r.ends) + len(r.body)
					lc.body = bytes.Clone(r.body)
					lc.ends = make([]uint16, n)
					for i := range lc.ends {
						lc.ends[i] = uint16(paxDirEnd(r.ends, i, leaf.dw))
					}
					if col.kind == ccKindDecimal {
						lc.vals = make([]uint64, n)
						scale, lo := -1, 0
						var prev uint64
						for i := 0; i < n && lc.vals != nil; i++ {
							sp := r.body[lo:lc.ends[i]]
							lo = int(lc.ends[i])
							if len(sp) > 1 && sp[0] == 0 {
								sc, v, ok := expDecimalInt(sp[1:])
								if !ok || (scale >= 0 && sc != scale) {
									lc.vals = nil
									break
								}
								scale, prev = sc, uint64(v)^(1<<63)
							} else if len(sp) != 0 {
								lc.vals = nil
								break
							}
							lc.vals[i] = prev
						}
					}
				}
				col.leaves = append(col.leaves, lc)
			}
			// The page is zero-padded after its last region.
			used := len(payload)
			for used > 0 && payload[used-1] == 0 {
				used--
			}
			pages = append(pages, bytes.Clone(payload[:used]))
		})
		cols = append(cols, key)
		for _, c := range cs {
			if c.kind >= 0 {
				cols = append(cols, c)
			}
		}
	}
	for _, name := range names {
		table, store := pin.snap.tables[name], pin.snap.stores[name]
		var colNames []string
		for _, c := range table.Columns {
			colNames = append(colNames, c.Name)
		}
		walk(name, store.treeRoot(), store.colTypes, colNames)
		for _, def := range table.Indexes {
			if ist := pin.snap.indexStores[def.Name]; ist != nil {
				walk(def.Name, ist.treeRoot(), nil, nil)
			}
		}
	}
	return cols, pages
}

// ---- timing ---------------------------------------------------------------------------------------------

// ccTime returns the best of three timings of f, each repeated until it has run for 20 ms, in ns per
// call.
func ccTime(f func()) float64 {
	best := 0.0
	for trial := 0; trial < 3; trial++ {
		reps := 0
		start := time.Now()
		for time.Since(start) < 20*time.Millisecond {
			f()
			reps++
		}
		ns := float64(time.Since(start).Nanoseconds()) / float64(reps)
		if trial == 0 || ns < best {
			best = ns
		}
	}
	return best
}

var ccSink uint64

type ccResult struct {
	name                       string
	direct                     bool
	size                       int     // bytes over every leaf where the codec applies
	leaves                     int     // leaves where it applies
	encNs, allNs, sumNs, getNs float64 // per value (enc, all, sum) and per point read (get)
}

// ccMeasure sizes one column under every codec over all its leaves, and times a sample of them.
func ccMeasure(t *testing.T, col *ccColumn, perLeaf func(leaf int, codec string, size int)) []ccResult {
	step := len(col.leaves)/150 + 1
	var res []ccResult
	sampleVals := 0
	for j := 0; j < len(col.leaves); j += step {
		sampleVals += col.leaves[j].n
	}
	nsamp := (len(col.leaves) + step - 1) / step
	scratch := make([]uint64, 70000)
	kscratch := make([]byte, 0, 1<<17)
	pick := func(j, n int) int { return (j*7919 + 13) % n }
	switch col.kind {
	case ccKindInt, ccKindDecimal:
		for _, cd := range ccIntCodecs {
			r := ccResult{name: cd.name, direct: cd.direct}
			var bufs [][]byte
			var ls []*ccLeaf
			for j := range col.leaves {
				lc := &col.leaves[j]
				if lc.vals == nil || lc.n == 0 {
					continue
				}
				b, size, ok := cd.enc(lc.vals, col.w)
				if !ok {
					continue
				}
				r.size += size
				r.leaves++
				perLeaf(j, cd.name, size)
				if j%step == 0 {
					cd.all(b, lc.n, col.w, scratch)
					for i, v := range lc.vals {
						if scratch[i] != v {
							t.Fatalf("%s.%s %s: value %d decodes to %d, want %d", col.tree, col.name, cd.name, i, scratch[i], v)
						}
					}
					if cd.get != nil {
						if i := pick(j, lc.n); cd.get(b, lc.n, col.w, i) != lc.vals[i] {
							t.Fatalf("%s.%s %s: get(%d) is wrong", col.tree, col.name, cd.name, i)
						}
					}
					bufs = append(bufs, b)
					ls = append(ls, lc)
				}
			}
			if len(bufs) == 0 {
				continue
			}
			vals := 0
			for _, lc := range ls {
				vals += lc.n
			}
			r.encNs = ccTime(func() {
				for _, lc := range ls {
					b, _, _ := cd.enc(lc.vals, col.w)
					ccSink += uint64(len(b))
				}
			}) / float64(vals)
			r.allNs = ccTime(func() {
				for k, lc := range ls {
					cd.all(bufs[k], lc.n, col.w, scratch)
					ccSink += scratch[lc.n-1]
				}
			}) / float64(vals)
			r.sumNs = ccTime(func() {
				for k, lc := range ls {
					if cd.sum != nil {
						ccSink += cd.sum(bufs[k], lc.n, col.w)
						continue
					}
					cd.all(bufs[k], lc.n, col.w, scratch)
					var s uint64
					for _, v := range scratch[:lc.n] {
						s += v
					}
					ccSink += s
				}
			}) / float64(vals)
			r.getNs = ccTime(func() {
				for k, lc := range ls {
					i := pick(k, lc.n)
					if cd.get != nil {
						ccSink += cd.get(bufs[k], lc.n, col.w, i)
						continue
					}
					cd.all(bufs[k], lc.n, col.w, scratch)
					ccSink += scratch[i]
				}
			}) / float64(len(ls))
			res = append(res, r)
		}
		if col.kind == ccKindInt {
			break
		}
		fallthrough // a decimal column is also measured in its stored byte form
	case ccKindBytes:
		for _, cd := range ccBytesCodecs {
			r := ccResult{name: cd.name, direct: cd.direct}
			if col.kind == ccKindDecimal {
				r.name = "stored-" + cd.name
			}
			var bufs [][]byte
			var ls []*ccLeaf
			for j := range col.leaves {
				lc := &col.leaves[j]
				if lc.n == 0 {
					continue
				}
				b, size, ok := cd.enc(lc.ends, lc.body)
				if !ok {
					continue
				}
				r.size += size
				r.leaves++
				perLeaf(j, r.name, size)
				if j%step == 0 {
					if want := ccPlainVisit(ccPlainImage(lc.ends, lc.body), lc.n); cd.visit(b, lc.n) != want {
						t.Fatalf("%s.%s %s: visit checksum differs", col.tree, col.name, cd.name)
					}
					bufs = append(bufs, b)
					ls = append(ls, lc)
				}
			}
			if len(bufs) == 0 {
				continue
			}
			vals := 0
			for _, lc := range ls {
				vals += lc.n
			}
			r.encNs = ccTime(func() {
				for _, lc := range ls {
					b, _, _ := cd.enc(lc.ends, lc.body)
					ccSink += uint64(len(b))
				}
			}) / float64(vals)
			r.allNs = ccTime(func() {
				for k, lc := range ls {
					ccSink += cd.visit(bufs[k], lc.n)
				}
			}) / float64(vals)
			r.getNs = ccTime(func() {
				for k, lc := range ls {
					if cd.get != nil {
						ccSink += cd.get(bufs[k], lc.n, pick(k, lc.n))
						continue
					}
					ccSink += cd.visit(bufs[k], lc.n)
				}
			}) / float64(len(ls))
			res = append(res, r)
		}
	case ccKindKey:
		for _, cd := range ccKeyCodecs {
			r := ccResult{name: cd.name, direct: cd.direct}
			var bufs [][]byte
			var ls []*ccLeaf
			for j := range col.leaves {
				lc := &col.leaves[j]
				if lc.n == 0 {
					continue
				}
				b, size, ok := cd.enc(lc.kends, lc.kblob, col.w)
				if !ok {
					continue
				}
				r.size += size
				r.leaves++
				perLeaf(j, cd.name, size)
				if j%step == 0 {
					i := pick(j, lc.n)
					if got := cd.find(b, lc.n, ccKeyAt(lc.kends, lc.kblob, i), kscratch); got != i {
						t.Fatalf("%s keys %s: find returns %d, want %d", col.tree, cd.name, got, i)
					}
					bufs = append(bufs, b)
					ls = append(ls, lc)
				}
			}
			if len(bufs) == 0 {
				continue
			}
			vals := 0
			for _, lc := range ls {
				vals += lc.n
			}
			r.encNs = ccTime(func() {
				for _, lc := range ls {
					b, _, _ := cd.enc(lc.kends, lc.kblob, col.w)
					ccSink += uint64(len(b))
				}
			}) / float64(vals)
			r.allNs = ccTime(func() {
				for k, lc := range ls {
					ccSink += cd.all(bufs[k], lc.n, kscratch)
				}
			}) / float64(vals)
			r.getNs = ccTime(func() {
				for k, lc := range ls {
					ccSink += uint64(cd.find(bufs[k], lc.n, ccKeyAt(lc.kends, lc.kblob, pick(k, lc.n)), kscratch))
				}
			}) / float64(len(ls))
			res = append(res, r)
		}
	}
	_ = nsamp
	return res
}

func TestColumnCodecExperiment(t *testing.T) {
	if os.Getenv("JED_FORMAT_EXP") == "" {
		t.Skip("set JED_FORMAT_EXP=1")
	}
	dir := os.Getenv("JED_FORMAT_EXP_DIR")
	if dir == "" {
		dir = t.TempDir()
	}
	path := filepath.Join(dir, "codec.jed")
	os.Remove(path)
	os.RemoveAll(path + ".lock")
	db := expBuild(t, path, DefaultPageSize)
	if err := db.Compact("main"); err != nil {
		t.Fatal(err)
	}
	cols, pages := ccCollect(t, db)
	db.Close()

	// Strategy totals: for each leaf region take the smallest codec the strategy allows.
	type strategy struct {
		name  string
		allow func(kind int, codec string) bool
		total int
	}
	in := func(set ...string) func(string) bool {
		return func(c string) bool {
			for _, s := range set {
				if s == c {
					return true
				}
			}
			return false
		}
	}
	byteDirect := in("plain", "for-bytes", "dict8", "prefix", "stored-plain", "stored-dict8")
	bitDirect := in("plain", "for-bytes", "for-bits", "dict8", "prefix", "stored-plain", "stored-dict8")
	seq := in("plain", "for-bytes", "for-bits", "dict8", "prefix", "stored-plain", "stored-dict8", "delta-bits", "delta-varint", "for-varint", "front-16", "front-all")
	seqLZ := in("plain", "for-bytes", "for-bits", "dict8", "prefix", "stored-plain", "stored-dict8", "delta-bits", "delta-varint", "for-varint", "front-16", "front-all", "lz4", "lz4-body", "stored-lz4", "stored-lz4-body")
	strategies := []*strategy{
		{name: "today (steps 1-5)", allow: func(k int, c string) bool { return c == "plain" || c == "prefix" || c == "stored-plain" }},
		{name: "direct, byte-aligned (FOR bytes, dict8)", allow: func(k int, c string) bool { return byteDirect(c) }},
		{name: "direct, bit-packed (+ FOR bits)", allow: func(k int, c string) bool { return bitDirect(c) }},
		{name: "+ sequential ints and keys (delta, varint, front coding)", allow: func(k int, c string) bool { return seq(c) }},
		{name: "+ per-column LZ4 where smaller", allow: func(k int, c string) bool { return seqLZ(c) }},
	}

	type colRes struct {
		col *ccColumn
		res []ccResult
	}
	byTree := map[string][]colRes{}
	var treeOrder []string
	fmt.Printf("\n%d columns, %d leaf pages\n", len(cols), len(pages))
	fmt.Printf("\nper column: B/val over every leaf; ns/val for encode, decode-all and sum; ns per point read (one value of one leaf)\n")
	for _, col := range cols {
		best := make([]map[int]int, len(strategies))
		for i := range best {
			best[i] = map[int]int{}
		}
		res := ccMeasure(t, col, func(leaf int, codec string, size int) {
			for i, s := range strategies {
				if !s.allow(col.kind, codec) {
					continue
				}
				if cur, ok := best[i][leaf]; !ok || size < cur {
					best[i][leaf] = size
				}
			}
		})
		for i, s := range strategies {
			for _, v := range best[i] {
				s.total += v
			}
		}
		if len(byTree[col.tree]) == 0 {
			treeOrder = append(treeOrder, col.tree)
		}
		byTree[col.tree] = append(byTree[col.tree], colRes{col, res})
		kind := map[int]string{ccKindInt: "int", ccKindBytes: "bytes", ccKindDecimal: "decimal", ccKindKey: "keys"}[col.kind]
		avg := 0
		if len(col.leaves) > 0 {
			avg = col.rows / len(col.leaves)
		}
		fmt.Printf("\n%s.%s (%s, width %d, %d rows, %d per leaf)\n", col.tree, col.name, kind, col.w, col.rows, avg)
		fmt.Printf("  %-16s %6s %8s %8s %8s %8s %9s\n", "codec", "leaves", "B/val", "enc", "all", "sum", "point")
		for _, r := range res {
			rows := float64(col.rows) * float64(r.leaves) / float64(len(col.leaves))
			d := " "
			if r.direct {
				d = "*"
			}
			fmt.Printf("  %-15s%s %5.0f%% %8.2f %8.2f %8.2f %8.2f %9.1f\n", r.name, d, 100*float64(r.leaves)/float64(len(col.leaves)),
				float64(r.size)/rows, r.encNs, r.allNs, r.sumNs, r.getNs)
		}
	}

	fmt.Printf("\nleaf bytes by strategy (each region takes its smallest allowed codec; +1 byte per region for the codec id is not counted)\n")
	for _, s := range strategies {
		fmt.Printf("  %-58s %7.1f MB  %5.1f%%\n", s.name, float64(s.total)/1e6, 100*float64(s.total)/float64(strategies[0].total))
	}

	// Per tree and strategy: what one row costs. Each column takes the smallest allowed codec that
	// applies to at least 99% of its leaves. "point" is reaching every column of one row plus finding
	// its key; "scan" is decoding every column of every row.
	fmt.Printf("\nper tree and strategy: bytes per row, ns to read one whole row of one leaf (point), ns per row to decode every column (scan)\n")
	for _, tree := range treeOrder {
		fmt.Printf("  %s\n", tree)
		for _, s := range strategies {
			var bpr, point, scan float64
			for _, cr := range byTree[tree] {
				var pickd *ccResult
				var pb float64
				for i := range cr.res {
					r := &cr.res[i]
					if !s.allow(cr.col.kind, r.name) || r.leaves*100 < 99*len(cr.col.leaves) {
						continue
					}
					b := float64(r.size) / (float64(cr.col.rows) * float64(r.leaves) / float64(len(cr.col.leaves)))
					if pickd == nil || b < pb {
						pickd, pb = r, b
					}
				}
				if pickd == nil {
					continue
				}
				bpr += pb
				point += pickd.getNs
				scan += pickd.allNs
			}
			fmt.Printf("    %-58s %7.1f B/row %9.0f ns point %7.1f ns/row scan\n", s.name, bpr, point, scan)
		}
	}

	// Whole-page compression of today's pages, for comparison, with its per-page costs.
	used, lz, fl := 0, 0, 0
	var comp [][]byte
	step := len(pages)/400 + 1
	for j, p := range pages {
		used += len(p)
		c := lz4Compress(p)
		lz += len(c)
		fl += len(ccDeflate(p))
		if j%step == 0 {
			comp = append(comp, c)
		}
	}
	fmt.Printf("  %-58s %7.1f MB  (page payloads incl. directories: %.1f MB)\n", "whole-page LZ4 of today's pages", float64(lz)/1e6, float64(used)/1e6)
	fmt.Printf("  %-58s %7.1f MB\n", "whole-page DEFLATE of today's pages", float64(fl)/1e6)

	var sample [][]byte
	for j := 0; j < len(pages); j += step {
		sample = append(sample, pages[j])
	}
	full := make([]byte, DefaultPageSize)
	fmt.Printf("\nper-page costs (ns per 8 KiB page, %d sampled pages)\n", len(sample))
	fmt.Printf("  %-44s %9.0f\n", "CRC-32 of a page", ccTime(func() { ccSink += uint64(crc32.ChecksumIEEE(full)) }))
	fmt.Printf("  %-44s %9.0f\n", "LZ4 compress a page", ccTime(func() {
		for _, p := range sample {
			ccSink += uint64(len(lz4Compress(p)))
		}
	})/float64(len(sample)))
	fmt.Printf("  %-44s %9.0f\n", "LZ4 decompress a page", ccTime(func() {
		for k, p := range sample {
			out, _ := lz4Decompress(comp[k], len(p))
			ccSink += uint64(len(out))
		}
	})/float64(len(sample)))
	fmt.Printf("  %-44s %9.0f\n", "DEFLATE decompress a page", ccTime(func() {
		p := sample[0]
		ccSink += uint64(len(ccInflate(ccDeflate(p), len(p))))
	}))
	fmt.Printf("  %-44s %9.0f\n", "copy a page (memmove)", ccTime(func() { ccSink += uint64(copy(full, sample[0])) }))

	ccReadCosts(t, path)
}

// ccReadCosts measures what reading one page costs on this machine: from the OS page cache, and from
// the device with O_DIRECT (bypassing the cache).
func ccReadCosts(t *testing.T, path string) {
	const pageSize = int(DefaultPageSize)
	st, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	npages := int(st.Size()) / pageSize
	const align = 4096
	raw := make([]byte, pageSize+align)
	off := int(uintptr(unsafe.Pointer(&raw[0])) & (align - 1))
	buf := raw[align-off : align-off+pageSize]
	measure := func(flag int, label string, seqRead bool) {
		f, err := os.OpenFile(path, os.O_RDONLY|flag, 0)
		if err != nil {
			fmt.Printf("  %-44s unavailable: %v\n", label, err)
			return
		}
		defer f.Close()
		const reads = 4000
		durs := make([]time.Duration, 0, reads)
		x := uint64(88172645463325252)
		for i := 0; i < reads; i++ {
			x ^= x << 13
			x ^= x >> 7
			x ^= x << 17
			page := int(x % uint64(npages))
			if seqRead {
				page = i % npages
			}
			st := time.Now()
			if _, err := f.ReadAt(buf, int64(page)*int64(pageSize)); err != nil {
				fmt.Printf("  %-44s read failed: %v\n", label, err)
				return
			}
			durs = append(durs, time.Since(st))
		}
		sort.Slice(durs, func(a, b int) bool { return durs[a] < durs[b] })
		fmt.Printf("  %-44s median %8.1f µs   p90 %8.1f µs\n", label, float64(durs[reads/2].Nanoseconds())/1e3, float64(durs[reads*9/10].Nanoseconds())/1e3)
	}
	fmt.Printf("\nreading one 8 KiB page of %s (%d pages)\n", filepath.Base(path), npages)
	measure(0, "pread, OS page cache (random)", false)
	measure(syscall.O_DIRECT, "pread, O_DIRECT from the device (random)", false)
	measure(syscall.O_DIRECT, "pread, O_DIRECT from the device (sequential)", true)
}
