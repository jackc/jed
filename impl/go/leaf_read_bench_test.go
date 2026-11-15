package jed

import (
	"fmt"
	"math/rand"
	"testing"
)

// In-place reads of a base-plus-offsets column (spec/design/leaf-write-cost.md §4): the cost of
// reading row i straight from each region kind's bytes, against building the full-width copy every
// core builds today and reading that. Run with
//
//	go test -run '^$' -bench BenchmarkLeafColumnRead -benchtime=200000x .

// readCase is one column of a leaf: its values and the region bytes of a kind.
type readCase struct {
	name    string
	w, n    int
	vb      [][]byte
	nulls   []bool
	region  []byte // after the kind byte
	kind    byte
	runsIdx int // leafRuns mode the region was written in
	blocks  int // leafBlocks the region was written in
}

// leafReadValues is a column's values: a drifting column (row × 7), small values with one outlier,
// and uniform random values in 0 … 99,999.
func leafReadValues(pattern string, n int) []uint64 {
	vals := make([]uint64, n)
	r := rand.New(rand.NewSource(7))
	for i := range vals {
		switch pattern {
		case "drift":
			vals[i] = uint64(1<<63) + uint64(100000+i*7)
		case "outlier":
			vals[i] = uint64(1<<63) + uint64(i%50)
			if i == n/2 {
				vals[i] = uint64(1<<63) + 9000000000
			}
		case "random":
			vals[i] = uint64(1<<63) + uint64(r.Intn(100000))
		}
	}
	return vals
}

func leafReadCases(b *testing.B) []readCase {
	const n, w = 476, 8
	var out []readCase
	savedRuns, savedFORX, savedBlocks, savedExc := leafRuns, leafFORX, leafBlocks, leafBlocksExc
	defer func() { leafRuns, leafFORX, leafBlocks, leafBlocksExc = savedRuns, savedFORX, savedBlocks, savedExc }()
	for _, pattern := range []string{"drift", "outlier", "random"} {
		vals := leafReadValues(pattern, n)
		vb := make([][]byte, n)
		nulls := make([]bool, n)
		for i, v := range vals {
			vb[i] = make([]byte, w)
			putBEUint(vb[i], v)
		}
		add := func(name string, runs int, forx bool, force byte, blocks int, exc bool) {
			leafRuns, leafFORX, leafBlocks, leafBlocksExc = runs, forx, blocks, exc
			r := newRunsRange(w)
			for _, v := range vals {
				r.add(v)
			}
			var reg []byte
			switch force {
			case regionFORX:
				if _, _, ok := forExc(w, r); !ok {
					return
				}
				reg = encodeFORX(w, n, vb, nulls, r)
			case regionRuns:
				reg = encodeRuns(w, n, vb, nulls, r.runs())
			case regionBlocks:
				reg = encodeBlocks(w, n, vb, nulls, r.blocks())
			default:
				reg = encodeRegion(colType{Scalar: scalarInt64}, n, vb, nulls)
			}
			out = append(out, readCase{
				name: pattern + "/" + name, w: w, n: n, vb: vb, nulls: nulls,
				region: reg[1:], kind: reg[0], runsIdx: runs, blocks: blocks,
			})
		}
		add("kind1", leafRunsOff, false, 0, 0, false)
		add("exception", leafRunsOff, true, regionFORX, 0, false)
		add("runs-search", leafRunsSearch, false, regionRuns, 0, false)
		add("runs-rank", leafRunsRank, false, regionRuns, 0, false)
		add("blocks64", leafRunsOff, false, regionBlocks, 64, false)
		add("blocks64-exc", leafRunsOff, false, regionBlocks, 64, true)
		add("blocks32-exc", leafRunsOff, false, regionBlocks, 32, true)
	}
	return out
}

// inPlace returns a reader of row i straight from the region bytes.
func (c readCase) inPlace() func(i int) uint64 {
	n, w, reg := c.n, c.w, c.region
	bm := (n + 7) / 8
	switch c.kind {
	case 0:
		body := reg[bm:]
		return func(i int) uint64 { return beUintN(body[i*w:], w) }
	case regionFOR:
		base := beUint(reg[bm : bm+w])
		bw := int(reg[bm+w])
		offs := reg[bm+w+1:]
		if bw == 0 {
			return func(int) uint64 { return base }
		}
		return func(i int) uint64 { return base + beUintN(offs[i*bw:], bw) }
	case regionFORX:
		p := bm
		base := beUint(reg[p : p+w])
		bw := int(reg[p+w])
		exc := int(reg[p+w+1])<<8 | int(reg[p+w+2])
		excV := beUint(reg[p+w+3 : p+2*w+3])
		offs := reg[p+2*w+3:]
		return func(i int) uint64 {
			if i == exc {
				return excV
			}
			if bw == 0 {
				return base
			}
			return base + beUintN(offs[i*bw:], bw)
		}
	case regionRuns:
		saved := leafRuns
		leafRuns = c.runsIdx
		rr, err := parseRunsRegion(reg, 0, len(reg), n, w)
		leafRuns = saved
		if err != nil {
			panic(err)
		}
		return func(i int) uint64 { v, _ := rr.value(i); return v }
	case regionBlocks:
		saved := leafBlocks
		leafBlocks = c.blocks
		br, err := parseBlocksRegion(reg, 0, len(reg), n, w)
		leafBlocks = saved
		if err != nil {
			panic(err)
		}
		return func(i int) uint64 { v, _ := br.value(i); return v }
	}
	panic(fmt.Sprintf("kind %d", c.kind))
}

// dense builds the full-width copy of the column, as the readers do today.
func (c readCase) dense() []byte {
	body := make([]byte, c.n*c.w)
	read := c.inPlace()
	for i := 0; i < c.n; i++ {
		putBEUint(body[i*c.w:(i+1)*c.w], read(i))
	}
	return body
}

func BenchmarkLeafColumnRead(b *testing.B) {
	for _, c := range leafReadCases(b) {
		read := c.inPlace()
		for i := 0; i < c.n; i++ {
			if read(i) != beUint(c.vb[i]) {
				b.Fatalf("%s: row %d reads %d, want %d", c.name, i, read(i), beUint(c.vb[i]))
			}
		}
		size := 1 + len(c.region)
		order := rand.New(rand.NewSource(1)).Perm(c.n)
		b.Run(c.name+"/point", func(b *testing.B) {
			b.ReportMetric(float64(size), "region-bytes")
			var sink uint64
			for k := 0; k < b.N; k++ {
				sink += read(order[k%c.n])
			}
			_ = sink
		})
		b.Run(c.name+"/scan", func(b *testing.B) {
			var sink uint64
			for k := 0; k < b.N; k++ {
				for i := 0; i < c.n; i++ {
					sink += read(i)
				}
			}
			_ = sink
		})
		if c.kind == regionRuns {
			// A scan walks run by run, with no lookup per row.
			saved := leafRuns
			leafRuns = c.runsIdx
			rr, _ := parseRunsRegion(c.region, 0, len(c.region), c.n, c.w)
			leafRuns = saved
			b.Run(c.name+"/scan-by-run", func(b *testing.B) {
				var sink uint64
				for k := 0; k < b.N; k++ {
					for r := 0; r < rr.r; r++ {
						s, o, bw := rr.run(r)
						hi := c.n
						if r+1 < rr.r {
							hi, _, _ = rr.run(r + 1)
						}
						base := beUint(rr.hdr[r*(c.w+5)+4 : r*(c.w+5)+4+c.w])
						if bw == 0 {
							sink += base * uint64(hi-s)
							continue
						}
						d := rr.data[o : o+(hi-s)*bw]
						for i := 0; i < hi-s; i++ {
							sink += base + beUint(d[i*bw:(i+1)*bw])
						}
					}
				}
				_ = sink
			})
		}
		b.Run(c.name+"/build-copy", func(b *testing.B) {
			for k := 0; k < b.N; k++ {
				_ = c.dense()
			}
		})
	}
	// Reading the full-width copy, once built: what every kind costs after conversion.
	c := leafReadCases(b)[0]
	body := c.dense()
	order := rand.New(rand.NewSource(1)).Perm(c.n)
	b.Run("copy/point", func(b *testing.B) {
		var sink uint64
		for k := 0; k < b.N; k++ {
			i := order[k%c.n]
			sink += beUintN(body[i*c.w:], c.w)
		}
		_ = sink
	})
	b.Run("copy/scan", func(b *testing.B) {
		var sink uint64
		for k := 0; k < b.N; k++ {
			for i := 0; i < c.n; i++ {
				sink += beUintN(body[i*c.w:], c.w)
			}
		}
		_ = sink
	})
}
