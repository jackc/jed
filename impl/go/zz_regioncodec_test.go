package jed

// EXPERIMENT (not part of the core's test contract): which codec suits a variable-width leaf column
// region, and how does page size change the answer? Builds the format experiment's database plus two
// tables of harder data (prose, varied-key jsonb, arrays), and for every leaf region measures:
//
//   - fsst      a symbol table in the manner of FSST (Boncz, Neumann, Leis 2020): up to 255 symbols of
//               1-8 bytes, code 255 escapes one literal byte. Every value decodes on its own.
//   - symtab    the same construction with literal bytes standing for themselves and symbols taking
//               byte values the region does not use, so a region too small to pay for single-byte
//               symbols still gains. Every value decodes on its own.
//   - huf       order-0 canonical Huffman over the bodies, each value byte-aligned (decodes on its own).
//   - lz4+huf   the core's LZ4 block, then order-0 Huffman over the block.
//   - arr-*     integer arrays only: byte-shuffled elements under LZ4, and the elements as a minimum
//               plus narrow offsets (byte-aligned or bit-packed) with a one-byte count per value.
//
// against plain, the 256-entry dictionary, LZ4 and DEFLATE from zz_colcodec_test.go.
//
//	JED_FORMAT_EXP=1 JED_REGION_PAGES=4096,8192,16384,32768 go test -run TestRegionCodecExperiment -v .

import (
	"encoding/binary"
	"fmt"
	"math/rand"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"testing"
	"time"
)

// ---- harder test data ------------------------------------------------------------------------------------

// expProse returns lines of real prose: the design documents' own text, one value per line.
func expProse(t testing.TB) (lines, words []string) {
	files, _ := filepath.Glob("../../spec/design/*.md")
	sort.Strings(files)
	seen := map[string]bool{}
	for _, f := range files {
		b, err := os.ReadFile(f)
		if err != nil {
			t.Fatal(err)
		}
		for _, ln := range strings.Split(string(b), "\n") {
			ln = strings.TrimSpace(ln)
			if len(ln) < 24 || strings.ContainsAny(ln, "'\\\x00") {
				continue
			}
			if len(ln) > 160 {
				ln = ln[:160]
				for len(ln) > 0 && ln[len(ln)-1] >= 0x80 { // do not cut a UTF-8 sequence
					ln = ln[:len(ln)-1]
				}
			}
			lines = append(lines, ln)
			for _, w := range strings.Fields(strings.ToLower(ln)) {
				w = strings.Trim(w, ".,;:()`*\"[]{}<>!?|#=-+/")
				if len(w) < 3 || len(w) > 12 || seen[w] || strings.IndexFunc(w, func(r rune) bool { return r < 'a' || r > 'z' }) >= 0 {
					continue
				}
				seen[w] = true
				words = append(words, w)
			}
		}
	}
	return lines, words
}

// expExtra adds two tables whose variable-width columns are not drawn from small template sets.
func expExtra(t testing.TB, db *Database) {
	s := db.Session(SessionOptions{})
	defer s.Close()
	lines, words := expProse(t)
	if len(lines) < 10000 || len(words) < 2000 {
		t.Fatalf("prose source too small: %d lines, %d words", len(lines), len(words))
	}
	r := rand.New(rand.NewSource(11))
	keys := words[:80]
	resources := words[100:140]

	// activity: prose, short unique titles, structured paths, and jsonb whose key set varies per row.
	expExec(t, s, `CREATE TABLE activity (id i64 PRIMARY KEY, title text NOT NULL, path text NOT NULL,
		message text NOT NULL, attrs jsonb NOT NULL)`)
	expInsert(t, s, "activity", expScale(120000), 500, func(i int, b *strings.Builder) {
		title := words[r.Intn(len(words))] + " " + words[r.Intn(len(words))]
		if r.Intn(2) == 0 {
			title += " " + words[r.Intn(len(words))]
		}
		path := fmt.Sprintf("/api/v%d/%s/%d/%s?page=%d&sort=%s", 1+r.Intn(3), resources[r.Intn(len(resources))],
			r.Intn(5000000), resources[r.Intn(len(resources))], 1+r.Intn(40), words[r.Intn(60)])
		var j strings.Builder
		j.WriteByte('{')
		perm := r.Perm(len(keys))
		for k, n := 0, 4+r.Intn(9); k < n; k++ {
			if k > 0 {
				j.WriteByte(',')
			}
			fmt.Fprintf(&j, `"%s":`, keys[perm[k]])
			switch r.Intn(6) {
			case 0:
				fmt.Fprintf(&j, "%d", r.Intn(100000))
			case 1:
				fmt.Fprintf(&j, "%v", r.Intn(2) == 0)
			case 2:
				fmt.Fprintf(&j, "%d.%02d", r.Intn(1000), r.Intn(100))
			case 3:
				fmt.Fprintf(&j, `{"%s":%d,"%s":"%s"}`, keys[r.Intn(len(keys))], r.Intn(500), keys[r.Intn(len(keys))], words[r.Intn(len(words))])
			case 4:
				fmt.Fprintf(&j, `["%s","%s"]`, words[r.Intn(len(words))], words[r.Intn(len(words))])
			default:
				fmt.Fprintf(&j, `"%s"`, words[r.Intn(len(words))])
			}
		}
		j.WriteByte('}')
		fmt.Fprintf(b, "%d,'%s','%s','%s','%s'", i+1, title, path, lines[r.Intn(len(lines))], j.String())
	})

	// samples: arrays. tags is a short text array from a 300-word pool; scores are small integers;
	// refs are sorted ids from a 10M space (a list of foreign keys); readings use the full i32 range.
	expExec(t, s, `CREATE TABLE samples (id i64 PRIMARY KEY, tags text[] NOT NULL, scores i32[] NOT NULL,
		refs i64[] NOT NULL, readings i32[] NOT NULL)`)
	pool := words[200:500]
	ints := func(b *strings.Builder, n int, f func(k int) int64) {
		b.WriteString("'{")
		for k := 0; k < n; k++ {
			if k > 0 {
				b.WriteByte(',')
			}
			fmt.Fprintf(b, "%d", f(k))
		}
		b.WriteString("}'")
	}
	expInsert(t, s, "samples", expScale(150000), 500, func(i int, b *strings.Builder) {
		fmt.Fprintf(b, "%d,'{", i+1)
		for k, n := 0, 1+r.Intn(6); k < n; k++ {
			if k > 0 {
				b.WriteByte(',')
			}
			b.WriteString(pool[r.Intn(len(pool))])
		}
		b.WriteString("}',")
		ints(b, 5+r.Intn(16), func(int) int64 { return int64(r.Intn(101)) })
		b.WriteByte(',')
		refs := make([]int64, 3+r.Intn(10))
		for k := range refs {
			refs[k] = r.Int63n(10000000)
		}
		sort.Slice(refs, func(a, c int) bool { return refs[a] < refs[c] })
		ints(b, len(refs), func(k int) int64 { return refs[k] })
		b.WriteByte(',')
		ints(b, 8+r.Intn(25), func(int) int64 { return int64(int32(r.Uint32())) })
	})
}

// ---- symbol tables (fsst, symtab) ------------------------------------------------------------------------

const stMaxLen = 8

// stUnit ids: 0..255 a literal byte, 256+k symbol k.
type stTable struct {
	syms  [][]byte
	first [256][]uint16 // symbols by first byte, longest first
}

func (tb *stTable) index() {
	for i := range tb.first {
		tb.first[i] = tb.first[i][:0]
	}
	order := make([]int, len(tb.syms))
	for i := range order {
		order[i] = i
	}
	sort.SliceStable(order, func(a, b int) bool { return len(tb.syms[order[a]]) > len(tb.syms[order[b]]) })
	for _, k := range order {
		tb.first[tb.syms[k][0]] = append(tb.first[tb.syms[k][0]], uint16(k))
	}
}

// match returns the unit at p (a symbol, else the literal byte) and its length. A symbol never runs
// past end, so every value encodes on its own.
func (tb *stTable) match(body []byte, p, end int) (unit, n int) {
	for _, k := range tb.first[body[p]] {
		s := tb.syms[k]
		if p+len(s) <= end && string(body[p:p+len(s)]) == string(s) {
			return 256 + int(k), len(s)
		}
	}
	return int(body[p]), 1
}

func (tb *stTable) bytesOf(u int) []byte {
	if u < 256 {
		return []byte{byte(u)}
	}
	return tb.syms[u-256]
}

// stBuild grows a table over five passes: parse with the current table, count units and adjacent
// pairs, and keep the candidates (units and pair concatenations) with the largest gain. classic counts
// a literal as two bytes (FSST's escape), so single bytes are candidates; otherwise a literal costs
// one byte and only longer symbols help. A candidate must repay its own table entry.
func stBuild(ends []uint16, body []byte, classic bool, maxSyms int) *stTable {
	tb := &stTable{}
	if maxSyms <= 0 {
		return tb
	}
	type cand struct {
		s    string
		gain int
	}
	for gen := 0; gen < 5; gen++ {
		tb.index()
		c1 := map[int]int{}
		c2 := map[uint32]int{}
		lo := 0
		for _, e := range ends {
			prev := -1
			for p := lo; p < int(e); {
				u, n := tb.match(body, p, int(e))
				c1[u]++
				if prev >= 0 {
					c2[uint32(prev)<<16|uint32(u)]++
				}
				prev = u
				p += n
			}
			lo = int(e)
		}
		gains := map[string]int{}
		add := func(s []byte, count int) {
			saved := len(s) - 1 // bytes saved per use against literals at one byte each
			if classic {
				saved = len(s) // against nothing: FSST ranks by bytes covered
			}
			if g := count*saved - (len(s) + 1); g > 0 && (classic || len(s) > 1) {
				if g > gains[string(s)] {
					gains[string(s)] = g
				}
			}
		}
		for u, c := range c1 {
			add(tb.bytesOf(u), c)
		}
		var cat [2 * stMaxLen]byte
		for k, c := range c2 {
			a, b := tb.bytesOf(int(k>>16)), tb.bytesOf(int(k&0xffff))
			if len(a)+len(b) > stMaxLen {
				continue
			}
			n := copy(cat[:], a)
			n += copy(cat[n:], b)
			add(cat[:n], c)
		}
		cs := make([]cand, 0, len(gains))
		for s, g := range gains {
			cs = append(cs, cand{s, g})
		}
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
	tb.index()
	return tb
}

// fsst buffer: nsym u8, nsym length bytes, the symbols, n u16 encoded ends, the encoded bodies.
func fsstEnc(ends []uint16, body []byte) ([]byte, int, bool) {
	tb := stBuild(ends, body, true, 255)
	b := []byte{byte(len(tb.syms))}
	for _, s := range tb.syms {
		b = append(b, byte(len(s)))
	}
	for _, s := range tb.syms {
		b = append(b, s...)
	}
	dir := len(b)
	b = append(b, make([]byte, 2*len(ends))...)
	base := len(b)
	lo := 0
	for i, e := range ends {
		for p := lo; p < int(e); {
			u, n := tb.match(body, p, int(e))
			if u < 256 {
				b = append(b, 255, byte(u))
			} else {
				b = append(b, byte(u-256))
			}
			p += n
		}
		lo = int(e)
		if len(b)-base > 0xffff {
			return nil, 0, false
		}
		binary.BigEndian.PutUint16(b[dir+2*i:], uint16(len(b)-base))
	}
	return b, len(b), true
}

type stDec struct {
	lens [256]uint8
	syms [256]uint64 // the symbol's bytes, little-endian, so one 8-byte store writes it
}

func stLoad(b []byte) uint64 {
	var v [8]byte
	copy(v[:], b)
	return binary.LittleEndian.Uint64(v[:])
}

// stRoom returns out with room for a value of encoded length n (a code expands to at most 8 bytes,
// and each store writes 8).
func stRoom(out []byte, n int) []byte {
	if cap(out) < 8*n+8 {
		return make([]byte, 8*n+8)
	}
	return out[:cap(out)]
}

// fsstOpen reads the table and returns the directory and encoded bodies.
func fsstOpen(b []byte, n int, d *stDec) (dir, enc []byte) {
	k := int(b[0])
	p := 1 + k
	for i := 0; i < k; i++ {
		l := b[1+i]
		d.lens[i] = l
		d.syms[i] = stLoad(b[p : p+int(l)])
		p += int(l)
	}
	return b[p : p+2*n], b[p+2*n:]
}

func fsstValue(d *stDec, enc []byte, out []byte) []byte {
	out = stRoom(out, len(enc))
	o := 0
	for p := 0; p < len(enc); p++ {
		c := enc[p]
		if c == 255 {
			p++
			out[o] = enc[p]
			o++
			continue
		}
		binary.LittleEndian.PutUint64(out[o:], d.syms[c])
		o += int(d.lens[c])
	}
	return out[:o]
}

// symtab buffer: mode u8 (0 no escape, 1 escape), the escape byte, nsym u8, per symbol its code byte
// and length, the symbols, n u16 encoded ends, the encoded bodies.
func symtabEnc(ends []uint16, body []byte) ([]byte, int, bool) {
	var freq [256]int
	for _, c := range body {
		freq[c]++
	}
	// Code values: every byte value the region does not use. With fewer than 16, the rarest used
	// values are evicted to make 16, and one code becomes the escape that carries them.
	order := make([]int, 256)
	for i := range order {
		order[i] = i
	}
	sort.SliceStable(order, func(a, b int) bool { return freq[order[a]] < freq[order[b]] })
	ncodes := 0
	for ncodes < 256 && freq[order[ncodes]] == 0 {
		ncodes++
	}
	esc, hasEsc := 0, false
	if ncodes < 16 {
		ncodes, hasEsc = 16, true
	}
	var isCode [256]bool
	for _, c := range order[:ncodes] {
		isCode[c] = true
	}
	codes := order[:ncodes]
	if hasEsc {
		esc = codes[0]
		codes = codes[1:]
	}
	tb := stBuild(ends, body, false, min(len(codes), 255))
	b := []byte{0, byte(esc), byte(len(tb.syms))}
	if hasEsc {
		b[0] = 1
	}
	for i, s := range tb.syms {
		b = append(b, byte(codes[i]), byte(len(s)))
	}
	for _, s := range tb.syms {
		b = append(b, s...)
	}
	dir := len(b)
	b = append(b, make([]byte, 2*len(ends))...)
	base := len(b)
	lo := 0
	for i, e := range ends {
		for p := lo; p < int(e); {
			u, n := tb.match(body, p, int(e))
			switch {
			case u >= 256:
				b = append(b, byte(codes[u-256]))
			case isCode[u]:
				b = append(b, byte(esc), byte(u))
			default:
				b = append(b, byte(u))
			}
			p += n
		}
		lo = int(e)
		if len(b)-base > 0xffff {
			return nil, 0, false
		}
		binary.BigEndian.PutUint16(b[dir+2*i:], uint16(len(b)-base))
	}
	return b, len(b), true
}

func symtabOpen(b []byte, n int, d *stDec) (dir, enc []byte, esc int) {
	esc = -1
	if b[0] == 1 {
		esc = int(b[1])
	}
	k := int(b[2])
	p := 3 + 2*k
	for i := 0; i < k; i++ {
		c, l := b[3+2*i], b[4+2*i]
		d.lens[c] = l
		d.syms[c] = stLoad(b[p : p+int(l)])
		p += int(l)
	}
	return b[p : p+2*n], b[p+2*n:], esc
}

func symtabValue(d *stDec, enc []byte, esc int, out []byte) []byte {
	out = stRoom(out, len(enc))
	o := 0
	for p := 0; p < len(enc); p++ {
		c := enc[p]
		if l := d.lens[c]; l != 0 {
			binary.LittleEndian.PutUint64(out[o:], d.syms[c])
			o += int(l)
		} else if int(c) == esc {
			p++
			out[o] = enc[p]
			o++
		} else {
			out[o] = c
			o++
		}
	}
	return out[:o]
}

func rcSum(v []byte) uint64 {
	if len(v) == 0 {
		return 0
	}
	return uint64(len(v)) + uint64(v[0])
}

func rcSpan(dir, enc []byte, i int) []byte {
	lo := 0
	if i > 0 {
		lo = int(binary.BigEndian.Uint16(dir[2*(i-1):]))
	}
	return enc[lo:binary.BigEndian.Uint16(dir[2*i:])]
}

// ---- order-0 Huffman ---------------------------------------------------------------------------------------

const hufMax = 11 // longest code, so one 2048-entry table decodes a symbol

// hufLengths returns a code length per byte value (0 = unused), limited to hufMax.
func hufLengths(freq *[256]int) (lens [256]uint8) {
	type node struct {
		w    int
		l, r int // children, -1 for a leaf
		sym  int
	}
	var nodes []node
	for s, f := range freq {
		if f > 0 {
			nodes = append(nodes, node{f, -1, -1, s})
		}
	}
	if len(nodes) == 1 {
		lens[nodes[0].sym] = 1
		return
	}
	sort.SliceStable(nodes, func(a, b int) bool { return nodes[a].w < nodes[b].w })
	// Two queues: the sorted leaves, and the internal nodes, which are made in nondecreasing weight.
	leaves, q1, q2 := len(nodes), 0, len(nodes)
	take := func() int {
		if q1 < leaves && (q2 >= len(nodes) || nodes[q1].w <= nodes[q2].w) {
			q1++
			return q1 - 1
		}
		q2++
		return q2 - 1
	}
	for leaves-q1+len(nodes)-q2 > 1 {
		a := take()
		b := take()
		nodes = append(nodes, node{nodes[a].w + nodes[b].w, a, b, -1})
	}
	live := []int{len(nodes) - 1}
	var walk func(i, d int)
	walk = func(i, d int) {
		if nodes[i].l < 0 {
			lens[nodes[i].sym] = uint8(d)
			return
		}
		walk(nodes[i].l, d+1)
		walk(nodes[i].r, d+1)
	}
	if len(live) == 1 {
		walk(live[0], 0)
	}
	// Limit lengths: clamp, then lengthen the shortest codes until the Kraft sum fits.
	kraft := 0
	for s := range lens {
		if lens[s] > hufMax {
			lens[s] = hufMax
		}
		if lens[s] > 0 {
			kraft += 1 << (hufMax - lens[s])
		}
	}
	for kraft > 1<<hufMax {
		best := -1
		for s := range lens {
			if lens[s] > 0 && lens[s] < hufMax && (best < 0 || lens[s] > lens[best] || (lens[s] == lens[best] && freq[s] < freq[best])) {
				best = s
			}
		}
		kraft -= 1 << (hufMax - lens[best] - 1)
		lens[best]++
	}
	return
}

func hufCodes(lens *[256]uint8) (codes [256]uint16) {
	code := 0
	for l := uint8(1); l <= hufMax; l++ {
		for s := 0; s < 256; s++ {
			if lens[s] == l {
				codes[s] = uint16(code)
				code++
			}
		}
		code <<= 1
	}
	return
}

// hufTableSize is the stored size of the code lengths: 256 nibbles, or a count and (symbol, length)
// pairs when fewer than 64 values are used.
func hufTableSize(lens *[256]uint8) int {
	used := 0
	for _, l := range lens {
		if l > 0 {
			used++
		}
	}
	return min(128, 1+2*used)
}

type hufW struct {
	b    []byte
	acc  uint64
	bits int
}

func (w *hufW) put(v uint64, n int) {
	w.acc = w.acc<<n | v
	w.bits += n
	for w.bits >= 8 {
		w.bits -= 8
		w.b = append(w.b, byte(w.acc>>w.bits))
	}
}

func (w *hufW) flush() {
	if w.bits > 0 {
		w.b = append(w.b, byte(w.acc<<(8-w.bits)))
		w.bits, w.acc = 0, 0
	}
}

// hufDecTable maps the next hufMax bits to (length<<8 | symbol).
func hufDecTable(lens []byte, tab *[1 << hufMax]uint16) {
	var l256 [256]uint8
	copy(l256[:], lens)
	codes := hufCodes(&l256)
	for s := 0; s < 256; s++ {
		l := int(l256[s])
		if l == 0 {
			continue
		}
		lo := int(codes[s]) << (hufMax - l)
		v := uint16(l)<<8 | uint16(s)
		for k := lo; k < lo+1<<(hufMax-l); k++ {
			tab[k] = v
		}
	}
}

// hufDecode appends count symbols read from enc starting at bit skip.
func hufDecode(tab *[1 << hufMax]uint16, enc []byte, skip, count int, out []byte) []byte {
	if cap(out) < count {
		out = make([]byte, count)
	}
	out = out[:count]
	o := 0
	var acc uint64
	bits, p := 0, 0
	for p < len(enc) && bits <= 56 {
		acc = acc<<8 | uint64(enc[p])
		p++
		bits += 8
	}
	bits -= skip
	for ; count > 0; count-- {
		for bits < hufMax && p < len(enc) {
			acc = acc<<8 | uint64(enc[p])
			p++
			bits += 8
		}
		var idx uint64
		if bits >= hufMax {
			idx = acc >> (bits - hufMax)
		} else {
			idx = acc << (hufMax - bits)
		}
		e := tab[idx&(1<<hufMax-1)]
		bits -= int(e >> 8)
		out[o] = byte(e)
		o++
	}
	return out
}

// huf buffer: 256 code lengths, n u16 plain ends (a value's decoded length), n u16 encoded ends, then
// the bits, each value starting on a byte.
func hufEnc(ends []uint16, body []byte) ([]byte, int, bool) {
	var freq [256]int
	for _, c := range body {
		freq[c]++
	}
	if len(body) == 0 {
		return nil, 0, false
	}
	lens := hufLengths(&freq)
	codes := hufCodes(&lens)
	b := append([]byte{}, lens[:]...)
	for _, e := range ends {
		b = binary.BigEndian.AppendUint16(b, e)
	}
	dir := len(b)
	b = append(b, make([]byte, 2*len(ends))...)
	w := hufW{}
	lo := 0
	for i, e := range ends {
		for _, c := range body[lo:e] {
			w.put(uint64(codes[c]), int(lens[c]))
		}
		w.flush()
		lo = int(e)
		binary.BigEndian.PutUint16(b[dir+2*i:], uint16(len(w.b)))
	}
	b = append(b, w.b...)
	// Stored: the code lengths, one directory of encoded ends, and a length per value. A value's
	// decoded length is charged at one byte (two where any value is 255 bytes or longer).
	lenBytes := len(ends)
	lo = 0
	for _, e := range ends {
		if int(e)-lo >= 255 {
			lenBytes = 2 * len(ends)
			break
		}
		lo = int(e)
	}
	return b, hufTableSize(&lens) + 2*len(ends) + lenBytes + len(w.b), true
}

func hufGet(tab *[1 << hufMax]uint16, b []byte, n, i int, out []byte) []byte {
	plain, dir, enc := b[256:256+2*n], b[256+2*n:256+4*n], b[256+4*n:]
	lo, plo := 0, 0
	if i > 0 {
		lo = int(binary.BigEndian.Uint16(dir[2*(i-1):]))
		plo = int(binary.BigEndian.Uint16(plain[2*(i-1):]))
	}
	return hufDecode(tab, enc[lo:binary.BigEndian.Uint16(dir[2*i:])], 0, int(binary.BigEndian.Uint16(plain[2*i:]))-plo, out)
}

// hufBlock codes one byte string: 256 code lengths, then the bits.
func hufBlock(src []byte) (buf []byte, size int) {
	var freq [256]int
	for _, c := range src {
		freq[c]++
	}
	lens := hufLengths(&freq)
	codes := hufCodes(&lens)
	w := hufW{b: append([]byte{}, lens[:]...)}
	for _, c := range src {
		w.put(uint64(codes[c]), int(lens[c]))
	}
	w.flush()
	return w.b, hufTableSize(&lens) + len(w.b) - 256
}

// ---- integer arrays ----------------------------------------------------------------------------------------

// rcArray splits a stored one-dimensional array without NULLs into its element bytes. The stored span
// is ndim u8 (1), flags u8 (0), len u32, lower bound i32 (1), then the elements at w bytes each.
func rcArray(span []byte, w int) (elems []byte, ok bool) {
	if len(span) == 2 && span[0] == 0 && span[1] == 0 {
		return nil, true // the empty array
	}
	if len(span) < 10 || span[0] != 1 || span[1] != 0 || binary.BigEndian.Uint32(span[6:]) != 1 {
		return nil, false
	}
	n := int(binary.BigEndian.Uint32(span[2:]))
	if n == 0 || len(span) != 10+n*w {
		return nil, false
	}
	return span[10:], true
}

func rcArrayRebuild(out []byte, elems []byte, w int) []byte {
	if len(elems) == 0 {
		return append(out, 0, 0)
	}
	out = append(out, 1, 0)
	out = binary.BigEndian.AppendUint32(out, uint32(len(elems)/w))
	out = binary.BigEndian.AppendUint32(out, 1)
	return append(out, elems...)
}

// rcArrayCodecs returns the integer-array codecs for elements of w bytes. tag is the bytes a stored
// span carries before the array body (the inline tag, or nothing).
func rcArrayCodecs(w int, tag []byte) []ccBytes {
	split := func(ends []uint16, body []byte) (counts []int, elems []byte, ok bool) {
		lo := 0
		for _, e := range ends {
			sp := body[lo:e]
			lo = int(e)
			if len(sp) < len(tag) || string(sp[:len(tag)]) != string(tag) {
				return nil, nil, false
			}
			el, ok := rcArray(sp[len(tag):], w)
			if !ok || len(el)/w > 255 {
				return nil, nil, false
			}
			counts = append(counts, len(el)/w)
			elems = append(elems, el...)
		}
		return counts, elems, true
	}
	// emit rebuilds every stored span from the counts and the element bytes.
	emit := func(counts []byte, elems []byte, each func(span []byte)) {
		var span []byte
		at := 0
		for _, c := range counts {
			span = append(span[:0], tag...)
			span = rcArrayRebuild(span, elems[at:at+int(c)*w], w)
			at += int(c) * w
			each(span)
		}
	}
	sum := func(dec func(b []byte, n int, each func(span []byte))) func(b []byte, n int) uint64 {
		return func(b []byte, n int) uint64 {
			var s uint64
			dec(b, n, func(span []byte) { s += rcSum(span) })
			return s
		}
	}
	// typed buffer: n count bytes, the minimum at w bytes, the offset width (bytes, or bits when
	// bitpack), then the offsets.
	typed := func(bitpack bool) ccBytes {
		name := "arr-for-bytes"
		if bitpack {
			name = "arr-for-bits"
		}
		dec := func(b []byte, n int, each func(span []byte)) {
			lo := ccBE(b[n:], w)
			bw := int(b[n+w])
			data := b[n+w+1:]
			m := 0
			for _, c := range b[:n] {
				m += int(c)
			}
			elems := make([]byte, m*w)
			for i := 0; i < m; i++ {
				var v uint64
				if bitpack {
					v = ccGet(data, i*bw, bw)
				} else {
					v = ccBE(data[i*bw:], bw)
				}
				ccPutBE(elems[i*w:], w, (v+lo)^1<<(8*w-1))
			}
			emit(b[:n], elems, each)
		}
		c := ccBytes{name: name,
			enc: func(ends []uint16, body []byte) ([]byte, int, bool) {
				counts, elems, ok := split(ends, body)
				if !ok {
					return nil, 0, false
				}
				m := len(elems) / w
				vals := make([]uint64, m)
				for i := range vals {
					vals[i] = ccBE(elems[i*w:], w) ^ 1<<(8*w-1) // order-preserving image
				}
				lo, hi := ccMinMax(vals)
				b := make([]byte, 0, len(counts)+w+1+len(elems)+8)
				for _, c := range counts {
					b = append(b, byte(c))
				}
				b = append(b, make([]byte, w)...)
				ccPutBE(b[len(b)-w:], w, lo)
				if bitpack {
					bw := 0
					for bw < 64 && (hi-lo)>>bw != 0 {
						bw++
					}
					b = append(b, byte(bw))
					at := len(b)
					b = append(b, make([]byte, (m*bw+7)/8+8)...)
					for i, v := range vals {
						ccPut(b[at:], i*bw, bw, v-lo)
					}
					return b, at + (m*bw+7)/8, true
				}
				bw := ccByteWidth(hi - lo)
				b = append(b, byte(bw))
				at := len(b)
				b = append(b, make([]byte, m*bw+8)...)
				for i, v := range vals {
					ccPutBE(b[at+i*bw:], bw, v-lo)
				}
				return b, at + m*bw, true
			},
		}
		c.visit = sum(dec)
		c.get = func(b []byte, n, i int) uint64 {
			at := 0
			for _, k := range b[:i] {
				at += int(k)
			}
			m := int(b[i])
			lo := ccBE(b[n:], w)
			bw := int(b[n+w])
			data := b[n+w+1:]
			var span [10 + 255*8 + 1]byte
			sp := append(span[:0], tag...)
			if m == 0 {
				return rcSum(append(sp, 0, 0))
			}
			sp = append(sp, 1, 0)
			sp = binary.BigEndian.AppendUint32(sp, uint32(m))
			sp = binary.BigEndian.AppendUint32(sp, 1)
			sp = sp[:len(sp)+m*w]
			el := sp[len(sp)-m*w:]
			for k := 0; k < m; k++ {
				var v uint64
				if bitpack {
					v = ccGet(data, (at+k)*bw, bw)
				} else {
					v = ccBE(data[(at+k)*bw:], bw)
				}
				ccPutBE(el[k*w:], w, (v+lo)^1<<(8*w-1))
			}
			return rcSum(sp)
		}
		c.direct = true
		rcFull[name] = dec
		return c
	}
	shufDec := func(b []byte, n int, each func(span []byte)) {
		p, _ := lz4Decompress(b[4:], int(binary.BigEndian.Uint32(b)))
		m := (len(p) - n) / w
		elems := make([]byte, m*w)
		for k := 0; k < w; k++ {
			plane := p[n+k*m:]
			for i := 0; i < m; i++ {
				elems[i*w+k] = plane[i]
			}
		}
		emit(p[:n], elems, each)
	}
	shuffle := ccBytes{name: "arr-shuf-lz4",
		enc: func(ends []uint16, body []byte) ([]byte, int, bool) {
			counts, elems, ok := split(ends, body)
			if !ok {
				return nil, 0, false
			}
			m := len(elems) / w
			p := make([]byte, 0, len(counts)+len(elems))
			for _, c := range counts {
				p = append(p, byte(c))
			}
			for k := 0; k < w; k++ {
				for i := 0; i < m; i++ {
					p = append(p, elems[i*w+k])
				}
			}
			c := lz4Compress(p)
			b := binary.BigEndian.AppendUint32(nil, uint32(len(p)))
			return append(b, c...), 2 + len(c), true
		},
		visit: sum(shufDec),
	}
	rcFull["arr-shuf-lz4"] = shufDec
	return []ccBytes{typed(false), typed(true), shuffle}
}

// rcFull decodes every value of a region, for the round-trip check.
var rcFull = map[string]func(b []byte, n int, each func(span []byte)){
	"fsst": func(b []byte, n int, each func(span []byte)) {
		var d stDec
		dir, enc := fsstOpen(b, n, &d)
		for i := 0; i < n; i++ {
			each(fsstValue(&d, rcSpan(dir, enc, i), nil))
		}
	},
	"symtab": func(b []byte, n int, each func(span []byte)) {
		var d stDec
		dir, enc, esc := symtabOpen(b, n, &d)
		for i := 0; i < n; i++ {
			each(symtabValue(&d, rcSpan(dir, enc, i), esc, nil))
		}
	},
	"huf": func(b []byte, n int, each func(span []byte)) {
		var tab [1 << hufMax]uint16
		hufDecTable(b[:256], &tab)
		for i := 0; i < n; i++ {
			each(hufGet(&tab, b, n, i, nil))
		}
	},
	"lz4+huf": func(b []byte, n int, each func(span []byte)) {
		var tab [1 << hufMax]uint16
		hufDecTable(b[8:264], &tab)
		c := hufDecode(&tab, b[264:], 0, int(binary.BigEndian.Uint32(b[4:])), nil)
		p, _ := lz4Decompress(c, int(binary.BigEndian.Uint32(b)))
		lo := 0
		for i := 0; i < n; i++ {
			hi := int(binary.BigEndian.Uint16(p[2*i:]))
			each(p[2*n+lo : 2*n+hi])
			lo = hi
		}
	},
}

// rcVerify round-trips every eleventh leaf of a column through each new codec.
func rcVerify(t *testing.T, col *ccColumn) {
	for j := 0; j < len(col.leaves); j += 11 {
		lc := &col.leaves[j]
		if lc.n == 0 {
			continue
		}
		for _, cd := range ccBytesCodecs {
			dec := rcFull[cd.name]
			if dec == nil {
				continue
			}
			b, _, ok := cd.enc(lc.ends, lc.body)
			if !ok {
				continue
			}
			i, lo := 0, 0
			dec(b, lc.n, func(span []byte) {
				if i < lc.n {
					if want := lc.body[lo:lc.ends[i]]; string(span) != string(want) {
						t.Fatalf("%s.%s %s: leaf %d value %d decodes to %q, want %q", col.tree, col.name, cd.name, j, i, span, want)
					}
					lo = int(lc.ends[i])
				}
				i++
			})
			if i != lc.n {
				t.Fatalf("%s.%s %s: leaf %d decodes %d values, want %d", col.tree, col.name, cd.name, j, i, lc.n)
			}
		}
	}
}

// ---- the general codecs, as ccBytes ------------------------------------------------------------------------

var rcBytesCodecs = []ccBytes{
	{name: "fsst", direct: true, enc: fsstEnc,
		visit: func(b []byte, n int) uint64 {
			var d stDec
			dir, enc := fsstOpen(b, n, &d)
			var s uint64
			var out [4200]byte
			for i := 0; i < n; i++ {
				s += rcSum(fsstValue(&d, rcSpan(dir, enc, i), out[:0]))
			}
			return s
		},
		get: func(b []byte, n, i int) uint64 {
			var d stDec
			dir, enc := fsstOpen(b, n, &d)
			var out [4200]byte
			return rcSum(fsstValue(&d, rcSpan(dir, enc, i), out[:0]))
		},
	},
	{name: "symtab", direct: true, enc: symtabEnc,
		visit: func(b []byte, n int) uint64 {
			var d stDec
			dir, enc, esc := symtabOpen(b, n, &d)
			var s uint64
			var out [4200]byte
			for i := 0; i < n; i++ {
				s += rcSum(symtabValue(&d, rcSpan(dir, enc, i), esc, out[:0]))
			}
			return s
		},
		get: func(b []byte, n, i int) uint64 {
			var d stDec
			dir, enc, esc := symtabOpen(b, n, &d)
			var out [4200]byte
			return rcSum(symtabValue(&d, rcSpan(dir, enc, i), esc, out[:0]))
		},
	},
	{name: "huf", direct: true, enc: hufEnc,
		visit: func(b []byte, n int) uint64 {
			var tab [1 << hufMax]uint16
			hufDecTable(b[:256], &tab)
			var s uint64
			var out [4200]byte
			for i := 0; i < n; i++ {
				s += rcSum(hufGet(&tab, b, n, i, out[:0]))
			}
			return s
		},
		get: func(b []byte, n, i int) uint64 {
			var tab [1 << hufMax]uint16
			hufDecTable(b[:256], &tab)
			var out [4200]byte
			return rcSum(hufGet(&tab, b, n, i, out[:0]))
		},
	},
	// LZ4 over the plain image, then order-0 Huffman over the LZ4 block.
	{name: "lz4+huf",
		enc: func(ends []uint16, body []byte) ([]byte, int, bool) {
			p := ccPlainImage(ends, body)
			c := lz4Compress(p)
			h, size := hufBlock(c)
			b := binary.BigEndian.AppendUint32(nil, uint32(len(p)))
			b = binary.BigEndian.AppendUint32(b, uint32(len(c)))
			return append(b, h...), 4 + size, true
		},
		visit: func(b []byte, n int) uint64 {
			var tab [1 << hufMax]uint16
			hufDecTable(b[8:264], &tab)
			c := hufDecode(&tab, b[264:], 0, int(binary.BigEndian.Uint32(b[4:])), nil)
			p, _ := lz4Decompress(c, int(binary.BigEndian.Uint32(b)))
			return ccPlainVisit(p, n)
		},
	},
}

// ---- driver --------------------------------------------------------------------------------------------------

type rcCell struct {
	bpv, all, point float64 // bytes per value, ns per value decoding the column, ns per point read
	cover           float64 // share of leaves where the codec applies
}

func TestRegionCodecExperiment(t *testing.T) {
	if os.Getenv("JED_FORMAT_EXP") == "" {
		t.Skip("set JED_FORMAT_EXP=1")
	}
	dir := os.Getenv("JED_FORMAT_EXP_DIR")
	if dir == "" {
		dir = t.TempDir()
	}
	sizes := []uint32{DefaultPageSize}
	if v := os.Getenv("JED_REGION_PAGES"); v != "" {
		sizes = nil
		for _, f := range strings.Split(v, ",") {
			var n uint32
			fmt.Sscanf(f, "%d", &n)
			sizes = append(sizes, n)
		}
	}
	base := append([]ccBytes{}, ccBytesCodecs...)
	defer func() { ccBytesCodecs = base }()

	strategies := []struct {
		name string
		set  []string
	}{
		{"plain", []string{"plain"}},
		{"direct today: + dictionary", []string{"plain", "dict8"}},
		{"direct: + symtab", []string{"plain", "dict8", "symtab"}},
		{"direct: + fsst", []string{"plain", "dict8", "fsst"}},
		{"direct: + huf", []string{"plain", "dict8", "huf"}},
		{"direct: + symtab, huf, typed arrays", []string{"plain", "dict8", "symtab", "huf", "arr-for-bytes", "arr-for-bits"}},
		{"dictionary + LZ4", []string{"plain", "dict8", "lz4"}},
		{"dictionary + LZ4 + typed arrays", []string{"plain", "dict8", "lz4", "arr-for-bytes", "arr-for-bits", "arr-shuf-lz4"}},
		{"dictionary + LZ4+Huffman + typed arrays", []string{"plain", "dict8", "lz4", "lz4+huf", "arr-for-bytes", "arr-for-bits", "arr-shuf-lz4"}},
		{"DEFLATE (reference)", []string{"plain", "flate"}},
	}
	var colOrder, codecOrder []string
	cells := map[string]map[string]map[uint32]rcCell{} // column, codec, page size
	totals := map[uint32][]int{}
	perLeafRows := map[string]map[uint32]int{}
	leafPages := map[uint32]int{}

	for _, ps := range sizes {
		path := filepath.Join(dir, fmt.Sprintf("region%d.jed", ps))
		os.Remove(path)
		os.RemoveAll(path + ".lock")
		db := expBuild(t, path, ps)
		expExtra(t, db)
		if err := db.Compact("main"); err != nil {
			t.Fatal(err)
		}
		cols, pages := ccCollect(t, db)
		db.Close()
		os.Remove(path)
		leafPages[ps] = len(pages)
		totals[ps] = make([]int, len(strategies))
		for _, col := range cols {
			if col.kind != ccKindBytes || col.w > 0 || col.rows == 0 {
				continue
			}
			ccBytesCodecs = append(append([]ccBytes{}, base...), rcBytesCodecs...)
			if w := col.elemWidth; w > 0 {
				// the inline tag, if the region keeps one
				var tag []byte
				for _, lc := range col.leaves {
					if lc.n > 0 && lc.ends[0] > 0 {
						if _, ok := rcArray(lc.body[:lc.ends[0]], w); !ok {
							tag = []byte{lc.body[0]}
						}
						break
					}
				}
				ccBytesCodecs = append(ccBytesCodecs, rcArrayCodecs(w, tag)...)
			}
			rcVerify(t, col)
			best := make([]map[int]int, len(strategies))
			for i := range best {
				best[i] = map[int]int{}
			}
			res := ccMeasure(t, col, func(leaf int, codec string, size int) {
				for i, s := range strategies {
					for _, c := range s.set {
						if c == codec {
							if cur, ok := best[i][leaf]; !ok || size < cur {
								best[i][leaf] = size
							}
						}
					}
				}
			})
			for i := range strategies {
				for _, v := range best[i] {
					totals[ps][i] += v
				}
			}
			name := col.tree + "." + col.name
			if cells[name] == nil {
				cells[name] = map[string]map[uint32]rcCell{}
				perLeafRows[name] = map[uint32]int{}
				colOrder = append(colOrder, name)
			}
			perLeafRows[name][ps] = col.rows / len(col.leaves)
			for _, r := range res {
				if cells[name][r.name] == nil {
					cells[name][r.name] = map[uint32]rcCell{}
				}
				seen := false
				for _, c := range codecOrder {
					seen = seen || c == r.name
				}
				if !seen {
					codecOrder = append(codecOrder, r.name)
				}
				rows := float64(col.rows) * float64(r.leaves) / float64(len(col.leaves))
				cells[name][r.name][ps] = rcCell{bpv: float64(r.size) / rows, all: r.allNs, point: r.getNs,
					cover: float64(r.leaves) / float64(len(col.leaves))}
			}
		}
	}

	hdr := func() {
		fmt.Printf("  %-14s", "codec")
		for _, ps := range sizes {
			fmt.Printf(" | %6dK: %6s %6s %8s", ps/1024, "B/val", "ns/val", "point ns")
		}
		fmt.Println()
	}
	fmt.Printf("\nper column and page size: bytes per value, ns per value to decode the whole column, ns to read one value of one leaf\n")
	fmt.Printf("(a codec that applies to under 99%% of leaves shows its coverage; point reads of a sequential codec decode the region)\n")
	for _, name := range colOrder {
		fmt.Printf("\n%s  rows per leaf:", name)
		for _, ps := range sizes {
			fmt.Printf(" %d", perLeafRows[name][ps])
		}
		fmt.Println()
		hdr()
		for _, codec := range codecOrder {
			byPS := cells[name][codec]
			if byPS == nil {
				continue
			}
			fmt.Printf("  %-14s", codec)
			for _, ps := range sizes {
				c, ok := byPS[ps]
				switch {
				case !ok:
					fmt.Printf(" | %8s %6s %6s %8s", "", "-", "-", "-")
				case c.cover < 0.99:
					fmt.Printf(" | %7.0f%% %6.1f %6.1f %8.0f", 100*c.cover, c.bpv, c.all, c.point)
				default:
					fmt.Printf(" | %8s %6.1f %6.1f %8.0f", "", c.bpv, c.all, c.point)
				}
			}
			fmt.Println()
		}
	}
	fmt.Printf("\nvariable-width column bytes over every leaf, each region on its smallest allowed codec (MB)\n")
	fmt.Printf("  %-44s", "strategy")
	for _, ps := range sizes {
		fmt.Printf(" %8dK", ps/1024)
	}
	fmt.Println()
	for i, s := range strategies {
		fmt.Printf("  %-44s", s.name)
		for _, ps := range sizes {
			fmt.Printf(" %9.1f", float64(totals[ps][i])/1e6)
		}
		fmt.Println()
	}
	fmt.Printf("  %-44s", "leaf pages")
	for _, ps := range sizes {
		fmt.Printf(" %9d", leafPages[ps])
	}
	fmt.Println()
}

// TestRegionEngineExperiment builds the experiment database with whatever leaf switches the process
// has, and reports load time, compacted size and read/write timings, with result checksums so two
// configurations can be compared. Gated by JED_FORMAT_EXP=1; page sizes from JED_REGION_PAGES.
//
//	JED_FORMAT_EXP=1 go test -run TestRegionEngineExperiment -v -timeout 60m .
func TestRegionEngineExperiment(t *testing.T) {
	if os.Getenv("JED_FORMAT_EXP") == "" {
		t.Skip("set JED_FORMAT_EXP=1")
	}
	dir := os.Getenv("JED_FORMAT_EXP_DIR")
	if dir == "" {
		dir = t.TempDir()
	}
	sizes := []uint32{DefaultPageSize}
	if v := os.Getenv("JED_REGION_PAGES"); v != "" {
		sizes = nil
		for _, f := range strings.Split(v, ",") {
			var n uint32
			fmt.Sscanf(f, "%d", &n)
			sizes = append(sizes, n)
		}
	}
	for _, ps := range sizes {
		path := filepath.Join(dir, fmt.Sprintf("engine%d.jed", ps))
		os.Remove(path)
		os.RemoveAll(path + ".lock")
		start := time.Now()
		db := expBuild(t, path, ps)
		core := time.Since(start)
		start = time.Now()
		expExtra(t, db)
		extra := time.Since(start)
		st, _ := os.Stat(path)
		loaded := st.Size()
		if err := db.Compact("main"); err != nil {
			t.Fatal(err)
		}
		st, _ = os.Stat(path)
		db.Close()
		fmt.Printf("\npage %d: load %.1fs + %.1fs, loaded %.1f MB, compacted %.1f MB\n", ps, core.Seconds(), extra.Seconds(),
			float64(loaded)/(1<<20), float64(st.Size())/(1<<20))
		db, err := OpenDatabase(path)
		if err != nil {
			t.Fatal(err)
		}
		s := db.Session(SessionOptions{})
		for _, q := range []string{
			"SELECT count(*), sum(id)::text FROM activity",
			"SELECT sum(length(title)), sum(length(path)) FROM activity",
			"SELECT sum(length(message)) FROM activity",
			"SELECT count(*) FROM activity WHERE message LIKE '%storage%'",
			"SELECT sum(length(attrs::text)) FROM activity",
			"SELECT sum(cardinality(scores)), sum(cardinality(refs)), sum(cardinality(readings)) FROM samples",
			"SELECT sum(scores[1]), sum(refs[2])::text, sum(readings[3]) FROM samples",
			"SELECT sum(cardinality(tags)) FROM samples",
			"SELECT count(*) FROM sessions WHERE user_agent LIKE '%Linux%'",
			"SELECT count(*) FROM orders WHERE status = 'shipped'",
		} {
			var best, first time.Duration
			var out outcome
			for i := 0; i < 5; i++ {
				st := time.Now()
				o, err := queryOutcome(s, q, nil)
				if err != nil {
					t.Fatalf("%s: %v", q, err)
				}
				d := time.Since(st)
				if i == 0 {
					first = d
				} else if i == 1 || d < best {
					best = d
				}
				out = o
			}
			fmt.Printf("  %-96s first %9v resident %9v  = %v\n", q, first.Round(time.Microsecond), best.Round(time.Microsecond), out.Rows)
		}
		r := rand.New(rand.NewSource(9))
		point := func(label, sql string, n int, arg func() int64) {
			p, err := s.Prepare(sql)
			if err != nil {
				t.Fatal(err)
			}
			for pass := 0; pass < 2; pass++ {
				st := time.Now()
				for i := 0; i < n; i++ {
					if _, err := prepOutcome(s, p, []Value{IntValue(arg())}); err != nil {
						t.Fatal(err)
					}
				}
				if pass == 1 {
					fmt.Printf("  %-96s %9v per op\n", label, (time.Since(st) / time.Duration(n)).Round(10*time.Nanosecond))
				}
			}
		}
		nAct, nSam := int64(expScale(120000)), int64(expScale(150000))
		point("point: SELECT message FROM activity WHERE id = $1", "SELECT message FROM activity WHERE id = $1", 20000, func() int64 { return 1 + r.Int63n(nAct) })
		point("point: SELECT id FROM activity WHERE id = $1", "SELECT id FROM activity WHERE id = $1", 20000, func() int64 { return 1 + r.Int63n(nAct) })
		point("point: SELECT scores FROM samples WHERE id = $1", "SELECT scores FROM samples WHERE id = $1", 20000, func() int64 { return 1 + r.Int63n(nSam) })
		w0 := expWchar()
		point("update 1 row/commit: activity.title", "UPDATE activity SET title = 'changed title text' WHERE id = $1", 2000, func() int64 { return 1 + r.Int63n(nAct) })
		fmt.Printf("  %-96s %9d\n", "  bytes written per update commit", (expWchar()-w0)/4000)
		next := nAct
		point("insert 1 row/commit: activity (append)", "INSERT INTO activity VALUES ($1, 'a new title', '/srv/new/path.txt', 'a new message of ordinary length for the log', '{\"k\": 1}')", 2000, func() int64 { next++; return next })
		s.Close()
		db.Close()
		os.Remove(path)
	}
}
