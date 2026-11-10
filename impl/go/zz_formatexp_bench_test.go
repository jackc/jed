package jed

// EXPERIMENT microbenchmarks for zz_formatexp_test.go: decode kernels over today's leaf regions
// versus candidate encodings. Each benchmark walks `pages` regions of `n` values, like a scan.
//
//	go test -run xxx -bench FormatExp -benchtime 200x .

import (
	"bytes"
	"encoding/binary"
	"math/rand"
	"testing"
)

const (
	fxN     = 124  // rows per leaf (line_items)
	fxPages = 4000 // leaves
)

var fxSink uint64

// --- i64 column: dense big-endian sign-flipped slots vs FOR ----------------------------------------

func fxInt64Pages(spread int) (plain [][]byte, for1 [][]byte, for2 [][]byte, bases []uint64) {
	r := rand.New(rand.NewSource(7))
	base := uint64(1) << 63
	for p := 0; p < fxPages; p++ {
		a, b, c := make([]byte, fxN*8), make([]byte, fxN), make([]byte, fxN*2)
		for i := 0; i < fxN; i++ {
			d := uint64(r.Intn(spread))
			binary.BigEndian.PutUint64(a[i*8:], base+d)
			b[i] = byte(d)
			binary.LittleEndian.PutUint16(c[i*2:], uint16(d))
		}
		plain, for1, for2, bases = append(plain, a), append(for1, b), append(for2, c), append(bases, base)
		base += uint64(spread)
	}
	return
}

func BenchmarkFormatExpSumI64Plain(b *testing.B) {
	plain, _, _, _ := fxInt64Pages(200)
	b.SetBytes(fxN * fxPages)
	b.ResetTimer()
	for it := 0; it < b.N; it++ {
		var s int64
		for _, pg := range plain {
			for i := 0; i+8 <= len(pg); i += 8 {
				s += int64(binary.BigEndian.Uint64(pg[i:]) ^ 1<<63)
			}
		}
		fxSink += uint64(s)
	}
}

func BenchmarkFormatExpSumI64For1(b *testing.B) {
	_, for1, _, bases := fxInt64Pages(200)
	b.SetBytes(fxN * fxPages)
	b.ResetTimer()
	for it := 0; it < b.N; it++ {
		var s int64
		for p, pg := range for1 {
			var t uint64
			for _, v := range pg {
				t += uint64(v)
			}
			s += int64(bases[p]^1<<63)*int64(len(pg)) + int64(t)
		}
		fxSink += uint64(s)
	}
}

func BenchmarkFormatExpSumI64For2(b *testing.B) {
	_, _, for2, bases := fxInt64Pages(200)
	b.SetBytes(fxN * fxPages)
	b.ResetTimer()
	for it := 0; it < b.N; it++ {
		var s int64
		for p, pg := range for2 {
			base := int64(bases[p] ^ 1<<63)
			for i := 0; i+2 <= len(pg); i += 2 {
				s += base + int64(binary.LittleEndian.Uint16(pg[i:]))
			}
		}
		fxSink += uint64(s)
	}
}

// A filter `col = const` over the same data: plain compares 8-byte slots; FOR rewrites the constant
// once per page and compares bytes (and skips the page when the constant is out of [base, base+max]).
func BenchmarkFormatExpEqI64Plain(b *testing.B) {
	plain, _, _, bases := fxInt64Pages(200)
	want := make([]byte, 8)
	binary.BigEndian.PutUint64(want, bases[fxPages/2]+17)
	w := binary.BigEndian.Uint64(want)
	b.SetBytes(fxN * fxPages)
	b.ResetTimer()
	for it := 0; it < b.N; it++ {
		c := 0
		for _, pg := range plain {
			for i := 0; i+8 <= len(pg); i += 8 {
				if binary.BigEndian.Uint64(pg[i:]) == w {
					c++
				}
			}
		}
		fxSink += uint64(c)
	}
}

func BenchmarkFormatExpEqI64For1(b *testing.B) {
	_, for1, _, bases := fxInt64Pages(200)
	w := bases[fxPages/2] + 17
	b.SetBytes(fxN * fxPages)
	b.ResetTimer()
	for it := 0; it < b.N; it++ {
		c := 0
		for p, pg := range for1 {
			d := w - bases[p]
			if d > 255 { // out of the page's range: zone-map skip
				continue
			}
			c += bytes.Count(pg, []byte{byte(d)})
		}
		fxSink += uint64(c)
	}
}

// --- low-cardinality text: directory + tagged bodies vs a page dictionary ----------------------------

var fxStatuses = []string{"pending", "paid", "shipped", "delivered", "cancelled", "refunded"}

type fxTextPage struct {
	ends, body []byte // today: u32 end offsets + (tag, u16 len, bytes)
	dict       [][]byte
	codes      []byte
}

func fxTextPages() []fxTextPage {
	r := rand.New(rand.NewSource(8))
	out := make([]fxTextPage, fxPages)
	for p := range out {
		pg := &out[p]
		for _, s := range fxStatuses {
			pg.dict = append(pg.dict, []byte(s))
		}
		for i := 0; i < fxN; i++ {
			c := r.Intn(len(fxStatuses))
			s := fxStatuses[c]
			pg.body = append(pg.body, 0, byte(len(s)>>8), byte(len(s)))
			pg.body = append(pg.body, s...)
			pg.ends = binary.BigEndian.AppendUint32(pg.ends, uint32(len(pg.body)))
			pg.codes = append(pg.codes, byte(c))
		}
	}
	return out
}

func BenchmarkFormatExpEqTextPlain(b *testing.B) {
	pages := fxTextPages()
	want := []byte("shipped")
	b.SetBytes(fxN * fxPages)
	b.ResetTimer()
	for it := 0; it < b.N; it++ {
		c := 0
		for p := range pages {
			pg := &pages[p]
			lo := uint32(0)
			for i := 0; i < fxN; i++ {
				hi := paxDirEnd(pg.ends, i)
				sp := pg.body[lo:hi]
				lo = hi
				if len(sp) > 3 && sp[0] == 0 && bytes.Equal(sp[3:], want) {
					c++
				}
			}
		}
		fxSink += uint64(c)
	}
}

func BenchmarkFormatExpEqTextDict(b *testing.B) {
	pages := fxTextPages()
	want := []byte("shipped")
	b.SetBytes(fxN * fxPages)
	b.ResetTimer()
	for it := 0; it < b.N; it++ {
		c := 0
		for p := range pages {
			pg := &pages[p]
			code := -1
			for k, d := range pg.dict {
				if bytes.Equal(d, want) {
					code = k
					break
				}
			}
			if code < 0 {
				continue
			}
			c += bytes.Count(pg.codes, []byte{byte(code)})
		}
		fxSink += uint64(c)
	}
}

// --- point lookup: binary search over full keys vs a shared page prefix ------------------------------

func BenchmarkFormatExpKeySearchPlain(b *testing.B) {
	keyEnd := make([]byte, 0, fxN*4)
	blob := make([]byte, 0, fxN*8)
	for i := 0; i < fxN; i++ {
		blob = binary.BigEndian.AppendUint64(blob, 1<<63+1000000+uint64(i))
		keyEnd = binary.BigEndian.AppendUint32(keyEnd, uint32(len(blob)))
	}
	probe := make([]byte, 8)
	b.ResetTimer()
	for it := 0; it < b.N; it++ {
		for q := 0; q < fxPages; q++ {
			binary.BigEndian.PutUint64(probe, 1<<63+1000000+uint64((q*31)%fxN))
			lo, hi := 0, fxN
			for lo < hi {
				m := (lo + hi) / 2
				s := uint32(0)
				if m > 0 {
					s = paxDirEnd(keyEnd, m-1)
				}
				if bytes.Compare(blob[s:paxDirEnd(keyEnd, m)], probe) < 0 {
					lo = m + 1
				} else {
					hi = m
				}
			}
			fxSink += uint64(lo)
		}
	}
}

func BenchmarkFormatExpKeySearchPrefix(b *testing.B) {
	// fixed-width key, 7-byte shared prefix, 1-byte suffixes, no directory.
	prefix := make([]byte, 8)
	binary.BigEndian.PutUint64(prefix, 1<<63+1000000)
	base := prefix[7]
	prefix = prefix[:7]
	suffix := make([]byte, fxN)
	for i := range suffix {
		suffix[i] = base + byte(i)
	}
	probe := make([]byte, 8)
	b.ResetTimer()
	for it := 0; it < b.N; it++ {
		for q := 0; q < fxPages; q++ {
			binary.BigEndian.PutUint64(probe, 1<<63+1000000+uint64((q*31)%fxN))
			lo, hi := 0, fxN
			if c := bytes.Compare(probe[:7], prefix); c < 0 {
				hi = 0
			} else if c > 0 {
				lo = fxN
			}
			t := probe[7]
			for lo < hi {
				m := (lo + hi) / 2
				if suffix[m] < t {
					lo = m + 1
				} else {
					hi = m
				}
			}
			fxSink += uint64(lo)
		}
	}
}

// --- write side: what does choosing an encoding cost per leaf? ---------------------------------------

// One leaf's worth of work for a 6-fixed-column table: today a copy; a candidate adds a min/max pass
// and a narrowing pack per column.
func BenchmarkFormatExpEncodePlain(b *testing.B) {
	plain, _, _, _ := fxInt64Pages(200)
	out := make([]byte, 0, 8192)
	b.ResetTimer()
	for it := 0; it < b.N; it++ {
		for _, pg := range plain[:1000] {
			out = out[:0]
			for c := 0; c < 6; c++ {
				out = append(out, pg...)
			}
		}
	}
	fxSink += uint64(len(out))
}

func BenchmarkFormatExpEncodeFor(b *testing.B) {
	plain, _, _, _ := fxInt64Pages(200)
	out := make([]byte, 0, 8192)
	b.ResetTimer()
	for it := 0; it < b.N; it++ {
		for _, pg := range plain[:1000] {
			out = out[:0]
			for c := 0; c < 6; c++ {
				lo, hi := ^uint64(0), uint64(0)
				for i := 0; i+8 <= len(pg); i += 8 {
					v := binary.BigEndian.Uint64(pg[i:])
					lo, hi = min(lo, v), max(hi, v)
				}
				out = binary.BigEndian.AppendUint64(out, lo)
				switch w := hi - lo; {
				case w < 1<<8:
					for i := 0; i+8 <= len(pg); i += 8 {
						out = append(out, byte(binary.BigEndian.Uint64(pg[i:])-lo))
					}
				case w < 1<<16:
					for i := 0; i+8 <= len(pg); i += 8 {
						out = binary.LittleEndian.AppendUint16(out, uint16(binary.BigEndian.Uint64(pg[i:])-lo))
					}
				default:
					out = append(out, pg...)
				}
			}
		}
	}
	fxSink += uint64(len(out))
}
