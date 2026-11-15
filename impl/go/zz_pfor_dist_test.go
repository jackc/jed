package jed

// EXPERIMENT (spec/design/leaf-write-cost.md §9): format v54's kind 6 under several exception caps,
// and the blocks prototype, on realistic column distributions. JED_PFOR_DIST=1 to run;
// JED_PFOR_DIST_ROWS scales the tables.

import (
	"fmt"
	"math"
	"math/rand/v2"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"testing"
	"time"
)

type distTable struct {
	name   string
	create string
	cols   []string
	rows   func(n int, rng *rand.Rand) [][]string
	stages []distStage
	micro  string // a single-row UPDATE with $ID
}

type distStage struct {
	name string
	sql  []string
}

var distT0 = time.Date(2025, 1, 1, 0, 0, 0, 0, time.UTC)

func tsLit(t time.Time) string {
	return "CAST('" + t.Format("2006-01-02 15:04:05.000000") + "+00' AS timestamptz)"
}

func dateLit(t time.Time) string { return "DATE '" + t.Format("2006-01-02") + "'" }

func distTables(n int) []distTable {
	now1 := distT0.Add(time.Duration(n)*2*time.Second + 10*24*time.Hour)
	now2 := now1.Add(3 * 24 * time.Hour)
	return []distTable{
		{
			name: "orders",
			create: "CREATE TABLE orders (id bigint PRIMARY KEY, created_at timestamptz, updated_at timestamptz, " +
				"customer_id bigint, amount_cents bigint, status integer, discount_cents integer, valid_until date, note text)",
			cols: []string{"created_at", "updated_at", "customer_id", "amount_cents", "status", "discount_cents", "valid_until"},
			rows: func(n int, rng *rand.Rand) [][]string {
				zipf := rand.NewZipf(rng, 1.1, 1, 499999)
				out := make([][]string, n)
				for i := 0; i < n; i++ {
					id := i + 1
					c := distT0.Add(time.Duration(id)*2*time.Second + time.Duration(rng.NormFloat64()*1.5e6)*time.Microsecond)
					if rng.Float64() < 0.003 { // late-arriving backfill
						c = c.Add(-time.Duration(3600+rng.IntN(3*86400)) * time.Second)
					}
					u := c.Add(time.Duration(rng.IntN(30_000_000)) * time.Microsecond)
					cust := (zipf.Uint64()*2654435761)%500000 + 1 // popular customers scattered
					amt := int64(math.Exp(math.Log(2500) + 1.1*rng.NormFloat64()))
					st := 0
					switch x := rng.Float64(); {
					case x < 0.40:
						st = 0
					case x < 0.70:
						st = 1
					case x < 0.95:
						st = 2
					case x < 0.999:
						st = 3
					default:
						st = 99
					}
					disc := 0
					if rng.Float64() < 0.15 {
						disc = 100 + rng.IntN(4900)
					}
					vu := "DATE '9999-12-31'"
					if rng.Float64() < 0.03 {
						vu = dateLit(c.AddDate(0, 0, 30+rng.IntN(335)))
					}
					out[i] = []string{
						strconv.Itoa(id), tsLit(c), tsLit(u), strconv.FormatUint(cust, 10), strconv.FormatInt(amt, 10),
						strconv.Itoa(st), strconv.Itoa(disc), vu, "'note " + strconv.Itoa(rng.IntN(1_000_000)) + "'",
					}
				}
				return out
			},
			micro: "UPDATE orders SET updated_at = " + tsLit(now1) + " WHERE id = $ID",
			stages: []distStage{
				{"touch 1/200", []string{"UPDATE orders SET updated_at = " + tsLit(now1) + " WHERE id % 200 = 7"}},
				{"touch 2/200", []string{"UPDATE orders SET updated_at = " + tsLit(now1.Add(time.Hour)) + " WHERE id % 200 = 107"}},
				{"touch 10%", []string{"UPDATE orders SET updated_at = " + tsLit(now2) + " + CAST(id % 3600 AS integer) * INTERVAL '1 second' WHERE id % 10 = 3"}},
				{"hot tail", []string{fmt.Sprintf("UPDATE orders SET updated_at = %s, status = 3 WHERE id > %d", tsLit(now2.Add(time.Hour)), n-n/20)}},
				{"status 8%", []string{"UPDATE orders SET status = 2 WHERE id % 13 = 1"}},
				{"amount fix", []string{"UPDATE orders SET amount_cents = amount_cents * 100 WHERE id % 211 = 3"}},
				{"expire 2%", []string{"UPDATE orders SET valid_until = DATE '2025-06-01' WHERE id % 50 = 9"}},
			},
		},
		{
			name:   "sentinels",
			create: "CREATE TABLE sentinels (id bigint PRIMARY KEY, s1 date, s2 date, s4 date, s6 date, s8 date, s12 date)",
			cols:   []string{"s1", "s2", "s4", "s6", "s8", "s12"},
			rows: func(n int, rng *rand.Rand) [][]string {
				rates := []float64{0.01, 0.02, 0.04, 0.06, 0.08, 0.12}
				out := make([][]string, n)
				for i := 0; i < n; i++ {
					row := []string{strconv.Itoa(i + 1)}
					for _, p := range rates {
						if rng.Float64() < p {
							row = append(row, dateLit(distT0.AddDate(0, 0, rng.IntN(400))))
						} else {
							row = append(row, "DATE '9999-12-31'")
						}
					}
					out[i] = row
				}
				return out
			},
		},
		{
			name:   "events",
			create: "CREATE TABLE events (id bigint PRIMARY KEY, ts timestamptz, device_id integer, reading bigint, seq bigint)",
			cols:   []string{"ts", "device_id", "reading", "seq"},
			rows: func(n int, rng *rand.Rand) [][]string {
				out := make([][]string, n)
				seq := map[int]int{}
				t := distT0
				for i := 0; i < n; i++ {
					t = t.Add(time.Duration(rng.ExpFloat64()*200_000) * time.Microsecond)
					dev := 1 + rng.IntN(5000)
					seq[dev]++
					r := int64(20000 + rng.NormFloat64()*300)
					if rng.Float64() < 0.005 { // sensor glitch
						r = 1_000_000_000 + rng.Int64N(1000)
					}
					out[i] = []string{strconv.Itoa(i + 1), tsLit(t), strconv.Itoa(dev), strconv.FormatInt(r, 10), strconv.Itoa(seq[dev])}
				}
				return out
			},
			micro: "UPDATE events SET reading = 5000000000 WHERE id = $ID",
			stages: []distStage{
				{"glitch fix", []string{"UPDATE events SET reading = 20000 WHERE reading > 100000000"}},
				{"late fix", []string{"UPDATE events SET reading = 0 WHERE id % 997 = 0"}},
			},
		},
	}
}

type distMode struct {
	name   string
	pfor   int
	blocks int
	bexc   bool
}

var distModes = []distMode{
	{"v53", 0, 0, false},
	{"pfor≤1", 1, 0, false},
	{"pfor≤4", 4, 0, false},
	{"pfor≤8", 8, 0, false},
	{"v54", pforMaxK, 0, false},
}

// distModesFrom selects modes by JED_PFOR_DIST_MODES (comma-separated names), all by default.
func distModesFrom() []distMode {
	want := os.Getenv("JED_PFOR_DIST_MODES")
	if want == "" {
		return distModes
	}
	all := append(append([]distMode(nil), distModes...),
		distMode{"blk64", 0, 64, false}, distMode{"blk64+exc", 0, 64, true})
	var out []distMode
	for _, name := range strings.Split(want, ",") {
		for _, m := range all {
			if m.name == name {
				out = append(out, m)
			}
		}
	}
	return out
}

type regionStats struct {
	bytes, plain int
	kinds        [10]int
	excs         int
}

func columnRegionStats(t *testing.T, path, table string, cols []string) (map[string]*regionStats, int) {
	st, leaves, _, _ := columnRegionStatsFill(t, path, table, cols)
	return st, leaves
}

// columnRegionStatsFill also returns the mean leaf fill (payload / C) and rows per leaf.
func columnRegionStatsFill(t *testing.T, path, table string, cols []string) (map[string]*regionStats, int, float64, float64) {
	t.Helper()
	file, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	img, err := openImage(file)
	if err != nil {
		t.Fatal(err)
	}
	defer img.Close()
	store := img.committed.store(table)
	ct, _ := img.committed.table(table)
	src := store.leafSrc()
	out := map[string]*regionStats{}
	idx := map[int]string{}
	for _, c := range cols {
		out[c] = &regionStats{}
	}
	for c, cl := range ct.Columns {
		if _, ok := out[cl.Name]; ok {
			idx[c] = cl.Name
		}
	}
	leaves, payload, nrows := 0, 0, 0
	var walk func(n *pnode)
	walk = func(n *pnode) {
		if !n.isLeaf() {
			for _, c := range n.children {
				child, err := resolveChild(c, src)
				if err != nil {
					t.Fatal(err)
				}
				walk(child)
			}
			return
		}
		leaves++
		if n.packed == nil {
			t.Fatal("leaf not packed")
		}
		l := n.packed.dirs
		rows := n.keyLen()
		nrows += rows
		payload += n.payload(store.cap, store.shape)
		for c, name := range idx {
			r := &l.regions[c]
			st := out[name]
			w, _ := fixedValueWidth(n.packed.colTypes[c])
			plain := 1 + (rows+7)/8 + rows*w
			st.plain += plain
			switch {
			case r.pfRows != nil:
				st.kinds[6]++
				st.excs += len(r.pfRows) / 2
				st.bytes += r.stored
			case r.isFOR:
				st.kinds[1]++
				st.bytes += r.stored
			case r.stored > 0:
				st.kinds[8]++ // blocks / runs
				st.bytes += r.stored
			default:
				st.kinds[0]++
				st.bytes += plain
			}
		}
	}
	if root := store.rows.rootNode(); root != nil {
		walk(root)
	}
	return out, leaves, float64(payload) / float64(leaves*store.cap.c), float64(nrows) / float64(leaves)
}

func TestPFORDistributions(t *testing.T) {
	if os.Getenv("JED_PFOR_DIST") == "" {
		t.Skip("set JED_PFOR_DIST=1")
	}
	n := 200_000
	if s := os.Getenv("JED_PFOR_DIST_ROWS"); s != "" {
		n, _ = strconv.Atoi(s)
	}
	savedPFOR, savedBlocks, savedExc := leafPFOR, leafBlocks, leafBlocksExc
	defer func() { leafPFOR, leafBlocks, leafBlocksExc = savedPFOR, savedBlocks, savedExc }()
	only := os.Getenv("JED_PFOR_DIST_TABLE")
	for _, tb := range distTables(n) {
		if only != "" && only != tb.name {
			continue
		}
		rows := tb.rows(n, rand.New(rand.NewPCG(1, 2)))
		order := os.Getenv("JED_PFOR_DIST_ORDER") // "shuffled": rows arrive in random key order
		if order == "shuffled" {
			r := rand.New(rand.NewPCG(3, 4))
			r.Shuffle(len(rows), func(i, j int) { rows[i], rows[j] = rows[j], rows[i] })
		}
		var inserts []string
		for at := 0; at < len(rows); at += 1000 {
			var sb strings.Builder
			sb.WriteString("INSERT INTO " + tb.name + " VALUES ")
			for i := at; i < min(at+1000, len(rows)); i++ {
				if i > at {
					sb.WriteString(", ")
				}
				sb.WriteString("(" + strings.Join(rows[i], ", ") + ")")
			}
			inserts = append(inserts, sb.String())
		}
		t.Logf("==== %s: %d rows", tb.name, n)
		for _, m := range distModesFrom() {
			leafPFOR, leafBlocks, leafBlocksExc = m.pfor, m.blocks, m.bexc
			path := filepath.Join(t.TempDir(), tb.name+".jed")
			db, err := CreateDatabase(CreateOptions{Path: path, SkipFsync: true})
			if err != nil {
				t.Fatal(err)
			}
			sess := mustSession(db, SessionOptions{})
			mustExec(t, sess, tb.create)
			start := time.Now()
			for _, sql := range inserts {
				mustExec(t, sess, sql)
			}
			loadT := time.Since(start)
			live := []uint32{fileLivePages(t, path)}
			stats0, leaves, fill, perLeaf := columnRegionStatsFill(t, path, tb.name, tb.cols)
			// Single-row touches rolled back (update_untouched_large_rollback's shape): each sets one
			// row's micro column to a value far outside its leaf's span.
			upd := time.Duration(0)
			if tb.micro != "" {
				r := rand.New(rand.NewPCG(5, 6))
				const iters = 2000
				for k := 0; k < iters+200; k++ {
					sql := strings.ReplaceAll(tb.micro, "$ID", strconv.Itoa(1+r.IntN(n)))
					start := time.Now()
					mustExec(t, sess, "BEGIN")
					mustExec(t, sess, sql)
					mustExec(t, sess, "ROLLBACK")
					if k >= 200 {
						upd += time.Since(start)
					}
				}
				upd /= iters
			}
			var stageT []time.Duration
			for _, st := range tb.stages {
				start := time.Now()
				for _, sql := range st.sql {
					mustExec(t, sess, sql)
				}
				stageT = append(stageT, time.Since(start))
				live = append(live, fileLivePages(t, path))
			}
			statsEnd, leavesEnd := columnRegionStats(t, path, tb.name, tb.cols)
			// A full scan of the measured columns, best of three.
			scan := time.Duration(math.MaxInt64)
			sel := "SELECT count(*), " + strings.Join(func() []string {
				var s []string
				for _, c := range tb.cols {
					s = append(s, "max("+c+")")
				}
				return s
			}(), ", ") + " FROM " + tb.name
			for r := 0; r < 3; r++ {
				start := time.Now()
				queryRows(t, sess, sel)
				scan = min(scan, time.Since(start))
			}
			sess.Close()
			if err := db.Close(); err != nil {
				t.Fatal(err)
			}
			var sb strings.Builder
			fmt.Fprintf(&sb, "%-11s load %6.0f ms  scan %5.1f ms  touch+rollback %6.1f µs  rows/leaf %.0f fill %.1f%%  leaves %d→%d  live",
				m.name, ms(loadT), ms(scan), float64(upd.Nanoseconds())/1000, perLeaf, 100*fill, leaves, leavesEnd)
			for i, l := range live {
				if i == 0 {
					fmt.Fprintf(&sb, " %d", l)
				} else {
					fmt.Fprintf(&sb, " →%d", l)
				}
			}
			fmt.Fprintf(&sb, " (%+.1f%%)  stage ms", 100*float64(int(live[len(live)-1])-int(live[0]))/float64(live[0]))
			for _, d := range stageT {
				fmt.Fprintf(&sb, " %.0f", ms(d))
			}
			t.Log(sb.String())
			names := append([]string(nil), tb.cols...)
			sort.Strings(names)
			for _, c := range tb.cols {
				s0, s1 := stats0[c], statsEnd[c]
				t.Logf("    %-15s loaded %8d B (%5.1f%% of plain) k%s exc %6d | end %8d B (%5.1f%%) k%s exc %6d",
					c, s0.bytes, 100*float64(s0.bytes)/float64(s0.plain), kindStr(s0), s0.excs,
					s1.bytes, 100*float64(s1.bytes)/float64(s1.plain), kindStr(s1), s1.excs)
			}
		}
	}
}

func ms(d time.Duration) float64 { return float64(d.Microseconds()) / 1000 }

func kindStr(s *regionStats) string {
	var parts []string
	for k, c := range s.kinds {
		if c > 0 {
			parts = append(parts, fmt.Sprintf("%d:%d", k, c))
		}
	}
	return "[" + strings.Join(parts, " ") + "]"
}
