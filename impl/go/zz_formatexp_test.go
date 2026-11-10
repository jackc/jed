package jed

// EXPERIMENT (not part of the core's test contract): where do a database file's bytes go, and what
// would alternative leaf-region encodings save? Gated by JED_FORMAT_EXP=1. It builds realistic
// tables in a file database, walks every tree, and sizes each leaf region under candidate encodings.
//
//	JED_FORMAT_EXP=1 go test -run TestFormatExperiment -v -timeout 30m .

import (
	"bytes"
	"compress/flate"
	"context"
	"encoding/binary"
	"fmt"
	"math/bits"
	"math/rand"
	"os"
	"path/filepath"
	"runtime/pprof"
	"sort"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

func expExec(t testing.TB, s *Session, sql string) {
	t.Helper()
	if _, err := queryOutcome(s, sql, nil); err != nil {
		if len(sql) > 300 {
			sql = sql[:300]
		}
		t.Fatalf("%q: %v", sql, err)
	}
}

// expInsert runs multi-row INSERTs of batch rows each, one commit per statement.
func expInsert(t testing.TB, s *Session, table string, rows, batch int, row func(i int, b *strings.Builder)) {
	t.Helper()
	var b strings.Builder
	for i := 0; i < rows; {
		b.Reset()
		b.WriteString("INSERT INTO " + table + " VALUES ")
		for j := 0; j < batch && i < rows; j, i = j+1, i+1 {
			if j > 0 {
				b.WriteByte(',')
			}
			b.WriteByte('(')
			row(i, &b)
			b.WriteByte(')')
		}
		expExec(t, s, b.String())
	}
}

var expWords = strings.Fields("the quick brown fox jumps over lazy dog customer requested gift wrap " +
	"deliver after five leave at front door fragile handle with care call before delivery " +
	"expedite backorder partial shipment replacement refund pending approved address verified")

func expSentence(r *rand.Rand, minW, maxW int) string {
	n := minW + r.Intn(maxW-minW+1)
	var b strings.Builder
	for i := 0; i < n; i++ {
		if i > 0 {
			b.WriteByte(' ')
		}
		b.WriteString(expWords[r.Intn(len(expWords))])
	}
	return b.String()
}

func expUUID(hi, lo uint64) string {
	var u [16]byte
	binary.BigEndian.PutUint64(u[:8], hi)
	binary.BigEndian.PutUint64(u[8:], lo)
	return fmt.Sprintf("%x-%x-%x-%x-%x", u[0:4], u[4:6], u[6:8], u[8:10], u[10:16])
}

func expTS(base time.Time, us int64) string {
	return base.Add(time.Duration(us) * time.Microsecond).UTC().Format("2006-01-02 15:04:05.000000+00")
}

// expScale scales the row counts (JED_FORMAT_EXP_SCALE, default 1.0).
func expScale(n int) int {
	var f float64 = 1
	if v := os.Getenv("JED_FORMAT_EXP_SCALE"); v != "" {
		fmt.Sscanf(v, "%g", &f)
	}
	return int(float64(n) * f)
}

func expBuild(t testing.TB, path string, pageSize uint32) *Database {
	db, err := CreateDatabase(CreateOptions{Path: path, PageSize: pageSize, SkipFsync: true, Credential: expCredential()})
	if err != nil {
		t.Fatal(err)
	}
	s := db.Session(SessionOptions{})
	defer s.Close()
	base := time.Date(2025, 1, 1, 0, 0, 0, 0, time.UTC)

	// orders: the classic OLTP table. Serial PK, a skewed FK, enum-like text, a mostly-NULL note.
	expExec(t, s, `CREATE TABLE orders (id i64 PRIMARY KEY, customer_id i32 NOT NULL, status text NOT NULL,
		created_at timestamptz NOT NULL, amount decimal(10,2) NOT NULL, currency text NOT NULL,
		shipped boolean NOT NULL, note text)`)
	expExec(t, s, `CREATE INDEX orders_customer ON orders (customer_id)`)
	expExec(t, s, `CREATE INDEX orders_status_created ON orders (status, created_at)`)
	r := rand.New(rand.NewSource(1))
	zc := rand.NewZipf(r, 1.2, 8, 50000)
	statuses := []string{"pending", "paid", "shipped", "delivered", "cancelled", "refunded"}
	statusW := []int{5, 10, 15, 60, 7, 3}
	pick := func(w []int) int {
		x := r.Intn(100)
		for i, v := range w {
			if x < v {
				return i
			}
			x -= v
		}
		return 0
	}
	currencies := []string{"USD", "EUR", "GBP", "CAD"}
	curW := []int{80, 12, 5, 3}
	var ts int64
	expInsert(t, s, "orders", expScale(300000), 500, func(i int, b *strings.Builder) {
		ts += int64(r.Intn(4000000)) // ~2s apart
		note := "NULL"
		if r.Intn(100) < 30 {
			note = "'" + expSentence(r, 3, 12) + "'"
		}
		fmt.Fprintf(b, "%d,%d,'%s','%s',%d.%02d,'%s',%v,%s", i+1, zc.Uint64()+1, statuses[pick(statusW)],
			expTS(base, ts), 5+r.Intn(500), r.Intn(100), currencies[pick(curW)], r.Intn(100) < 75, note)
	})

	// line_items: composite PK, small ints, a formatted SKU.
	expExec(t, s, `CREATE TABLE line_items (order_id i64 NOT NULL, line_no i16 NOT NULL, product_id i32 NOT NULL,
		qty i16 NOT NULL, unit_price decimal(10,2) NOT NULL, sku text NOT NULL, PRIMARY KEY (order_id, line_no))`)
	expExec(t, s, `CREATE INDEX line_items_product ON line_items (product_id)`)
	r = rand.New(rand.NewSource(2))
	zp := rand.NewZipf(r, 1.1, 20, 20000)
	order, line := 1, 0
	expInsert(t, s, "line_items", expScale(500000), 500, func(i int, b *strings.Builder) {
		line++
		if line > 1 && r.Intn(100) < 40 {
			order, line = order+1, 1
		}
		p := zp.Uint64() + 1
		fmt.Fprintf(b, "%d,%d,%d,%d,%d.%02d,'SKU-%04d-%06d'", order, line, p, 1+r.Intn(5), 1+r.Intn(200),
			[]int{0, 49, 95, 99}[r.Intn(4)], p%37, p)
	})

	// events: time-series / telemetry. A time-ordered (v7-shaped) uuid PK, f64 reading.
	expExec(t, s, `CREATE TABLE events (id uuid PRIMARY KEY, ts timestamptz NOT NULL, device_id i32 NOT NULL,
		kind i16 NOT NULL, value f64 NOT NULL, seq i64 NOT NULL)`)
	expExec(t, s, `CREATE INDEX events_device_ts ON events (device_id, ts)`)
	r = rand.New(rand.NewSource(3))
	ts = 0
	seqs := make([]int64, 1000)
	expInsert(t, s, "events", expScale(300000), 500, func(i int, b *strings.Builder) {
		ts += int64(r.Intn(200000)) // ~100ms apart
		ms := uint64(base.UnixMilli() + ts/1000)
		dev := r.Intn(1000)
		seqs[dev]++
		fmt.Fprintf(b, "'%s','%s',%d,%d,%.2f,%d", expUUID(ms<<16|0x7000|uint64(r.Intn(4096)), 1<<63|r.Uint64()>>2),
			expTS(base, ts), dev+1, r.Intn(12), 20+r.NormFloat64()*5, seqs[dev])
	})

	// users: wide-ish text row with a jsonb settings blob and a unique email.
	expExec(t, s, `CREATE TABLE users (id i64 PRIMARY KEY, email text NOT NULL, name text NOT NULL,
		country text NOT NULL, created date NOT NULL, last_login timestamptz, settings jsonb NOT NULL)`)
	expExec(t, s, `CREATE UNIQUE INDEX users_email ON users (email)`)
	r = rand.New(rand.NewSource(4))
	first := strings.Fields("james mary robert patricia john jennifer michael linda david elizabeth william barbara richard susan joseph jessica thomas sarah charles karen")
	last := strings.Fields("smith johnson williams brown jones garcia miller davis rodriguez martinez hernandez lopez gonzalez wilson anderson thomas taylor moore jackson martin")
	domains := []string{"gmail.com", "yahoo.com", "outlook.com", "example.org", "icloud.com"}
	countries := strings.Fields("US US US US US GB GB CA DE FR AU IN BR JP MX")
	themes := []string{"light", "dark", "system"}
	expInsert(t, s, "users", expScale(100000), 500, func(i int, b *strings.Builder) {
		f, l := first[r.Intn(len(first))], last[r.Intn(len(last))]
		ll := "NULL"
		if r.Intn(100) < 80 {
			ll = "'" + expTS(base, int64(r.Intn(300*86400))*1000000) + "'"
		}
		fmt.Fprintf(b, `%d,'%s.%s%d@%s','%s %s','%s','%s',%s,'{"theme":"%s","notifications":{"email":%v,"push":%v},"locale":"en-%s","items_per_page":%d}'`,
			i+1, f, l, i, domains[r.Intn(len(domains))], strings.Title(f), strings.Title(l),
			countries[r.Intn(len(countries))], base.AddDate(0, 0, i/300).Format("2006-01-02"), ll,
			themes[r.Intn(3)], r.Intn(2) == 0, r.Intn(2) == 0, countries[r.Intn(len(countries))], []int{10, 25, 50, 100}[r.Intn(4)])
	})

	// sessions: random (v4) uuid PK — random-position inserts — and long low-cardinality strings.
	expExec(t, s, `CREATE TABLE sessions (token uuid PRIMARY KEY, user_id i64 NOT NULL, expires timestamptz NOT NULL,
		ip text NOT NULL, user_agent text NOT NULL)`)
	r = rand.New(rand.NewSource(5))
	uas := make([]string, 40)
	for i := range uas {
		uas[i] = fmt.Sprintf("Mozilla/5.0 (%s) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/%d.0.0.0 Safari/537.36",
			[]string{"Windows NT 10.0; Win64; x64", "Macintosh; Intel Mac OS X 10_15_7", "X11; Linux x86_64", "iPhone; CPU iPhone OS 17_5 like Mac OS X"}[i%4], 110+i/4)
	}
	expInsert(t, s, "sessions", expScale(100000), 500, func(i int, b *strings.Builder) {
		fmt.Fprintf(b, "'%s',%d,'%s','%d.%d.%d.%d','%s'", expUUID(r.Uint64(), r.Uint64()), 1+r.Intn(100000),
			expTS(base, int64(300*86400+r.Intn(30*86400))*1000000), 10+r.Intn(200), r.Intn(256), r.Intn(256), r.Intn(256),
			uas[int(r.ExpFloat64()*6)%len(uas)])
	})
	return db
}

// --- sizing --------------------------------------------------------------------------------------

// expSizes is one region's (or tree's) bytes under each encoding tier.
type expSizes struct {
	cur int // today's bytes
	t1  int // structural: u16/elided directories, no redundant tag/length, no PK value columns
	t2  int // + byte-aligned, random-access page-local encodings (prefix, FOR, dictionary)
	t3  int // + bit-packing / delta / LZ4 of a value blob (no O(1) access without unpacking)
	how map[string]int
}

func (a *expSizes) add(b expSizes) {
	a.cur, a.t1, a.t2, a.t3 = a.cur+b.cur, a.t1+b.t1, a.t2+b.t2, a.t3+b.t3
}

func (a *expSizes) chose(name string) {
	if a.how == nil {
		a.how = map[string]int{}
	}
	a.how[name]++
}

func expByteWidth(rng uint64) int {
	switch {
	case rng == 0:
		return 0
	case rng < 1<<8:
		return 1
	case rng < 1<<16:
		return 2
	case rng < 1<<32:
		return 4
	}
	return 8
}

func expLCP(a, b []byte) int {
	n := 0
	for n < len(a) && n < len(b) && a[n] == b[n] {
		n++
	}
	return n
}

func expBE(b []byte) uint64 {
	var v uint64
	for _, x := range b {
		v = v<<8 | uint64(x)
	}
	return v
}

// expInts sizes n unsigned values (nulls excluded from vals) of natural width w.
func expInts(vals []uint64, n, w, bm int) (t2, t3 int, how string) {
	plain := 1 + bm + n*w
	t2, how = plain, "plain"
	if len(vals) == 0 {
		return 2 + bm, 2 + bm, "allnull"
	}
	lo, hi := vals[0], vals[0]
	distinct := map[uint64]struct{}{}
	for _, v := range vals {
		if v < lo {
			lo = v
		}
		if v > hi {
			hi = v
		}
		if len(distinct) <= 256 {
			distinct[v] = struct{}{}
		}
	}
	bw := expByteWidth(hi - lo)
	if bw > w {
		bw = w
	}
	if s := 2 + w + bm + n*bw; s < t2 {
		t2, how = s, fmt.Sprintf("for%d", bw)
		if bw == 0 {
			how = "const"
		}
	}
	d := len(distinct)
	if d <= 256 {
		if s := 3 + d*w + bm + n; s < t2 {
			t2, how = s, "dict8"
		}
	}
	t3 = t2
	if s := 3 + w + bm + (n*bits.Len64(hi-lo)+7)/8; s < t3 {
		t3 = s
	}
	if d <= 256 && d > 0 {
		if s := 3 + d*w + bm + (n*bits.Len(uint(d-1))+7)/8; s < t3 {
			t3 = s
		}
	}
	if len(vals) > 1 {
		dlo, dhi := int64(vals[1]-vals[0]), int64(vals[1]-vals[0])
		for i := 2; i < len(vals); i++ {
			dd := int64(vals[i] - vals[i-1])
			if dd < dlo {
				dlo = dd
			}
			if dd > dhi {
				dhi = dd
			}
		}
		if s := 3 + w + 8 + bm + ((n-1)*bits.Len64(uint64(dhi-dlo))+7)/8; s < t3 {
			t3 = s
		}
	}
	return t2, t3, how
}

func expFixed(r *paxRegion, n int) expSizes {
	w := r.width
	out := expSizes{cur: 1 + len(r.bitmap) + len(r.body)}
	out.t1 = out.cur
	bm := 0
	for _, b := range r.bitmap {
		if b != 0 {
			bm = len(r.bitmap)
			break
		}
	}
	null := func(i int) bool { return r.bitmap[i/8]&(0x80>>(i%8)) != 0 }
	if w <= 8 {
		vals := make([]uint64, 0, n)
		for i := 0; i < n; i++ {
			if !null(i) {
				vals = append(vals, expBE(r.body[i*w:(i+1)*w]))
			}
		}
		var how string
		out.t2, out.t3, how = expInts(vals, n, w, bm)
		out.chose(how)
		return out
	}
	// 16-byte bodies (uuid, interval): a shared prefix, or a small dictionary.
	var first, lastv []byte
	distinct := map[string]struct{}{}
	p := w
	for i := 0; i < n; i++ {
		if null(i) {
			continue
		}
		v := r.body[i*w : (i+1)*w]
		if first == nil {
			first = v
		}
		if l := expLCP(first, v); l < p {
			p = l
		}
		lastv = v
		if len(distinct) <= 256 {
			distinct[string(v)] = struct{}{}
		}
	}
	_ = lastv
	out.t2 = 1 + bm + n*w
	how := "plain"
	if s := 3 + p + bm + n*(w-p); s < out.t2 {
		out.t2, how = s, fmt.Sprintf("prefix%d", p)
	}
	if d := len(distinct); d <= 256 {
		if s := 3 + d*w + bm + n; s < out.t2 {
			out.t2, how = s, "dict8"
		}
	}
	out.chose(how)
	out.t3 = out.t2
	return out
}

// expDecimalInt reads an inline-plain decimal body as (scale, unscaled) when it fits an i64.
func expDecimalInt(body []byte) (scale int, v int64, ok bool) {
	if len(body) < 5 {
		return 0, 0, false
	}
	neg := body[0]&1 != 0
	scale = int(binary.BigEndian.Uint16(body[1:]))
	nd := int(binary.BigEndian.Uint16(body[3:]))
	if nd > 4 || len(body) != 5+2*nd {
		return 0, 0, false
	}
	for i := 0; i < nd; i++ {
		v = v*10000 + int64(binary.BigEndian.Uint16(body[5+2*i:]))
	}
	if neg {
		v = -v
	}
	return scale, v, true
}

func expVariable(r *paxRegion, n int, ty colType) expSizes {
	out := expSizes{cur: 1 + len(r.ends) + len(r.body)}
	lenPrefixed := !ty.Composite && ty.Elem == nil && ty.RangeElem == nil && (ty.Scalar == scalarText || ty.Scalar == scalarBytea)
	isDecimal := !ty.Composite && ty.Elem == nil && ty.RangeElem == nil && ty.Scalar == scalarDecimal
	spans := make([][]byte, n)
	nulls, allPlain, content := 0, true, 0
	lo := uint32(0)
	for i := 0; i < n; i++ {
		hi := paxDirEnd(r.ends, i, expDW)
		sp := r.body[lo:hi]
		lo = hi
		if len(sp) == 0 {
			nulls++
			continue
		}
		if sp[0] != 0 {
			allPlain = false
		}
		spans[i] = sp
	}
	strip := 0
	if allPlain {
		strip = 1
		if lenPrefixed {
			strip = 3
		}
	}
	payload := func(sp []byte) []byte { return sp[strip:] }
	for _, sp := range spans {
		if sp != nil {
			content += len(sp) - strip
		}
	}
	out.t1 = 1 + 2*n + content
	expA["value directories: u32 -> u16"] += 2 * n
	if allPlain {
		expA["variable values: presence tag (pages with only plain values)"] += n - nulls
		if lenPrefixed {
			expA["text/bytea: u16 length prefix"] += 2 * (n - nulls)
		}
	} else if lenPrefixed {
		expA["text/bytea: u16 length prefix (pages with a non-plain value, not counted in the 23%)"] += 2 * (n - nulls)
	}
	out.t2 = out.t1
	how := "plain"
	bm := 0
	if nulls > 0 {
		bm = (n + 7) / 8
	}
	// dictionary: u8 codes + a u16-directory dictionary blob.
	distinct := map[string]struct{}{}
	dbytes := 0
	for _, sp := range spans {
		if sp == nil || len(distinct) > 256 {
			continue
		}
		k := string(payload(sp))
		if _, ok := distinct[k]; !ok {
			distinct[k] = struct{}{}
			dbytes += len(k)
		}
	}
	d := len(distinct)
	if d <= 256 && allPlain {
		if s := 3 + 2*d + dbytes + bm + n; s < out.t2 {
			out.t2, how = s, "dict8"
		}
	}
	// decimal → one scale + FOR integers.
	if isDecimal && allPlain && n > nulls {
		vals := make([]uint64, 0, n)
		scale, ok := -1, true
		for _, sp := range spans {
			if sp == nil {
				continue
			}
			sc, v, fits := expDecimalInt(sp[1:])
			if !fits || (scale >= 0 && sc != scale) {
				ok = false
				break
			}
			scale = sc
			vals = append(vals, uint64(v)^(1<<63))
		}
		if ok {
			f2, f3, h := expInts(vals, n, 8, bm)
			if f2+2 < out.t2 {
				out.t2, how = f2+2, "dec-"+h
			}
			if f3+2 < out.t3 || out.t3 == 0 {
				out.t3 = f3 + 2
			}
		}
	}
	out.chose(how)
	if out.t3 == 0 || out.t3 > out.t2 {
		out.t3 = out.t2
	}
	if d <= 256 && d > 0 && allPlain {
		if s := 3 + 2*d + dbytes + bm + (n*bits.Len(uint(d-1))+7)/8; s < out.t3 {
			out.t3 = s
		}
	}
	if allPlain && content > 64 {
		blob := make([]byte, 0, content)
		for _, sp := range spans {
			if sp != nil {
				blob = append(blob, payload(sp)...)
			}
		}
		if s := 5 + 2*n + len(lz4Compress(blob)); s < out.t3 {
			out.t3 = s
		}
	}
	return out
}

// expKeys sizes the key directory + blob. fixedKey: every key in the tree has one length (a
// fixed-width key schema needs no directory).
func expKeys(l *paxLeaf, n int, fixedKey bool) expSizes {
	out := expSizes{cur: len(l.keyEnd) + len(l.keyBlob)}
	dir := 2 * n
	if fixedKey {
		dir = 0
	}
	out.t1 = dir + len(l.keyBlob)
	if fixedKey {
		expA["key directory: elided, fixed-length keys"] += len(l.keyEnd)
	} else {
		expA["key directory: u32 -> u16"] += len(l.keyEnd) - dir
	}
	expA["(leaf bytes today: keys)"] += out.cur
	p := 0
	if n > 1 {
		p = expLCP(l.key(0), l.key(n-1))
	} else if n == 1 {
		p = 0
	}
	out.t2 = dir + 2 + p + len(l.keyBlob) - n*p
	if out.t2 > out.t1 {
		out.t2 = out.t1
	}
	out.t3 = out.t2
	// front coding with a restart every 16 keys (binary search over restarts, then a short scan).
	if n > 1 {
		per := 2
		if fixedKey {
			per = 1
		}
		fc := 2 * ((n + 15) / 16)
		for i := 0; i < n; i++ {
			k := l.key(i)
			if i%16 == 0 {
				fc += per - 1 + len(k)
			} else {
				fc += per + len(k) - expLCP(l.key(i-1), k)
			}
		}
		if fc < out.t3 {
			out.t3 = fc
			out.chose("frontcoded")
		} else {
			out.chose("prefix")
		}
	}
	if fixedKey && n > 1 {
		kl := len(l.key(0))
		if kl-p <= 8 {
			vals := make([]uint64, n)
			for i := range vals {
				vals[i] = expBE(l.key(i)[p:])
			}
			_, f3, _ := expInts(vals, n, kl-p, 0)
			if s := 2 + p + f3; s < out.t3 {
				out.t3 = s
			}
		}
	}
	return out
}

type expTree struct {
	name            string
	kind            string
	rows            int
	leaves, inner   int
	capBytes        int // leaves × C
	keys            expSizes
	colDir          expSizes
	cols            []expSizes
	colNames        []string
	lz4Page, zPage  int // whole used payload compressed, per page, summed
	overflowPointer int
}

func (tr *expTree) total() expSizes {
	var s expSizes
	s.add(tr.keys)
	s.add(tr.colDir)
	for _, c := range tr.cols {
		s.add(c)
	}
	return s
}

type expWalker struct {
	paging *sharedPaging
	pcap   pageCap
	t      testing.TB
}

func (w *expWalker) eachLeaf(ref pageRef, colTypes []colType, tr *expTree, visit func(n int, payload []byte, leaf *paxLeaf)) {
	block, err := w.paging.readBody(ref)
	if err != nil {
		w.t.Fatal(err)
	}
	pg, err := parsePage(block)
	if err != nil {
		w.t.Fatal(err)
	}
	switch pg.pageType {
	case pageInterior:
		tr.inner++
		children, _, err := parseIntegrityInterior(pg, w.pcap)
		if err != nil {
			w.t.Fatal(err)
		}
		for _, c := range children {
			w.eachLeaf(c, colTypes, tr, visit)
		}
	case pageLeaf:
		n := int(pg.itemCount)
		leaf, err := parsePaxLeaf(pg.payload, n, colTypes)
		if err != nil {
			w.t.Fatal(err)
		}
		visit(n, pg.payload, leaf)
	}
}

func (w *expWalker) tree(name, kind string, root *pnode, colTypes []colType, colNames []string, pkCols map[int]bool) *expTree {
	tr := &expTree{name: name, kind: kind, cols: make([]expSizes, len(colTypes)), colNames: colNames}
	if root == nil {
		return tr
	}
	// pass 1: is the key fixed-width across the tree?
	keyLen, fixedKey := -1, true
	w.eachLeaf(root.ref(), colTypes, &expTree{}, func(n int, _ []byte, leaf *paxLeaf) {
		for i := 0; i < n; i++ {
			if l := len(leaf.key(i)); keyLen < 0 {
				keyLen = l
			} else if l != keyLen {
				fixedKey = false
			}
		}
	})
	var zbuf bytes.Buffer
	w.eachLeaf(root.ref(), colTypes, tr, func(n int, payload []byte, leaf *paxLeaf) {
		expDW = leaf.dw
		tr.leaves++
		tr.rows += n
		tr.capBytes += len(payload)
		tr.keys.add(expKeys(leaf, n, fixedKey))
		k := len(colTypes)
		cd := leaf.dw * (k + 1)
		tr.colDir.add(expSizes{cur: cd, t1: cd / 2, t2: cd / 2, t3: cd / 2})
		expA["column directory: u32 -> u16"] += cd / 2
		expA["(leaf bytes today: all)"] += len(leaf.keyEnd) + len(leaf.keyBlob) + cd
		used := len(leaf.keyEnd) + len(leaf.keyBlob) + cd
		for c := range colTypes {
			r := &leaf.regions[c]
			var s expSizes
			if r.elided {
				// not stored: rebuilt from the keys
			} else if r.width > 0 {
				s = expFixed(r, n)
			} else {
				s = expVariable(r, n, colTypes[c])
			}
			used += s.cur
			expA["(leaf bytes today: all)"] += s.cur
			if pkCols[c] && r.width > 0 {
				expA["primary-key columns also stored as values"] += s.cur
				s.t1, s.t2, s.t3 = 0, 0, 0 // derivable from the key
				s.how = map[string]int{"=key": 1}
			}
			for h, v := range s.how {
				if tr.cols[c].how == nil {
					tr.cols[c].how = map[string]int{}
				}
				tr.cols[c].how[h] += v
			}
			tr.cols[c].add(s)
		}
		tr.lz4Page += len(lz4Compress(payload[:used]))
		zbuf.Reset()
		zw, _ := flate.NewWriter(&zbuf, 6)
		zw.Write(payload[:used])
		zw.Close()
		tr.zPage += zbuf.Len()
	})
	return tr
}

func expHow(m map[string]int) string {
	type kv struct {
		k string
		v int
	}
	var all []kv
	tot := 0
	for k, v := range m {
		all = append(all, kv{k, v})
		tot += v
	}
	sort.Slice(all, func(a, b int) bool { return all[a].v > all[b].v || all[a].v == all[b].v && all[a].k < all[b].k })
	var parts []string
	for i, e := range all {
		if i == 3 {
			break
		}
		parts = append(parts, fmt.Sprintf("%s %d%%", e.k, 100*e.v/tot))
	}
	return strings.Join(parts, ", ")
}

func expReport(t *testing.T, db *Database, path string) []*expTree {
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
	var trees []*expTree
	for _, name := range names {
		table, store := pin.snap.tables[name], pin.snap.stores[name]
		pk := map[int]bool{}
		for _, c := range table.PK {
			pk[c] = true
		}
		var colNames []string
		for _, c := range table.Columns {
			colNames = append(colNames, c.Name)
		}
		trees = append(trees, w.tree(name, "table", store.treeRoot(), store.colTypes, colNames, pk))
		for _, def := range table.Indexes {
			if ist := pin.snap.indexStores[def.Name]; ist != nil {
				trees = append(trees, w.tree(def.Name, "index", ist.treeRoot(), nil, nil, nil))
			}
		}
	}
	st, _ := os.Stat(path)
	var all expSizes
	var capTot, leaves, inner, lz, z int
	fmt.Printf("\nfile %s: %d bytes, %d pages of %d\n", filepath.Base(path), st.Size(), st.Size()/int64(w.pcap.c+16), w.pcap.c+16)
	fmt.Printf("\n%-26s %-5s %8s %6s %5s %5s | %7s %7s %7s %7s | %6s %6s\n", "tree", "kind", "rows", "leaves", "inner", "fill%", "cur B/r", "T1 B/r", "T2 B/r", "T3 B/r", "lz4pg", "flate")
	for _, tr := range trees {
		s := tr.total()
		if tr.rows == 0 {
			continue
		}
		rf := float64(tr.rows)
		fmt.Printf("%-26s %-5s %8d %6d %5d %5.1f | %7.1f %7.1f %7.1f %7.1f | %6.1f %6.1f\n", tr.name, tr.kind, tr.rows, tr.leaves, tr.inner,
			100*float64(s.cur)/float64(tr.capBytes), float64(s.cur)/rf, float64(s.t1)/rf, float64(s.t2)/rf, float64(s.t3)/rf,
			float64(tr.lz4Page)/rf, float64(tr.zPage)/rf)
		all.add(s)
		capTot += tr.capBytes
		leaves += tr.leaves
		inner += tr.inner
		lz += tr.lz4Page
		z += tr.zPage
	}
	pct := func(x int) float64 { return 100 * float64(x) / float64(all.cur) }
	fmt.Printf("\nall leaves: %d leaf pages (+%d interior), used payload %d bytes (fill %.1f%%)\n", leaves, inner, all.cur, 100*float64(all.cur)/float64(capTot))
	fmt.Printf("  T1 structural          %10d  %5.1f%% of current\n", all.t1, pct(all.t1))
	fmt.Printf("  T2 +random-access enc  %10d  %5.1f%%\n", all.t2, pct(all.t2))
	fmt.Printf("  T3 +bitpack/delta/lz4  %10d  %5.1f%%\n", all.t3, pct(all.t3))
	fmt.Printf("  ref: whole-page LZ4    %10d  %5.1f%%\n", lz, pct(lz))
	fmt.Printf("  ref: whole-page flate  %10d  %5.1f%%\n", z, pct(z))

	fmt.Printf("\nper-region detail (bytes per row)\n")
	for _, tr := range trees {
		if tr.rows == 0 {
			continue
		}
		rf := float64(tr.rows)
		fmt.Printf("\n%s (%s, %.0f rows/leaf)\n", tr.name, tr.kind, rf/float64(tr.leaves))
		line := func(name string, s expSizes) {
			fmt.Printf("  %-14s %7.2f %7.2f %7.2f %7.2f   %s\n", name, float64(s.cur)/rf, float64(s.t1)/rf, float64(s.t2)/rf, float64(s.t3)/rf, expHow(s.how))
		}
		fmt.Printf("  %-14s %7s %7s %7s %7s\n", "region", "cur", "T1", "T2", "T3")
		line("(keys)", tr.keys)
		line("(col dir)", tr.colDir)
		for c, s := range tr.cols {
			line(tr.colNames[c], s)
		}
	}
	return trees
}

// expWchar is the bytes this process has passed to write syscalls (Linux /proc/self/io).
func expWchar() int64 {
	b, _ := os.ReadFile("/proc/self/io")
	for _, ln := range strings.Split(string(b), "\n") {
		if strings.HasPrefix(ln, "wchar: ") {
			var v int64
			fmt.Sscanf(ln[7:], "%d", &v)
			return v
		}
	}
	return 0
}

type expRun struct {
	pageSize                  int
	build                     time.Duration
	loadWritten               int64
	fileLoaded, fileCompacted int64
	leaves, inner             int
	fill                      float64
	sz                        expSizes
	lz, z                     int
	timings                   []string
}

func expOne(t *testing.T, dir string, pageSize uint32, detail bool) expRun {
	path := filepath.Join(dir, fmt.Sprintf("exp%d.jed", pageSize))
	os.Remove(path)
	os.RemoveAll(path + ".lock")
	run := expRun{pageSize: int(pageSize)}
	start, w0 := time.Now(), expWchar()
	db := expBuild(t, path, pageSize)
	run.build, run.loadWritten = time.Since(start), expWchar()-w0
	st, _ := os.Stat(path)
	run.fileLoaded = st.Size()
	if detail {
		expReport(t, db, path)
	}
	if err := db.Compact("main"); err != nil {
		t.Fatal(err)
	}
	st, _ = os.Stat(path)
	run.fileCompacted = st.Size()
	if !detail {
		// quiet: swallow the report
		old := os.Stdout
		null, _ := os.Open(os.DevNull)
		devnull, _ := os.OpenFile(os.DevNull, os.O_WRONLY, 0)
		os.Stdout = devnull
		defer func() { null.Close(); devnull.Close() }()
		defer func() { os.Stdout = old }()
	}
	expA = map[string]int{}
	trees := expReport(t, db, path)
	fmt.Println("structural savings by component (compacted file):")
	expAPrint()
	capTot := 0
	for _, tr := range trees {
		run.sz.add(tr.total())
		run.leaves += tr.leaves
		run.inner += tr.inner
		run.lz += tr.lz4Page
		run.z += tr.zPage
		capTot += tr.capBytes
	}
	run.fill = float64(run.sz.cur) / float64(capTot)
	// Reopen: the first run of each query faults every leaf (read + CRC + directory validation, OS
	// cache warm); later runs are resident.
	db.Close()
	db, err := OpenDatabase(path)
	if err != nil {
		t.Fatal(err)
	}
	s := db.Session(SessionOptions{})
	defer s.Close()
	tm := func(label string, d time.Duration, extra string) {
		run.timings = append(run.timings, fmt.Sprintf("%-46s %10v %s", label, d, extra))
	}
	for _, q := range []string{
		"SELECT count(*) FROM line_items WHERE qty = 3",
		"SELECT count(*) FROM orders WHERE status = 'shipped'",
		"SELECT count(*) FROM sessions WHERE user_agent LIKE '%Linux%'",
	} {
		var best, first time.Duration
		var out outcome
		for i := 0; i < 6; i++ {
			st := time.Now()
			o, err := queryOutcome(s, q, nil)
			if err != nil {
				t.Fatal(err)
			}
			d := time.Since(st)
			if i == 0 {
				first = d
			} else if i == 1 || d < best {
				best = d
			}
			out = o
		}
		tm("scan first: "+q[22:], first, "")
		tm("scan resident: "+q[22:], best, fmt.Sprintf("cost %d", out.Cost))
	}
	// Point lookups by primary key and through a secondary index (resident).
	r := rand.New(rand.NewSource(9))
	nOrders := int64(expScale(300000))
	point := func(label, sql string, n int, arg func() int64) {
		p, err := s.Prepare(sql)
		if err != nil {
			t.Fatal(err)
		}
		var cost int64
		for pass := 0; pass < 2; pass++ {
			st := time.Now()
			for i := 0; i < n; i++ {
				o, err := prepOutcome(s, p, []Value{IntValue(arg())})
				if err != nil {
					t.Fatal(err)
				}
				cost = o.Cost
			}
			if pass == 1 {
				tm(label, time.Since(st)/time.Duration(n), fmt.Sprintf("per op, cost %d", cost))
			}
		}
	}
	point("pk lookup (orders)", "SELECT amount FROM orders WHERE id = $1", 20000, func() int64 { return 1 + r.Int63n(nOrders) })
	point("index lookup (orders by customer)", "SELECT count(*) FROM orders WHERE customer_id = $1", 5000, func() int64 { return 1000 + r.Int63n(40000) })
	// Small write transactions: one random-row update per commit (touches the table and one index).
	w0 = expWchar()
	point("update 1 row/commit (orders.status)", "UPDATE orders SET status = 'refunded' WHERE id = $1", 3000, func() int64 { return 1 + r.Int63n(nOrders) })
	tm("  bytes written per update commit", 0, fmt.Sprintf("%d", (expWchar()-w0)/6000))
	w0 = expWchar()
	next := nOrders
	point("insert 1 row/commit (orders, append)", "INSERT INTO orders VALUES ($1, 77, 'paid', '2026-01-01 00:00:00+00', 12.50, 'USD', true, NULL)", 3000, func() int64 { next++; return next })
	tm("  bytes written per insert commit", 0, fmt.Sprintf("%d", (expWchar()-w0)/6000))
	s.Close()
	db.Close()
	return run
}

func TestFormatExperiment(t *testing.T) {
	if os.Getenv("JED_FORMAT_EXP") == "" {
		t.Skip("set JED_FORMAT_EXP=1")
	}
	dir := os.Getenv("JED_FORMAT_EXP_DIR")
	if dir == "" {
		dir = t.TempDir()
	}
	sizes := []uint32{DefaultPageSize}
	if v := os.Getenv("JED_FORMAT_EXP_PAGES"); v != "" {
		sizes = nil
		for _, f := range strings.Split(v, ",") {
			var n uint32
			fmt.Sscanf(f, "%d", &n)
			sizes = append(sizes, n)
		}
	}
	var runs []expRun
	for _, ps := range sizes {
		runs = append(runs, expOne(t, dir, ps, len(sizes) == 1))
	}
	fmt.Printf("\n===== page-size summary =====\n")
	mb := func(x int64) float64 { return float64(x) / (1 << 20) }
	fmt.Printf("%7s %8s %9s %9s %9s %7s %6s %6s | %8s %8s %8s %8s | %8s %8s\n", "page", "build", "loadWrMB", "loadedMB", "compactMB", "leaves", "inner", "fill%", "cur MB", "T1 MB", "T2 MB", "T3 MB", "lz4 MB", "flate MB")
	for _, r := range runs {
		fmt.Printf("%7d %8.1fs %9.0f %9.1f %9.1f %7d %6d %6.1f | %8.1f %8.1f %8.1f %8.1f | %8.1f %8.1f\n", r.pageSize, r.build.Seconds(), mb(r.loadWritten), mb(r.fileLoaded), mb(r.fileCompacted),
			r.leaves, r.inner, 100*r.fill, mb(int64(r.sz.cur)), mb(int64(r.sz.t1)), mb(int64(r.sz.t2)), mb(int64(r.sz.t3)), mb(int64(r.lz)), mb(int64(r.z)))
	}
	for _, r := range runs {
		fmt.Printf("\npage %d\n", r.pageSize)
		for _, l := range r.timings {
			fmt.Println("  " + l)
		}
	}
}

// expTally accumulates what commits write, by page type (via expCommitHook).
type expTally struct {
	commits                                             int
	leaf, interior, catalog, overflow, other, fl, manif int
	freeEntries                                         int
}

func (a *expTally) hook(body, fl, manifest []dirtyPage, freeEntries int) {
	a.commits++
	for _, p := range body {
		switch p.bytes[0] {
		case pageLeaf:
			a.leaf++
		case pageInterior:
			a.interior++
		case pageCatalog:
			a.catalog++
		case pageOverflow:
			a.overflow++
		default:
			a.other++
		}
	}
	a.fl += len(fl)
	a.manif += len(manifest)
	a.freeEntries += freeEntries
}

func (a *expTally) print(label string) {
	c := float64(a.commits)
	if c == 0 {
		return
	}
	tot := a.leaf + a.interior + a.catalog + a.overflow + a.other + a.fl + a.manif
	fmt.Printf("%-34s commits %6d  pages/commit %6.1f = leaf %5.1f interior %4.1f catalog %4.1f free-list %5.1f manifest %4.1f other %3.1f (+1 meta) | avg free entries %7.0f\n",
		label, a.commits, float64(tot)/c, float64(a.leaf)/c, float64(a.interior)/c, float64(a.catalog)/c, float64(a.fl)/c, float64(a.manif)/c, float64(a.overflow+a.other)/c, float64(a.freeEntries)/c)
}

// TestCommitWriteExperiment: what does a commit write, by page type?
//
//	JED_FORMAT_EXP=1 go test -run TestCommitWriteExperiment -v -timeout 30m .
func TestCommitWriteExperiment(t *testing.T) {
	if os.Getenv("JED_FORMAT_EXP") == "" {
		t.Skip("set JED_FORMAT_EXP=1")
	}
	dir := os.Getenv("JED_FORMAT_EXP_DIR")
	if dir == "" {
		dir = t.TempDir()
	}
	path := filepath.Join(dir, "commit.jed")
	os.Remove(path)
	os.RemoveAll(path + ".lock")
	tally := &expTally{}
	expCommitHook = func(b, f, m []dirtyPage, n int) { tally.hook(b, f, m, n) }
	defer func() { expCommitHook = nil }()
	db := expBuild(t, path, DefaultPageSize)
	tally.print("bulk load (500 rows/commit)")
	st, _ := os.Stat(path)
	fmt.Printf("file after load: %d pages\n", st.Size()/int64(DefaultPageSize))
	phase := func(label string, s *Session, sql string, n int, arg func() int64) {
		*tally = expTally{}
		p, err := s.Prepare(sql)
		if err != nil {
			t.Fatal(err)
		}
		w0, t0 := expWchar(), time.Now()
		for i := 0; i < n; i++ {
			if _, err := prepOutcome(s, p, []Value{IntValue(arg())}); err != nil {
				t.Fatal(err)
			}
		}
		tally.print(label)
		fmt.Printf("%-34s %d bytes/commit, %v/commit\n", "", (expWchar()-w0)/int64(n), time.Since(t0)/time.Duration(n))
	}
	r := rand.New(rand.NewSource(9))
	nOrders := int64(expScale(300000))
	run := func(tag string) {
		s := db.Session(SessionOptions{})
		defer s.Close()
		next := nOrders + int64(len(tag))*1000000
		phase(tag+": update non-indexed col", s, "UPDATE orders SET shipped = NOT shipped WHERE id = $1", 2000, func() int64 { return 1 + r.Int63n(nOrders) })
		phase(tag+": update indexed col", s, "UPDATE orders SET status = 'refunded' WHERE id = $1", 2000, func() int64 { return 1 + r.Int63n(nOrders) })
		phase(tag+": insert (append)", s, "INSERT INTO orders VALUES ($1, 77, 'paid', '2026-01-01 00:00:00+00', 12.50, 'USD', true, NULL)", 2000, func() int64 { next++; return next })
	}
	run("after load")
	if err := db.Compact("main"); err != nil {
		t.Fatal(err)
	}
	if pf := os.Getenv("JED_FORMAT_EXP_PROF"); pf != "" {
		f, _ := os.Create(pf)
		pprof.StartCPUProfile(f)
		run("after compact")
		pprof.StopCPUProfile()
		f.Close()
	} else {
		run("after compact")
	}
	// The file must check clean and reopen with the same rows.
	rep, err := db.IntegrityCheck(context.Background(), "main", IntegrityCheckOptions{Level: IntegrityFull})
	if err != nil || !rep.OK() {
		t.Fatalf("integrity: %v %+v", err, rep.Findings)
	}
	before := countRows(t, db, "orders")
	db.Close()
	db, err = OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true, Credential: expCredential()})
	if err != nil {
		t.Fatal(err)
	}
	rep, err = db.IntegrityCheck(context.Background(), "main", IntegrityCheckOptions{Level: IntegrityFull})
	if err != nil || !rep.OK() || countRows(t, db, "orders") != before {
		t.Fatalf("after reopen: %v %+v", err, rep.Findings)
	}
	st, _ = os.Stat(path)
	fmt.Printf("integrity clean before and after reopen; %d orders; file %d pages\n", before, st.Size()/int64(DefaultPageSize))
	db.Close()
	os.Remove(path)
}

// --- free-list shape check -------------------------------------------------------------------------
// With JED_FREE_CHECK=<file>, every file commit in the whole test run checks the property the cursor
// design needs: between rebuilds the new free list is one contiguous run of the old one.

var expFC struct{ commits, rebuilds, contiguous, prefix, unchanged, violations, gated, flNow, flNew atomic.Int64 }

func init() {
	out := os.Getenv("JED_FREE_CHECK")
	if out == "" {
		return
	}
	expFreeCheck = func(before, after []uint32, rebuilt, pending, client, canReuse bool) {
		expFC.commits.Add(1)
		chain := int64(len(after)+2043) / 2044 // 8 KiB pages
		if len(after) < 2 {
			chain = 0
		}
		expFC.flNow.Add(chain)
		defer func(v int64) {
			if rebuilt || expFC.violations.Load() != v {
				expFC.flNew.Add(chain)
			}
		}(expFC.violations.Load())
		if rebuilt {
			expFC.rebuilds.Add(1)
			return
		}
		if !canReuse {
			expFC.gated.Add(1)
		}
		if len(after) == len(before) {
			same := true
			for i := range after {
				if after[i] != before[i] {
					same = false
					break
				}
			}
			if same {
				expFC.unchanged.Add(1)
				return
			}
		}
		// find after as a contiguous run of before
		ok := len(after) <= len(before)
		start := 0
		if ok && len(after) > 0 {
			start = sort.Search(len(before), func(i int) bool { return before[i] >= after[0] })
			ok = start+len(after) <= len(before)
			for i := 0; ok && i < len(after); i++ {
				ok = before[start+i] == after[i]
			}
		}
		switch {
		case !ok:
			expFC.violations.Add(1)
			f, _ := os.OpenFile(out, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
			fmt.Fprintf(f, "VIOLATION pending=%v client=%v canReuse=%v before=%d after=%d\n", pending, client, canReuse, len(before), len(after))
			f.Close()
		case len(after) == 0 || start+len(after) == len(before):
			expFC.prefix.Add(1) // only a prefix was consumed
		default:
			expFC.contiguous.Add(1) // a prefix consumed and a tail truncated
		}
	}
}

func TestZZFreeCheckSummary(t *testing.T) {
	out := os.Getenv("JED_FREE_CHECK")
	if out == "" {
		t.Skip()
	}
	f, _ := os.OpenFile(out, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	fmt.Fprintf(f, "SUMMARY commits=%d rebuilds=%d unchanged=%d prefix-only=%d prefix+tail=%d gated=%d violations=%d free-list pages today=%d kept-chain rule=%d\n", expFC.commits.Load(), expFC.rebuilds.Load(),
		expFC.unchanged.Load(), expFC.prefix.Load(), expFC.contiguous.Load(), expFC.gated.Load(), expFC.violations.Load(), expFC.flNow.Load(), expFC.flNew.Load())
	f.Close()
}

// TestDeferInteriorStress (PROTOTYPE, remap.go): random single- and multi-row commits against a file
// database with deferred interiors, checked against an in-memory database running the same
// statements. Every so often: a full integrity check, and a close and reopen (which must apply the
// remap list). Every commit recounts live pages by reachability.
//
//	JED_DEFER_INTERIOR=on go test -run TestDeferInteriorStress -count=1 .
func TestDeferInteriorStress(t *testing.T) {
	if !deferInterior {
		t.Skip("set JED_DEFER_INTERIOR=on")
	}
	t.Run("plaintext", func(t *testing.T) { deferInteriorStress(t, nil) })
	t.Run("encrypted", func(t *testing.T) { deferInteriorStress(t, KeyCredential([32]byte{1, 2, 3})) })
}

func deferInteriorStress(t *testing.T, cred *Credential) {
	SetVerifyLivePages(true)
	defer SetVerifyLivePages(false)
	path := filepath.Join(t.TempDir(), "defer.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, SkipFsync: true, Credential: cred})
	if err != nil {
		t.Fatal(err)
	}
	mem, err := CreateDatabase(CreateOptions{})
	if err != nil {
		t.Fatal(err)
	}
	defer mem.Close()
	both := func(sql string) {
		t.Helper()
		for _, d := range []*Database{db, mem} {
			s := d.Session(SessionOptions{})
			expExec(t, s, sql)
			s.Close()
		}
	}
	dump := func(d *Database) string {
		s := d.Session(SessionOptions{})
		defer s.Close()
		var b strings.Builder
		for _, q := range []string{
			"SELECT id, k, v, pad FROM t ORDER BY id",
			"SELECT id FROM t WHERE k BETWEEN 10 AND 40 ORDER BY k, id",
			"SELECT count(*), sum(k) FROM t",
			"SELECT id, n FROM u ORDER BY id",
		} {
			rows, err := s.Query(context.Background(), q)
			if err != nil {
				t.Fatal(err)
			}
			for rows.Next() {
				vals, err := rows.Values()
				if err != nil {
					t.Fatal(err)
				}
				fmt.Fprintln(&b, vals...)
			}
			if err := rows.Err(); err != nil {
				t.Fatal(err)
			}
			rows.Close()
		}
		return b.String()
	}
	both("CREATE TABLE t (id i64 PRIMARY KEY, k i32 NOT NULL, v text NOT NULL, pad text)")
	both("CREATE INDEX t_k ON t (k)")
	both("CREATE INDEX t_v ON t (v)")
	both("CREATE TABLE u (id i32 PRIMARY KEY, n i64 NOT NULL)")
	r := rand.New(rand.NewSource(42))
	next := int64(0)
	var rows []string
	for i := 0; i < 20000; i++ {
		next++
		rows = append(rows, fmt.Sprintf("(%d, %d, 'v%06d', '%s')", next, r.Intn(100), r.Intn(1000000), strings.Repeat("x", r.Intn(120))))
		if len(rows) == 500 {
			both("INSERT INTO t VALUES " + strings.Join(rows, ","))
			rows = rows[:0]
		}
	}
	both("INSERT INTO u VALUES (1, 0), (2, 0), (3, 0)")
	reopens, folds := 0, 0
	for i := 0; i < 6000; i++ {
		id := 1 + r.Int63n(next)
		switch x := r.Intn(100); {
		case x < 35:
			both(fmt.Sprintf("UPDATE t SET pad = '%s' WHERE id = %d", strings.Repeat("y", r.Intn(120)), id))
		case x < 55:
			both(fmt.Sprintf("UPDATE t SET k = %d, v = 'w%06d' WHERE id = %d", r.Intn(100), r.Intn(1000000), id))
		case x < 75:
			next++
			both(fmt.Sprintf("INSERT INTO t VALUES (%d, %d, 'n%06d', NULL)", next, r.Intn(100), r.Intn(1000000)))
		case x < 85:
			both(fmt.Sprintf("DELETE FROM t WHERE id = %d", id))
		case x < 90:
			both(fmt.Sprintf("DELETE FROM t WHERE id BETWEEN %d AND %d", id, id+int64(r.Intn(400))))
		case x < 95:
			both(fmt.Sprintf("UPDATE t SET k = k + 1 WHERE id BETWEEN %d AND %d", id, id+int64(r.Intn(300))))
		default:
			both(fmt.Sprintf("UPDATE u SET n = n + 1 WHERE id = %d", 1+r.Intn(3)))
		}
		if db.core.storage.paging.remap.len() == 0 {
			folds++
		}
		if i%250 == 249 {
			rep, err := db.IntegrityCheck(context.Background(), "main", IntegrityCheckOptions{Level: IntegrityFull})
			if err != nil || !rep.OK() {
				t.Fatalf("commit %d: integrity: %v %+v", i, err, rep.Findings)
			}
		}
		if i%400 == 399 {
			pendingEntries := db.core.storage.paging.remap.len()
			db.Close()
			if db, err = OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true, Credential: cred}); err != nil {
				t.Fatalf("commit %d: reopen: %v", i, err)
			}
			if got := db.core.storage.paging.remap.len(); got != pendingEntries {
				t.Fatalf("commit %d: reopened with %d remap entries, closed with %d", i, got, pendingEntries)
			}
			reopens++
			if a, b := dump(db), dump(mem); a != b {
				t.Fatalf("commit %d: contents differ after reopen", i)
			}
		}
	}
	if a, b := dump(db), dump(mem); a != b {
		t.Fatal("contents differ")
	}
	rep, err := db.IntegrityCheck(context.Background(), "main", IntegrityCheckOptions{Level: IntegrityFull})
	if err != nil || !rep.OK() {
		t.Fatalf("integrity: %v %+v", err, rep.Findings)
	}
	t.Logf("%d reopens; %d commits left an empty remap list; final list %d entries", reopens, folds, db.core.storage.paging.remap.len())
	db.Close()
}

// expCredential is a fixed key when JED_FORMAT_EXP_ENCRYPT is set, so the experiments run on an
// encrypted file; nil otherwise.
func expCredential() *Credential {
	if os.Getenv("JED_FORMAT_EXP_ENCRYPT") == "" {
		return nil
	}
	return KeyCredential([32]byte{7})
}

// expA accumulates the structural tier's savings by component (bytes).
var expA = map[string]int{}

func expAPrint() {
	total := expA["(leaf bytes today: all)"]
	var names []string
	for k := range expA {
		names = append(names, k)
	}
	sort.Slice(names, func(i, j int) bool { return expA[names[i]] > expA[names[j]] })
	for _, k := range names {
		fmt.Printf("  %-90s %12d  %5.1f%%\n", k, expA[k], 100*float64(expA[k])/float64(total))
	}
}

// expDW is the directory entry width of the leaf being sized (4, or 2 for a narrow leaf).
var expDW = 4

// TestLookupExperiment: repeated point-lookup timings on the compacted test database, to compare leaf
// layouts across processes.
//
//	JED_FORMAT_EXP=1 go test -run TestLookupExperiment -v -timeout 30m .
func TestLookupExperiment(t *testing.T) {
	if os.Getenv("JED_FORMAT_EXP") == "" {
		t.Skip("set JED_FORMAT_EXP=1")
	}
	dir := os.Getenv("JED_FORMAT_EXP_DIR")
	if dir == "" {
		dir = t.TempDir()
	}
	path := filepath.Join(dir, "lookup.jed")
	os.Remove(path)
	os.RemoveAll(path + ".lock")
	db := expBuild(t, path, DefaultPageSize)
	defer func() { db.Close(); os.Remove(path) }()
	if err := db.Compact("main"); err != nil {
		t.Fatal(err)
	}
	s := db.Session(SessionOptions{})
	defer s.Close()
	nOrders := int64(expScale(300000))
	run := func(label, sql string, n int, arg func(r *rand.Rand) int64) {
		p, err := s.Prepare(sql)
		if err != nil {
			t.Fatal(err)
		}
		var ds []time.Duration
		for pass := 0; pass < 9; pass++ {
			r := rand.New(rand.NewSource(9))
			st := time.Now()
			for i := 0; i < n; i++ {
				if _, err := prepOutcome(s, p, []Value{IntValue(arg(r))}); err != nil {
					t.Fatal(err)
				}
			}
			ds = append(ds, time.Since(st)/time.Duration(n))
		}
		first := ds[0]
		rest := append([]time.Duration(nil), ds[1:]...)
		sort.Slice(rest, func(i, j int) bool { return rest[i] < rest[j] })
		fmt.Printf("%-36s first pass %8v   later passes: min %8v median %8v max %8v\n", label, first, rest[0], rest[len(rest)/2], rest[len(rest)-1])
	}
	run("pk lookup (orders)", "SELECT amount FROM orders WHERE id = $1", 20000, func(r *rand.Rand) int64 { return 1 + r.Int63n(nOrders) })
	run("pk lookup, key column only", "SELECT id FROM orders WHERE id = $1", 20000, func(r *rand.Rand) int64 { return 1 + r.Int63n(nOrders) })
	run("index lookup (orders by customer)", "SELECT count(*) FROM orders WHERE customer_id = $1", 5000, func(r *rand.Rand) int64 { return 1000 + r.Int63n(40000) })
}
