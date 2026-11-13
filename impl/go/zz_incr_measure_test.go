package jed

// EXPERIMENT (spec/design/incremental-reclaim.md §7): file size, rebuild walks and commit latency of
// the free-list reclamation modes. Gated by JED_INCR_MEASURE=a|b; the mode is the process's
// environment (JED_INCR_RECLAIM, JED_RECLAIM_SEED_LIVE), so each mode is its own run:
//
//	JED_INCR_MEASURE=a JED_INCR_RECLAIM=on go test -run TestIncrMeasure -v -timeout 60m .

import (
	"fmt"
	"math/rand"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"testing"
	"time"
)

type incrPhase struct {
	name                   string
	lat                    []time.Duration
	walkLat                []time.Duration
	walks                  uint64
	maxRatio, sumRatio     float64
	commits                int
	endPages, endLive      uint32
	bytesWritten, maxPages uint32
}

func (p *incrPhase) observe(c *sharedCore, d time.Duration, walked bool) {
	st := c.storage
	st.mu.Lock()
	pc, live := st.pageCount, st.live.live
	st.mu.Unlock()
	p.lat = append(p.lat, d)
	if walked {
		p.walks++
		p.walkLat = append(p.walkLat, d)
	}
	r := float64(pc) / float64(max(live, 1))
	p.maxRatio = max(p.maxRatio, r)
	p.sumRatio += r
	p.commits++
	p.endPages, p.endLive = pc, live
	p.maxPages = max(p.maxPages, pc)
}

func pct(ds []time.Duration, q float64) time.Duration {
	if len(ds) == 0 {
		return 0
	}
	s := append([]time.Duration(nil), ds...)
	sort.Slice(s, func(i, j int) bool { return s[i] < s[j] })
	return s[min(len(s)-1, int(q*float64(len(s))))]
}

func (p *incrPhase) report(t *testing.T, mode string) {
	var sum time.Duration
	for _, d := range p.lat {
		sum += d
	}
	mean := time.Duration(0)
	if len(p.lat) > 0 {
		mean = sum / time.Duration(len(p.lat))
	}
	t.Logf("RESULT mode=%s phase=%s commits=%d walks=%d end_pages=%d end_live=%d end_ratio=%.3f mean_ratio=%.3f max_ratio=%.3f "+
		"lat_mean=%v p50=%v p99=%v max=%v walk_max=%v total=%v",
		mode, p.name, p.commits, p.walks, p.endPages, p.endLive, float64(p.endPages)/float64(max(p.endLive, 1)),
		p.sumRatio/float64(max(p.commits, 1)), p.maxRatio, mean, pct(p.lat, 0.5), pct(p.lat, 0.99), pct(p.lat, 1),
		pct(p.walkLat, 1), sum)
}

func incrMode() string {
	switch {
	case incrOn():
		return fmt.Sprintf("incr/%d", incrLeakDen)
	case reclaimSeedLive:
		return "2x+seedfix"
	}
	return "2x"
}

// incrTimed runs one statement (one commit) and records it.
func incrTimed(t *testing.T, p *incrPhase, db *Database, s *Session, sql string) {
	w := reclaimWalks.Load()
	start := time.Now()
	expExec(t, s, sql)
	p.observe(db.core, time.Since(start), reclaimWalks.Load() != w)
}

func TestIncrMeasure(t *testing.T) {
	which := os.Getenv("JED_INCR_MEASURE")
	if which == "" {
		t.Skip("JED_INCR_MEASURE=a|b")
	}
	dir := t.TempDir()
	if v := os.Getenv("JED_INCR_MEASURE_DIR"); v != "" {
		dir = v
	}
	mode := incrMode()
	switch which {
	case "a":
		incrMeasureA(t, filepath.Join(dir, "a.jed"), mode)
	case "b":
		incrMeasureB(t, filepath.Join(dir, "b.jed"), mode)
	}
}

// Workload (a): a 300k-row indexed table loaded by random-key 500-row inserts, then 25,000 single-row
// updates of the indexed column across 5 reopens, with an 8 MiB cache.
func incrMeasureA(t *testing.T, path, mode string) {
	const cache = 8 << 20
	db, err := CreateDatabase(CreateOptions{Path: path, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	s := db.Session(SessionOptions{})
	expExec(t, s, "CREATE TABLE t (id i64 PRIMARY KEY, k i64 NOT NULL, pad text NOT NULL)")
	expExec(t, s, "CREATE INDEX t_k ON t (k)")
	s.Close()
	db.Close()
	open := func() *Database {
		db, err := OpenDatabaseWithOptions(path, OpenOptions{CacheBytes: cache, SkipFsync: true})
		if err != nil {
			t.Fatal(err)
		}
		return db
	}
	r := rand.New(rand.NewSource(42))
	n := expScale(300000)
	ids := make([]int64, 0, n)
	pad := func() string { return strings.Repeat(string(rune('a'+r.Intn(26))), 40+r.Intn(40)) }
	load := &incrPhase{name: "load"}
	db = open()
	s = db.Session(SessionOptions{})
	var b strings.Builder
	for len(ids) < n {
		b.Reset()
		b.WriteString("INSERT INTO t VALUES ")
		for j := 0; j < 500 && len(ids) < n; j++ {
			id := r.Int63()
			ids = append(ids, id)
			if j > 0 {
				b.WriteByte(',')
			}
			fmt.Fprintf(&b, "(%d,%d,'%s')", id, r.Int63n(1<<40), pad())
		}
		incrTimed(t, load, db, s, b.String())
	}
	load.report(t, mode)
	s.Close()
	db.Close()
	upd := &incrPhase{name: "updates"}
	for round := 0; round < 5; round++ {
		db = open()
		s = db.Session(SessionOptions{})
		for k := 0; k < 5000; k++ {
			incrTimed(t, upd, db, s, fmt.Sprintf("UPDATE t SET k = %d, pad = '%s' WHERE id = %d", r.Int63n(1<<40), pad(), ids[r.Intn(len(ids))]))
		}
		s.Close()
		db.Close()
	}
	upd.report(t, mode)
	incrFinal(t, path, mode, cache)
}

// Workload (b): the five-table database of expBuild (1.3M rows over 500-row commits), then 10,000
// single-row updates spread over the tables across 2 reopens.
func incrMeasureB(t *testing.T, path, mode string) {
	build := &incrPhase{name: "build"}
	w := reclaimWalks.Load()
	last := time.Now()
	// expBuild's statements are one commit each: time each from the previous one's end.
	expAfterExec = func(s *Session) {
		now := time.Now()
		wn := reclaimWalks.Load()
		build.observe(s.core, now.Sub(last), wn != w)
		w, last = wn, now
	}
	db := expBuild(t, path, 0)
	expAfterExec = nil
	build.report(t, mode)
	db.Close()
	r := rand.New(rand.NewSource(7))
	upd := &incrPhase{name: "updates"}
	nOrders, nUsers := expScale(300000), expScale(100000)
	for round := 0; round < 2; round++ {
		db, err := OpenDatabaseWithOptions(path, OpenOptions{CacheBytes: 8 << 20, SkipFsync: true, Credential: expCredential()})
		if err != nil {
			t.Fatal(err)
		}
		s := db.Session(SessionOptions{})
		for k := 0; k < 5000; k++ {
			var sql string
			switch k % 3 {
			case 0:
				sql = fmt.Sprintf("UPDATE orders SET status = 'shipped', note = 'n%d' WHERE id = %d", k, 1+r.Intn(nOrders))
			case 1:
				sql = fmt.Sprintf("UPDATE users SET last_login = '2025-12-01 00:00:%02d+00' WHERE id = %d", k%60, 1+r.Intn(nUsers))
			default:
				sql = fmt.Sprintf("UPDATE orders SET customer_id = %d WHERE id = %d", 1+r.Intn(50000), 1+r.Intn(nOrders))
			}
			incrTimed(t, upd, db, s, sql)
		}
		s.Close()
		db.Close()
	}
	upd.report(t, mode)
	incrFinal(t, path, mode, 8<<20)
}

// incrFinal reports the closed file's size against its live pages, and what compaction would leave.
func incrFinal(t *testing.T, path, mode string, cache int) {
	db, err := OpenDatabaseWithOptions(path, OpenOptions{CacheBytes: cache, SkipFsync: true, Credential: expCredential()})
	if err != nil {
		t.Fatal(err)
	}
	st := db.core.storage
	pc, live, free := st.pageCount, st.live.live, len(st.freePages)
	fi, _ := os.Stat(path)
	if err := db.Compact("main"); err != nil {
		t.Logf("compact: %v", err)
	}
	cpc := st.pageCount
	db.Close()
	t.Logf("RESULT mode=%s phase=final pages=%d live=%d free=%d unlisted=%d ratio=%.3f bytes=%d compacted_pages=%d",
		mode, pc, live, free, int(pc)-int(live)-free-2, float64(pc)/float64(max(live, 1)), fi.Size(), cpc)
}
