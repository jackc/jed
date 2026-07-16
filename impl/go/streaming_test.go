package jed

// S3/S4: the lazy result cursor (spec/design/streaming.md §3/§4/§5/§6). The conformance corpus drives
// the materialized Execute path, so the lazy cursor — which only affects Query → Rows — is internal
// machinery the corpus cannot reach (CLAUDE.md §10). These per-core tests pin the contract: a
// fully-drained query yields the IDENTICAL rows + total cost as the eager path (§6); a caller that
// stops early reads (and charges) less (the early-exit win, §6); the cursor pins its snapshot for its
// life (§5); and a mid-drain error surfaces (§6).
//
// The first group covers the S3 streamingCursor (single-table no-blocking-operator scan); the second
// (Buffered* tests) covers the S4 bufferedScanCursor — a blocking plan (non-PK ORDER BY, DISTINCT,
// aggregate, window, join) whose input buffers but whose OUTPUT is yielded one row at a time.

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

// seededKV builds an in-memory shared db with t(id i32 PK, v i32) holding 1..=n (v = id * 10).
func seededKV(t *testing.T, n int64) *Database {
	t.Helper()
	db := memDB()
	w := db.WriteSession()
	if _, err := queryOutcome(w, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)", nil); err != nil {
		t.Fatalf("create: %v", err)
	}
	for i := int64(1); i <= n; i++ {
		if _, err := queryOutcome(w, fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", i, i*10), nil); err != nil {
			t.Fatalf("insert %d: %v", i, err)
		}
	}
	if err := w.Commit(); err != nil {
		t.Fatalf("commit: %v", err)
	}
	return db
}

// eagerResult: the materialized (Execute) rows + total cost — the oracle the streaming cursor matches.
func eagerResult(t *testing.T, s *Session, sql string) ([][]Value, int64) {
	t.Helper()
	out, err := queryOutcome(s, sql, nil)
	if err != nil {
		t.Fatalf("execute %q: %v", sql, err)
	}
	if out.Kind != outcomeQuery {
		t.Fatalf("not a query: %q", sql)
	}
	return out.Rows, out.Cost
}

// streamResult: the streaming (Query) rows, fully drained, + final cost.
func streamResult(t *testing.T, s *Session, sql string) ([][]Value, int64) {
	t.Helper()
	rows, err := s.queryValues(sql, nil)
	if err != nil {
		t.Fatalf("query %q: %v", sql, err)
	}
	var out [][]Value
	for rows.Next() {
		out = append(out, rows.Row())
	}
	if err := rows.Err(); err != nil {
		t.Fatalf("drain %q: %v", sql, err)
	}
	cost := rows.Cost()
	_ = rows.Close()
	return out, cost
}

// Every streamable shape: Query (lazy) must equal Execute (eager) on rows AND total cost.
// (rowsEqual / valueEqual are shared test helpers — value-canonical, NULL-safe — from spill_test.go.)
func TestStreamingMatchesEager(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 100)
	s := db.Session(SessionOptions{})
	defer s.Close()
	for _, sql := range []string{
		"SELECT id, v FROM t LIMIT 5",
		"SELECT id, v FROM t LIMIT 5 OFFSET 10",
		"SELECT id, v FROM t ORDER BY id",
		"SELECT id, v FROM t ORDER BY id LIMIT 7",
		"SELECT id, v FROM t ORDER BY id DESC LIMIT 7",
		"SELECT id, v FROM t WHERE v > 500 ORDER BY id",
		"SELECT id FROM t WHERE id >= 90 ORDER BY id",
		"SELECT v FROM t ORDER BY id LIMIT 3",
		"SELECT id, v + 1 FROM t ORDER BY id LIMIT 4",
		"SELECT id FROM t WHERE id = 9999", // empty
	} {
		er, ec := eagerResult(t, s, sql)
		sr, sc := streamResult(t, s, sql)
		if !rowsEqual(sr, er) {
			t.Fatalf("rows mismatch %q:\n eager=%v\n stream=%v", sql, er, sr)
		}
		if sc != ec {
			t.Fatalf("cost mismatch %q: eager=%d stream=%d", sql, ec, sc)
		}
	}
}

// A non-streamable shape still works through Query — it falls back to the buffered cursor.
func TestNonStreamableFallsBack(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 20)
	s := db.Session(SessionOptions{})
	defer s.Close()
	for _, sql := range []string{
		"SELECT count(*) FROM t",
		"SELECT v FROM t ORDER BY v",
		"SELECT DISTINCT v FROM t",
		"SELECT a.id FROM t a JOIN t b USING (id)",
	} {
		er, ec := eagerResult(t, s, sql)
		sr, sc := streamResult(t, s, sql)
		if !rowsEqual(sr, er) {
			t.Fatalf("rows mismatch %q:\n eager=%v\n stream=%v", sql, er, sr)
		}
		if sc != ec {
			t.Fatalf("cost mismatch %q: eager=%d stream=%d", sql, ec, sc)
		}
	}
}

// Early exit (§6): pulling only a prefix does LESS work than draining — fewer storage_row_read charges.
func TestStreamingEarlyExitChargesLess(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 1000)
	s := db.Session(SessionOptions{})
	defer s.Close()

	_, fullCost := streamResult(t, s, "SELECT id FROM t ORDER BY id")

	rows, err := s.queryValues("SELECT id FROM t ORDER BY id", nil)
	if err != nil {
		t.Fatal(err)
	}
	var prefix []int64
	for i := 0; i < 3 && rows.Next(); i++ {
		prefix = append(prefix, rows.Row()[0].Int)
	}
	partial := rows.Cost()
	_ = rows.Close()

	if len(prefix) != 3 || prefix[0] != 1 || prefix[1] != 2 || prefix[2] != 3 {
		t.Fatalf("early pull prefix = %v, want [1 2 3]", prefix)
	}
	if partial >= fullCost {
		t.Fatalf("early exit must charge less: partial=%d full=%d", partial, fullCost)
	}
}

// Snapshot pinning (§5): a streaming cursor reads the snapshot it opened on even as a concurrent writer
// commits, and the watermark holds at its version until it is closed.
func TestStreamingCursorPinsSnapshot(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 3) // version 1, ids 1..=3
	if db.Version() != 1 || db.OldestLiveTxid() != 1 {
		t.Fatalf("seed: version=%d oldest=%d", db.Version(), db.OldestLiveTxid())
	}
	reader := db.Session(SessionOptions{})
	defer reader.Close()

	rows, err := reader.queryValues("SELECT id FROM t ORDER BY id", nil)
	if err != nil {
		t.Fatal(err)
	}
	if !rows.Next() || rows.Row()[0].Int != 1 {
		t.Fatalf("first row != 1")
	}
	if db.OldestLiveTxid() != 1 {
		t.Fatalf("open cursor must pin v1, oldest=%d", db.OldestLiveTxid())
	}

	// A concurrent writer repeatedly rebuilds the same rightmost working leaf while the cursor is
	// open. This is the focused committed-root alias guard for insert-transient work: the pinned
	// root must remain byte/value-stable through every mutation, then a fresh reader must see them.
	w := db.WriteSession()
	for id := int64(4); id <= 67; id++ {
		if _, err := queryOutcome(w, fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", id, id*10), nil); err != nil {
			t.Fatal(err)
		}
	}
	if err := w.Commit(); err != nil {
		t.Fatal(err)
	}
	if db.Version() != 2 || db.OldestLiveTxid() != 1 {
		t.Fatalf("watermark must hold at the cursor's pin: version=%d oldest=%d", db.Version(), db.OldestLiveTxid())
	}

	// Draining the rest sees ONLY the v1 snapshot (ids 2, 3) — not the writer's rows.
	var rest []int64
	for rows.Next() {
		rest = append(rest, rows.Row()[0].Int)
	}
	if err := rows.Err(); err != nil {
		t.Fatal(err)
	}
	if len(rest) != 2 || rest[0] != 2 || rest[1] != 3 {
		t.Fatalf("frozen snapshot rest = %v, want [2 3]", rest)
	}

	// Closing the cursor releases the pin; the watermark advances.
	_ = rows.Close()
	if db.OldestLiveTxid() != 2 {
		t.Fatalf("closed cursor must release its pin, oldest=%d", db.OldestLiveTxid())
	}

	// A fresh streaming read sees every committed row.
	fresh, _ := streamResult(t, reader, "SELECT id FROM t ORDER BY id")
	if len(fresh) != 67 || fresh[0][0].Int != 1 || fresh[66][0].Int != 67 {
		t.Fatalf("fresh read endpoints/length = %v/%v/%d, want 1/67/67", fresh[0][0], fresh[len(fresh)-1][0], len(fresh))
	}
}

// A cursor opened inside a write transaction owns its working root. Later writes through the same
// session remain visible to later statements but never leak into the open cursor.
func TestWriteTransactionCursorStableAcrossLaterInserts(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 3)
	writer := db.Session(SessionOptions{})
	defer writer.Close()
	if err := writer.Begin(true); err != nil {
		t.Fatal(err)
	}
	if _, err := queryOutcome(writer, "INSERT INTO t VALUES (4, 40)", nil); err != nil {
		t.Fatal(err)
	}
	pinned, err := writer.queryValues("SELECT id FROM t ORDER BY id", nil)
	if err != nil {
		t.Fatal(err)
	}
	if !pinned.Next() || pinned.Row()[0].Int != 1 {
		t.Fatal("first pinned row != 1")
	}
	for id := int64(5); id <= 67; id++ {
		if _, err := queryOutcome(writer, fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", id, id*10), nil); err != nil {
			t.Fatal(err)
		}
	}

	latest, _ := eagerResult(t, writer, "SELECT count(*) FROM t")
	if len(latest) != 1 || latest[0][0].Int != 67 {
		t.Fatalf("latest working count = %v, want 67", latest)
	}
	var rest []int64
	for pinned.Next() {
		rest = append(rest, pinned.Row()[0].Int)
	}
	if err := pinned.Err(); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(rest, []int64{2, 3, 4}) {
		t.Fatalf("pinned cursor rest = %v, want [2 3 4]", rest)
	}
	_ = pinned.Close()
	if err := writer.Commit(); err != nil {
		t.Fatal(err)
	}
	fresh := db.Session(SessionOptions{})
	defer fresh.Close()
	got, _ := eagerResult(t, fresh, "SELECT count(*) FROM t")
	if len(got) != 1 || got[0][0].Int != 67 {
		t.Fatalf("fresh committed count = %v, want 67", got)
	}
}

// A mid-drain cost-ceiling abort (§6): the 54P01 surfaces during iteration via Err(), not at Query().
func TestStreamingMidDrainCostAbort(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 1000)
	s := db.Session(SessionOptions{MaxCost: 50})
	defer s.Close()
	rows, err := s.queryValues("SELECT id FROM t ORDER BY id", nil)
	if err != nil {
		t.Fatalf("query (build) must not abort: %v", err)
	}
	n := 0
	for rows.Next() {
		n++
		if n > 10000 {
			t.Fatal("the cost ceiling should have aborted the drain")
		}
	}
	err = rows.Err()
	if err == nil {
		t.Fatal("a mid-drain cost abort must surface via Err()")
	}
	if ee, ok := err.(*EngineError); !ok || ee.Code() != "54P01" {
		t.Fatalf("abort = %v, want a 54P01 cost-limit error", err)
	}
	_ = rows.Close()
}

// The bare Database.queryValues convenience streams too: the transient mint-a-session does not strand
// the cursor (it owns its snapshot).
func TestDatabaseQueryConvenienceStreams(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 50)
	rows, err := db.queryValues("SELECT id, v FROM t ORDER BY id LIMIT 4", nil)
	if err != nil {
		t.Fatal(err)
	}
	var got [][]int64
	for rows.Next() {
		r := rows.Row()
		got = append(got, []int64{r[0].Int, r[1].Int})
	}
	if err := rows.Err(); err != nil {
		t.Fatal(err)
	}
	_ = rows.Close()
	want := [][]int64{{1, 10}, {2, 20}, {3, 30}, {4, 40}}
	if fmt.Sprint(got) != fmt.Sprint(want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}

// The bare-handle Database.Query path pins the reader-liveness watermark exactly like a Session query
// (streaming.md §7 closing note): the fresh per-call session's provisional pin transfers to the Rows,
// so a held bare-handle cursor holds oldestLiveTxid, keeps within-session reclamation (v25) from
// recycling its snapshot's pages under compacting commit churn, and releases the watermark on Close.
// File-backed with a tiny page size so the churn actually orphans pages and accumulates a reusable
// free-list (an in-memory db cannot exercise the persisted-free-list reuse path).
func TestBareHandleQueryPinsWatermarkUnderReclamation(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "barehandle.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, PageSize: 256, SkipFsync: true})
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	defer db.Close()
	execDB(t, db, "CREATE TABLE t (id i64 PRIMARY KEY, v i64)")
	// A multi-leaf tree; every churn commit below rewrites all of it (whole-table UPDATE), orphaning
	// the prior tree so within-session compaction accumulates a reusable free-list.
	for i := 1; i <= 120; i++ {
		execDB(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", i, i*10))
	}
	v0 := db.Version()
	if got := db.OldestLiveTxid(); got != v0 {
		t.Fatalf("idle watermark = %d, want the committed version %d", got, v0)
	}

	// Open a bare-handle streaming cursor (the transient session closes before Query returns; the
	// pin rides the Rows) and pull ONE row so the scan is live mid-tree.
	rows, err := db.queryValues("SELECT id, v FROM t ORDER BY id", nil)
	if err != nil {
		t.Fatalf("query: %v", err)
	}
	if !rows.Next() {
		t.Fatalf("no first row (rows.Err=%v)", rows.Err())
	}
	if r := rows.Row(); r[0].Int != 1 || r[1].Int != 10 {
		t.Fatalf("first row = (%d, %d), want (1, 10)", r[0].Int, r[1].Int)
	}
	if got := db.OldestLiveTxid(); got != v0 {
		t.Fatalf("open bare-handle cursor: watermark = %d, want its pin %d", got, v0)
	}

	// Churn: whole-table UPDATE commits through the same bare handle — each orphans every leaf plus
	// the spine, so on an ungated path reuse would recycle the cursor's pinned pages.
	for i := 0; i < 150; i++ {
		execDB(t, db, "UPDATE t SET v = v + 1")
	}
	if got := db.OldestLiveTxid(); got != v0 {
		t.Fatalf("watermark advanced to %d under churn despite the held cursor pin %d", got, v0)
	}
	// The gate's observable contract: with the pin held, compaction defers wholesale (canReclaim needs
	// oldest_live == the new version), so the free-list generation must never pass the pin.
	if gen := db.core.storage.freeGenTxid; gen > v0 {
		t.Fatalf("free-list generation %d advanced past the held pin %d — reclamation ran under a live reader", gen, v0)
	}

	// Drain: the cursor must see EXACTLY its frozen snapshot (v = id * 10), untouched by the churn.
	want := int64(2)
	for rows.Next() {
		r := rows.Row()
		if r[0].Int != want || r[1].Int != want*10 {
			t.Fatalf("SNAPSHOT ISOLATION VIOLATED: drained (%d, %d), want (%d, %d) (the cursor's pages were reclaimed and overwritten)",
				r[0].Int, r[1].Int, want, want*10)
		}
		want++
	}
	if err := rows.Err(); err != nil {
		t.Fatalf("drain: %v", err)
	}
	if want != 121 {
		t.Fatalf("drained through id %d, want the full pinned snapshot 2..=120", want-1)
	}
	_ = rows.Close()
	if got, v := db.OldestLiveTxid(), db.Version(); got != v {
		t.Fatalf("closed cursor: watermark = %d, want the committed version %d", got, v)
	}

	// Self-validation (not vacuous): the first commit after the pin releases compacts the deferred
	// churn garbage — the generation advances past v0, proving the churn produced real orphans and
	// the hold above was the gate at work, not a lack of garbage.
	execDB(t, db, "UPDATE t SET v = v + 1")
	if gen := db.core.storage.freeGenTxid; gen <= v0 {
		t.Fatalf("post-close commit did not compact (freeGenTxid=%d <= %d) — the churn never produced gated garbage", gen, v0)
	}
}

// ---- S4: the lazy BUFFERED cursor (a blocking plan; streaming.md §4) ------------------------------

// Every blocking shape (aggregate / non-PK ORDER BY / DISTINCT / window / join / GROUP BY): Query (the
// lazy buffered cursor) must equal Execute (eager) on rows AND total cost under full drain (§6). These
// all route through tryBufferedQuery → bufferedScanCursor, not the streaming fast lane.
func TestBufferedMatchesEager(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 40)
	s := db.Session(SessionOptions{})
	defer s.Close()
	for _, sql := range []string{
		"SELECT count(*) FROM t",                                        // whole-table aggregate (Final, 1 row)
		"SELECT sum(v), avg(v), min(id) FROM t",                         // multi-aggregate
		"SELECT v FROM t ORDER BY v",                                    // ORDER BY the PK scan does NOT satisfy (Final sort)
		"SELECT v FROM t ORDER BY v DESC LIMIT 6",                       // top-N over a non-PK sort
		"SELECT DISTINCT v FROM t ORDER BY v",                           // no-PK DISTINCT then sort (Identity)
		"SELECT id, v + 1 FROM t GROUP BY id, v ORDER BY id",            // GROUP BY + projection expr (Project)
		"SELECT id, v FROM t GROUP BY id, v HAVING v > 200 ORDER BY id", // HAVING
		"SELECT a.id, b.v FROM t a JOIN t b USING (id) ORDER BY a.id",   // join + ORDER BY (Project)
		"SELECT sum(v) OVER (ORDER BY id) FROM t ORDER BY id",           // window function
	} {
		er, ec := eagerResult(t, s, sql)
		sr, sc := streamResult(t, s, sql)
		if !rowsEqual(sr, er) {
			t.Fatalf("rows mismatch %q:\n eager=%v\n stream=%v", sql, er, sr)
		}
		if sc != ec {
			t.Fatalf("cost mismatch %q: eager=%d stream=%d", sql, ec, sc)
		}
	}
}

// Early exit over a buffered cursor in Project mode (§4): the blocking part (scan + group + sort) runs
// in full on the first pull, but a caller that stops after a prefix skips the PROJECTION of every row it
// never pulls — so it charges LESS than a full drain. The top-N-over-the-buffer win.
func TestBufferedEarlyExitChargesLess(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 1000)
	s := db.Session(SessionOptions{})
	defer s.Close()

	sql := "SELECT id, v + 1 FROM t GROUP BY id, v ORDER BY id"
	fullRows, fullCost := streamResult(t, s, sql)
	if len(fullRows) != 1000 {
		t.Fatalf("full drain = %d rows, want 1000", len(fullRows))
	}

	rows, err := s.queryValues(sql, nil)
	if err != nil {
		t.Fatal(err)
	}
	var prefix [][]int64
	for i := 0; i < 3 && rows.Next(); i++ {
		r := rows.Row()
		prefix = append(prefix, []int64{r[0].Int, r[1].Int})
	}
	partial := rows.Cost()
	_ = rows.Close()

	want := [][]int64{{1, 11}, {2, 21}, {3, 31}}
	if fmt.Sprint(prefix) != fmt.Sprint(want) {
		t.Fatalf("early pull prefix = %v, want %v", prefix, want)
	}
	if partial >= fullCost {
		t.Fatalf("early exit over a buffered cursor must charge less: partial=%d full=%d", partial, fullCost)
	}
}

// Snapshot pinning (§5) for the buffered cursor: it captures its snapshot at Query time (the blocking
// part materializes from THAT snapshot on first pull), so a concurrent writer's rows never appear; the
// watermark holds at the cursor's version until it is closed.
func TestBufferedCursorPinsSnapshot(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 3) // version 1, ids 1..=3
	if db.OldestLiveTxid() != 1 {
		t.Fatalf("seed: oldest=%d", db.OldestLiveTxid())
	}
	reader := db.Session(SessionOptions{})
	defer reader.Close()

	// A blocking query (ORDER BY v — not PK order) → the buffered cursor. Pull one row (runs the
	// blocking part over the v1 snapshot), keep the cursor live.
	rows, err := reader.queryValues("SELECT v FROM t ORDER BY v", nil)
	if err != nil {
		t.Fatal(err)
	}
	if !rows.Next() || rows.Row()[0].Int != 10 {
		t.Fatalf("first row != 10")
	}
	if db.OldestLiveTxid() != 1 {
		t.Fatalf("open buffered cursor must pin v1, oldest=%d", db.OldestLiveTxid())
	}

	w := db.WriteSession()
	if _, err := queryOutcome(w, "INSERT INTO t VALUES (4, 40), (5, 50)", nil); err != nil {
		t.Fatal(err)
	}
	if err := w.Commit(); err != nil {
		t.Fatal(err)
	}
	if db.Version() != 2 || db.OldestLiveTxid() != 1 {
		t.Fatalf("watermark must hold at the cursor's pin: version=%d oldest=%d", db.Version(), db.OldestLiveTxid())
	}

	// Draining the rest sees ONLY the v1 snapshot (v = 20, 30) — not the writer's rows.
	var rest []int64
	for rows.Next() {
		rest = append(rest, rows.Row()[0].Int)
	}
	if err := rows.Err(); err != nil {
		t.Fatal(err)
	}
	if len(rest) != 2 || rest[0] != 20 || rest[1] != 30 {
		t.Fatalf("frozen snapshot rest = %v, want [20 30]", rest)
	}

	_ = rows.Close()
	if db.OldestLiveTxid() != 2 {
		t.Fatalf("closed buffered cursor must release its pin, oldest=%d", db.OldestLiveTxid())
	}
}

// A mid-drain cost-ceiling abort (§6) for the buffered cursor: building the cursor does NOT run the
// blocking part (deferred to the first pull), so Query succeeds and the 54P01 surfaces during iteration.
func TestBufferedMidDrainCostAbort(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 1000)
	s := db.Session(SessionOptions{MaxCost: 50})
	defer s.Close()
	rows, err := s.queryValues("SELECT v FROM t ORDER BY v", nil)
	if err != nil {
		t.Fatalf("query (build) must not abort: %v", err)
	}
	n := 0
	for rows.Next() {
		n++
		if n > 10000 {
			t.Fatal("the cost ceiling should have aborted the drain")
		}
	}
	err = rows.Err()
	if err == nil {
		t.Fatal("a mid-drain cost abort must surface via Err()")
	}
	if ee, ok := err.(*EngineError); !ok || ee.Code() != "54P01" {
		t.Fatalf("abort = %v, want a 54P01 cost-limit error", err)
	}
	_ = rows.Close()
}

// ---- the lazy streaming-SORT output (emitSorted; streaming.md §4/§7) ------------------------------

// Every streaming-external-sort shape (a single-table non-PK ORDER BY): Query (the lazy emitSorted
// drive — pulling the sortedRows iterator one row at a time) must equal Execute (the eager drive of the
// SAME emitter) on rows AND total cost under full drain (§6).
func TestSortedMatchesEager(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 40)
	s := db.Session(SessionOptions{})
	defer s.Close()
	for _, sql := range []string{
		"SELECT v FROM t ORDER BY v",                  // non-PK sort, full output
		"SELECT v FROM t ORDER BY v DESC",             // descending
		"SELECT v FROM t ORDER BY v LIMIT 7",          // top-N window
		"SELECT v FROM t ORDER BY v LIMIT 7 OFFSET 5", // LIMIT + OFFSET window
		"SELECT v FROM t ORDER BY v OFFSET 35",        // OFFSET near the end (tail window)
		"SELECT id, v + 1 FROM t ORDER BY v",          // a projection expression (operator_eval per row)
		"SELECT v FROM t WHERE id > 20 ORDER BY v",    // a residual WHERE filter
		"SELECT v FROM t WHERE id > 99999 ORDER BY v", // empty result
	} {
		er, ec := eagerResult(t, s, sql)
		sr, sc := streamResult(t, s, sql)
		if !rowsEqual(sr, er) {
			t.Fatalf("rows mismatch %q:\n eager=%v\n stream=%v", sql, er, sr)
		}
		if sc != ec {
			t.Fatalf("cost mismatch %q: eager=%d stream=%d", sql, ec, sc)
		}
	}
}

// Early exit over the lazy streaming-sort output (§4/§7) — the headline win of this slice. The sort's
// INPUT is blocking (every row scanned + sorted on the first pull), but the OUTPUT is now yielded from
// the sortedRows iterator one row at a time, so a caller that stops after a prefix skips the row_produced
// + projection of every windowed row it never pulls — charging LESS than a full drain. (Before this
// slice the sort output was an emitFinal, fully built + charged on the first pull, so an early exit
// charged the SAME — this test is what distinguishes the new behavior.)
func TestSortedEarlyExitChargesLess(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 1000)
	s := db.Session(SessionOptions{})
	defer s.Close()

	sql := "SELECT v FROM t ORDER BY v" // non-PK ORDER BY, no LIMIT → a 1000-row lazy Sorted output
	fullRows, fullCost := streamResult(t, s, sql)
	if len(fullRows) != 1000 {
		t.Fatalf("full drain = %d rows, want 1000", len(fullRows))
	}

	rows, err := s.queryValues(sql, nil)
	if err != nil {
		t.Fatal(err)
	}
	var prefix []int64
	for i := 0; i < 3 && rows.Next(); i++ {
		prefix = append(prefix, rows.Row()[0].Int)
	}
	partial := rows.Cost()
	_ = rows.Close()

	if fmt.Sprint(prefix) != fmt.Sprint([]int64{10, 20, 30}) {
		t.Fatalf("early pull prefix = %v, want [10 20 30]", prefix)
	}
	if partial >= fullCost {
		t.Fatalf("early exit over the lazy sort output must charge less: partial=%d full=%d", partial, fullCost)
	}
}

// The lazy streaming-sort output over the SPILLING merge path (sortedRows.merge): a file-backed database
// under a tiny workMem forces many spilled runs + a k-way merge. A full lazy drain must match the eager
// result (rows + cost — spill is invariant, spill.md §6), and an early exit must yield exactly the prefix
// while leaving NO spill temp file behind (the cursor's close releases undrained runs — Go has no
// destructor, §5).
func TestSortedSpillMergeStreamsLazily(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	path := filepath.Join(dir, "sorted_spill_lazy.jed")

	countSpillFiles := func() int {
		entries, err := os.ReadDir(dir)
		if err != nil {
			t.Fatal(err)
		}
		n := 0
		for _, e := range entries {
			if strings.HasPrefix(e.Name(), "jed-spill-") {
				n++
			}
		}
		return n
	}

	db, err := CreateDatabase(CreateOptions{Path: path, PageSize: DefaultPageSize, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	db.setSpillDirForTest(dir) // isolate live-run assertions from parallel OS-temp spills
	w := db.WriteSession()
	if _, err := queryOutcome(w, "CREATE TABLE t (id i32 PRIMARY KEY, k i32)", nil); err != nil {
		t.Fatal(err)
	}
	for id := int64(0); id < 200; id++ {
		k := (id * 48271) % 100 // scrambled key with many duplicates
		if _, err := queryOutcome(w, fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", id, k), nil); err != nil {
			t.Fatal(err)
		}
	}
	if err := w.Commit(); err != nil {
		t.Fatal(err)
	}

	// Eager oracle: a default-workMem session never spills 200 small rows (in-memory sort).
	sql := "SELECT id, k FROM t ORDER BY k, id"
	oracle := db.Session(SessionOptions{})
	er, ec := eagerResult(t, oracle, sql)
	oracle.Close()

	// A finite top-k whose shared fixed-row estimate fits workMem bypasses the external sorter:
	// after the blocking first pull there is no live spill run. Lowering the budget below K's
	// estimate falls back to the existing sorter, whose undrained merge keeps runs live.
	fit := db.Session(SessionOptions{})
	fit.SetWorkMem(512) // K=5 × (8 + 2×40) = 440 bytes
	fitRows, err := fit.queryValues(sql+" LIMIT 5", nil)
	if err != nil || !fitRows.Next() {
		t.Fatalf("top-k first pull: %v", err)
	}
	if n := countSpillFiles(); n != 0 {
		t.Fatalf("fitting top-k created %d spill files", n)
	}
	_ = fitRows.Close()
	fit.Close()

	fallback := db.Session(SessionOptions{})
	fallback.SetWorkMem(128)
	fallbackRows, err := fallback.queryValues(sql+" LIMIT 5", nil)
	if err != nil || !fallbackRows.Next() {
		t.Fatalf("fallback first pull: %v", err)
	}
	if n := countSpillFiles(); n == 0 {
		t.Fatal("top-k over workMem must fall back to the external sorter")
	}
	_ = fallbackRows.Close()
	fallback.Close()
	if n := countSpillFiles(); n != 0 {
		t.Fatalf("fallback close leaked %d spill files", n)
	}

	// Full lazy drain under a tiny workMem (forces spill + merge): rows + cost match the oracle.
	s := db.Session(SessionOptions{})
	s.SetWorkMem(128) // ~2-3 rows per run → dozens of runs + a deep merge
	sr, sc := streamResult(t, s, sql)
	if !rowsEqual(sr, er) {
		t.Fatalf("spilling lazy drain rows must match eager")
	}
	if sc != ec {
		t.Fatalf("spilling lazy drain cost must match eager: stream=%d eager=%d", sc, ec)
	}
	s.Close()
	if n := countSpillFiles(); n != 0 {
		t.Fatalf("a full drain leaked %d spill files", n)
	}

	// Early exit over the merge: pull a prefix, then close the cursor. close releases the undrained
	// merge's run files, so none leak.
	s2 := db.Session(SessionOptions{})
	s2.SetWorkMem(128)
	rows, err := s2.queryValues(sql, nil)
	if err != nil {
		t.Fatal(err)
	}
	var got [][]Value
	for i := 0; i < 5 && rows.Next(); i++ {
		got = append(got, rows.Row())
	}
	if !rowsEqual(got, er[:5]) {
		t.Fatalf("early pull prefix must match the eager order")
	}
	_ = rows.Close()
	s2.Close()
	if n := countSpillFiles(); n != 0 {
		t.Fatalf("an early exit leaked %d spill files", n)
	}
}

// ---- the lazy DEFERRED cursor (a top-level set-op / WITH; streaming.md §7) ------------------------

// Every top-level set operation / pure-query WITH: Query (the lazy deferredCursor) must equal Execute
// (eager) on rows AND total cost under full drain (§6). These route through tryDeferredQuery, which
// reuses the eager runSetOp / runWith verbatim, so the rows + cost are identical by construction (the
// unordered shapes are deterministic here — same snapshot, same code path).
func TestDeferredMatchesEager(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 20)
	s := db.Session(SessionOptions{})
	defer s.Close()
	for _, sql := range []string{
		// Set operations (every kind), with and without a trailing ORDER BY.
		"SELECT v FROM t WHERE id <= 3 UNION SELECT v FROM t WHERE id >= 18 ORDER BY v",
		"SELECT id FROM t UNION ALL SELECT id FROM t ORDER BY id",
		"SELECT v FROM t WHERE id <= 10 INTERSECT SELECT v FROM t WHERE id >= 5 ORDER BY v",
		"SELECT v FROM t EXCEPT SELECT v FROM t WHERE id <= 12 ORDER BY v",
		"SELECT v FROM t WHERE id = 1 UNION SELECT v FROM t WHERE id = 2", // unordered, still deterministic
		// Pure-query WITH: a CTE feeding a scan, an aggregate, and a join.
		"WITH x AS (SELECT id, v FROM t WHERE v > 100) SELECT id, v FROM x ORDER BY id",
		"WITH x AS (SELECT id FROM t) SELECT count(*) FROM x",
		"WITH a AS (SELECT id, v FROM t WHERE id <= 5) SELECT a.id, a.v FROM a JOIN t USING (id) ORDER BY a.id",
		// A recursive WITH (the working-table fixpoint runs entirely on the first pull).
		"WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 8) SELECT n FROM c ORDER BY n",
		// A WITH whose body is itself a set operation.
		"WITH x AS (SELECT v FROM t) SELECT v FROM x WHERE v <= 50 UNION SELECT v FROM x WHERE v >= 180 ORDER BY v",
	} {
		er, ec := eagerResult(t, s, sql)
		sr, sc := streamResult(t, s, sql)
		if !rowsEqual(sr, er) {
			t.Fatalf("rows mismatch %q:\n eager=%v\n stream=%v", sql, er, sr)
		}
		if sc != ec {
			t.Fatalf("cost mismatch %q: eager=%d stream=%d", sql, ec, sc)
		}
	}
}

// The deferred cursor's defining trait (§7): a set-op / WITH has no per-row top-level projection to
// defer, so the WHOLE query runs on the FIRST pull — unlike S3/S4, an early exit charges the SAME as a
// full drain (the only win is lazy-yield, not early-exit). This pins that the cost after one pull is
// already final.
func TestDeferredRunsFullyOnFirstPull(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 100)
	s := db.Session(SessionOptions{})
	defer s.Close()
	sql := "SELECT id FROM t UNION ALL SELECT id FROM t ORDER BY id"

	fullRows, fullCost := streamResult(t, s, sql)
	if len(fullRows) != 200 {
		t.Fatalf("full drain = %d rows, want 200", len(fullRows))
	}

	rows, err := s.queryValues(sql, nil)
	if err != nil {
		t.Fatal(err)
	}
	if !rows.Next() {
		t.Fatal("expected at least one row")
	}
	afterOne := rows.Cost()
	if err := rows.Err(); err != nil {
		t.Fatal(err)
	}
	_ = rows.Close()
	if afterOne != fullCost {
		t.Fatalf("a deferred set-op/WITH accrues its full cost on the first pull: afterOne=%d full=%d", afterOne, fullCost)
	}
}

// Snapshot pinning (§5) for the deferred cursor: it captures its snapshot at Query time and runs the
// set op on the first pull over THAT snapshot, so a concurrent writer's rows never appear; the
// watermark holds at the cursor's version until it is closed.
func TestDeferredCursorPinsSnapshot(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 3) // version 1, ids 1..=3
	if db.OldestLiveTxid() != 1 {
		t.Fatalf("seed: oldest=%d", db.OldestLiveTxid())
	}
	reader := db.Session(SessionOptions{})
	defer reader.Close()

	// A top-level UNION → the deferred cursor. Pull one row (runs the set op over the v1 snapshot),
	// keep the cursor live.
	rows, err := reader.queryValues("SELECT v FROM t WHERE id <= 2 UNION SELECT v FROM t WHERE id = 3 ORDER BY v", nil)
	if err != nil {
		t.Fatal(err)
	}
	if !rows.Next() || rows.Row()[0].Int != 10 {
		t.Fatalf("first row != 10")
	}
	if db.OldestLiveTxid() != 1 {
		t.Fatalf("open deferred cursor must pin v1, oldest=%d", db.OldestLiveTxid())
	}

	w := db.WriteSession()
	if _, err := queryOutcome(w, "INSERT INTO t VALUES (4, 40), (5, 50)", nil); err != nil {
		t.Fatal(err)
	}
	if err := w.Commit(); err != nil {
		t.Fatal(err)
	}
	if db.Version() != 2 || db.OldestLiveTxid() != 1 {
		t.Fatalf("watermark must hold at the cursor's pin: version=%d oldest=%d", db.Version(), db.OldestLiveTxid())
	}

	// Draining the rest sees ONLY the v1 snapshot (v = 20, 30) — not the writer's rows.
	var rest []int64
	for rows.Next() {
		rest = append(rest, rows.Row()[0].Int)
	}
	if err := rows.Err(); err != nil {
		t.Fatal(err)
	}
	if len(rest) != 2 || rest[0] != 20 || rest[1] != 30 {
		t.Fatalf("frozen snapshot rest = %v, want [20 30]", rest)
	}

	_ = rows.Close()
	if db.OldestLiveTxid() != 2 {
		t.Fatalf("closed deferred cursor must release its pin, oldest=%d", db.OldestLiveTxid())
	}
}

// A mid-drain cost-ceiling abort (§6) for the deferred cursor: building the cursor does NOT run the
// query (deferred to the first pull), so Query succeeds and the 54P01 surfaces during iteration.
func TestDeferredMidDrainCostAbort(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 1000)
	s := db.Session(SessionOptions{MaxCost: 50})
	defer s.Close()
	rows, err := s.queryValues("SELECT id FROM t UNION ALL SELECT id FROM t ORDER BY id", nil)
	if err != nil {
		t.Fatalf("query (build) must not abort: %v", err)
	}
	n := 0
	for rows.Next() {
		n++
		if n > 10000 {
			t.Fatal("the cost ceiling should have aborted the drain")
		}
	}
	err = rows.Err()
	if err == nil {
		t.Fatal("a mid-drain cost abort must surface via Err()")
	}
	if ee, ok := err.(*EngineError); !ok || ee.Code() != "54P01" {
		t.Fatalf("abort = %v, want a 54P01 cost-limit error", err)
	}
	_ = rows.Close()
}

// A data-modifying WITH (a write) must NOT take the deferred lazy path — it falls back to the
// materialized dispatch (it takes the write gate and commits). Routed through Query, it still returns
// the primary's RETURNING rows correctly.
func TestDeferredSkipsDataModifyingWith(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 5)
	s := db.Session(SessionOptions{})
	defer s.Close()
	// A writable CTE: INSERT … RETURNING fed to the primary. This is stmtIsWrite, so it bypasses
	// tryDeferredQuery and runs through the write path — but Query still surfaces its rows.
	rows, err := s.queryValues("WITH ins AS (INSERT INTO t VALUES (6, 60), (7, 70) RETURNING id) SELECT id FROM ins ORDER BY id", nil)
	if err != nil {
		t.Fatal(err)
	}
	var got []int64
	for rows.Next() {
		got = append(got, rows.Row()[0].Int)
	}
	if err := rows.Err(); err != nil {
		t.Fatal(err)
	}
	_ = rows.Close()
	if len(got) != 2 || got[0] != 6 || got[1] != 7 {
		t.Fatalf("RETURNING rows = %v, want [6 7]", got)
	}
	// The write committed: the rows are now visible.
	after, _ := eagerResult(t, s, "SELECT count(*) FROM t")
	if len(after) != 1 || after[0][0].Int != 7 {
		t.Fatalf("post-write count = %v, want 7", after)
	}
}

// ---- prepared-statement streaming (the prepared query path; streaming.md §7) ----------------------
//
// A prepared query (Prepare + the handle's QueryPrepared seam) routes its parsed AST through the SAME
// lazy lanes as the ad-hoc Query — so a prepared SELECT streams (single-table pull / blocking-buffer /
// deferred set-op), pins its snapshot in the watermark, and offers the early-exit win, all identical
// to a one-shot query.

// preparedStreamResult: a prepared query's rows, fully drained, + final cost.
func preparedStreamResult(t *testing.T, s *Session, sql string, params []Value) ([][]Value, int64) {
	t.Helper()
	stmt, err := s.Prepare(sql)
	if err != nil {
		t.Fatalf("prepare %q: %v", sql, err)
	}
	rows, err := s.queryStmt(stmt.ast, params, &stmt.sc)
	if err != nil {
		t.Fatalf("query prepared %q: %v", sql, err)
	}
	var out [][]Value
	for rows.Next() {
		out = append(out, rows.Row())
	}
	if err := rows.Err(); err != nil {
		t.Fatalf("drain prepared %q: %v", sql, err)
	}
	cost := rows.Cost()
	_ = rows.Close()
	return out, cost
}

// A fully-drained prepared query yields the IDENTICAL rows + total cost as the ad-hoc Query (and thus
// Execute, §6), across every lane — streaming, buffered, and deferred.
func TestPreparedQueryMatchesStreamed(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 100)
	s := db.Session(SessionOptions{})
	defer s.Close()
	for _, sql := range []string{
		"SELECT id, v FROM t LIMIT 5",                                                   // streaming (LIMIT short-circuit)
		"SELECT id, v FROM t ORDER BY id LIMIT 7",                                       // streaming (PK-ordered)
		"SELECT v FROM t ORDER BY v LIMIT 6",                                            // buffered (non-PK sort, top-N)
		"SELECT count(*) FROM t",                                                        // buffered (aggregate)
		"SELECT DISTINCT v FROM t ORDER BY v",                                           // buffered (DISTINCT + sort)
		"SELECT v FROM t WHERE id <= 3 UNION SELECT v FROM t WHERE id >= 98 ORDER BY v", // deferred (set op)
		"WITH x AS (SELECT id, v FROM t WHERE v > 500) SELECT id, v FROM x ORDER BY id", // deferred (WITH)
	} {
		er, ec := streamResult(t, s, sql)
		pr, pc := preparedStreamResult(t, s, sql, nil)
		if !rowsEqual(pr, er) {
			t.Fatalf("prepared rows mismatch %q:\n stream=%v\n prepared=%v", sql, er, pr)
		}
		if pc != ec {
			t.Fatalf("prepared cost mismatch %q: stream=%d prepared=%d", sql, ec, pc)
		}
	}
}

// A prepared query binds $N params and streams: the bound prepared run matches the ad-hoc bound Query
// on rows + cost, and the statement is reusable across runs with different params.
func TestPreparedQueryBindsParamsAndStreams(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 100)
	s := db.Session(SessionOptions{})
	defer s.Close()
	sql := "SELECT id, v FROM t WHERE id >= $1 ORDER BY id LIMIT 4"

	adHoc, ac := streamResultParams(t, s, sql, []Value{IntValue(90)})
	pr, pc := preparedStreamResult(t, s, sql, []Value{IntValue(90)})
	want := [][]int64{{90, 900}, {91, 910}, {92, 920}, {93, 930}}
	if !intRowsEqual(pr, want) {
		t.Fatalf("prepared bound rows = %v, want %v", pr, want)
	}
	if !rowsEqual(pr, adHoc) || pc != ac {
		t.Fatalf("prepared bound run must match ad-hoc: rowsEqual=%v cost prepared=%d adhoc=%d", rowsEqual(pr, adHoc), pc, ac)
	}
	// Reusable: a second run with a different param re-streams.
	pr2, _ := preparedStreamResult(t, s, sql, []Value{IntValue(1)})
	if len(pr2) != 4 || pr2[0][0].Int != 1 {
		t.Fatalf("reused prepared run = %v, want first id 1", pr2)
	}
}

// Early exit (§6) on the prepared path: pulling only a prefix charges LESS than a full drain — the
// streaming win now reaches prepared queries.
func TestPreparedQueryEarlyExitChargesLess(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 1000)
	s := db.Session(SessionOptions{})
	defer s.Close()
	stmt, err := s.Prepare("SELECT id FROM t ORDER BY id")
	if err != nil {
		t.Fatal(err)
	}
	full, err := s.queryStmt(stmt.ast, nil, &stmt.sc)
	if err != nil {
		t.Fatal(err)
	}
	for full.Next() {
	}
	if err := full.Err(); err != nil {
		t.Fatal(err)
	}
	fullCost := full.Cost()
	_ = full.Close()

	rows, err := s.queryStmt(stmt.ast, nil, &stmt.sc)
	if err != nil {
		t.Fatal(err)
	}
	var prefix []int64
	for i := 0; i < 3 && rows.Next(); i++ {
		prefix = append(prefix, rows.Row()[0].Int)
	}
	partial := rows.Cost()
	_ = rows.Close()
	if len(prefix) != 3 || prefix[0] != 1 || prefix[1] != 2 || prefix[2] != 3 {
		t.Fatalf("early pull prefix = %v, want [1 2 3]", prefix)
	}
	if partial >= fullCost {
		t.Fatalf("prepared early exit must charge less: partial=%d full=%d", partial, fullCost)
	}
}

// Snapshot pinning (§5) on the prepared path: an open prepared cursor pins its version in the
// watermark, sees only its open-time snapshot as a writer commits, and releases on close.
func TestPreparedQueryPinsSnapshot(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 3)
	if db.OldestLiveTxid() != 1 {
		t.Fatalf("seed oldest=%d", db.OldestLiveTxid())
	}
	reader := db.Session(SessionOptions{})
	defer reader.Close()
	stmt, err := reader.Prepare("SELECT id FROM t ORDER BY id")
	if err != nil {
		t.Fatal(err)
	}
	rows, err := reader.queryStmt(stmt.ast, nil, &stmt.sc)
	if err != nil {
		t.Fatal(err)
	}
	if !rows.Next() || rows.Row()[0].Int != 1 {
		t.Fatal("first row != 1")
	}
	if db.OldestLiveTxid() != 1 {
		t.Fatalf("open prepared cursor must pin v1, oldest=%d", db.OldestLiveTxid())
	}

	w := db.WriteSession()
	if _, err := queryOutcome(w, "INSERT INTO t VALUES (4, 40), (5, 50)", nil); err != nil {
		t.Fatal(err)
	}
	if err := w.Commit(); err != nil {
		t.Fatal(err)
	}
	if db.OldestLiveTxid() != 1 {
		t.Fatalf("watermark must hold at the pin, oldest=%d", db.OldestLiveTxid())
	}

	var rest []int64
	for rows.Next() {
		rest = append(rest, rows.Row()[0].Int)
	}
	if err := rows.Err(); err != nil {
		t.Fatal(err)
	}
	if len(rest) != 2 || rest[0] != 2 || rest[1] != 3 {
		t.Fatalf("frozen snapshot rest = %v, want [2 3]", rest)
	}
	_ = rows.Close()
	if db.OldestLiveTxid() != 2 {
		t.Fatalf("closed prepared cursor must release its pin, oldest=%d", db.OldestLiveTxid())
	}
}

// A mid-drain cost abort (§6) on the prepared path: the 54P01 surfaces during iteration via Err(),
// not at queryValues() — the prepared cursor defers its work like the ad-hoc one.
func TestPreparedQueryMidDrainCostAbort(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 1000)
	s := db.Session(SessionOptions{MaxCost: 50})
	defer s.Close()
	stmt, err := s.Prepare("SELECT id FROM t ORDER BY id")
	if err != nil {
		t.Fatal(err)
	}
	rows, err := s.queryStmt(stmt.ast, nil, &stmt.sc)
	if err != nil {
		t.Fatalf("query (build) must not abort: %v", err)
	}
	n := 0
	for rows.Next() {
		n++
		if n > 10000 {
			t.Fatal("the cost ceiling should have aborted the drain")
		}
	}
	err = rows.Err()
	if ee, ok := err.(*EngineError); !ok || ee.Code() != "54P01" {
		t.Fatalf("abort = %v, want a 54P01 cost-limit error", err)
	}
	_ = rows.Close()
}

// The bare Database.Prepare + Database.QueryPrepared convenience streams too (QueryPrepared mints a
// transient autocommit session per call; the cursor pins its snapshot, so it is not stranded when
// that session closes).
func TestDatabaseQueryPreparedConvenienceStreams(t *testing.T) {
	t.Parallel()
	db := seededKV(t, 50)
	stmt, err := db.Prepare("SELECT id, v FROM t ORDER BY id LIMIT 4")
	if err != nil {
		t.Fatal(err)
	}
	rows, err := db.QueryPrepared(context.Background(), stmt)
	if err != nil {
		t.Fatal(err)
	}
	var got [][]int64
	for rows.Next() {
		r := rows.Row()
		got = append(got, []int64{r[0].Int, r[1].Int})
	}
	if err := rows.Err(); err != nil {
		t.Fatal(err)
	}
	_ = rows.Close()
	want := [][]int64{{1, 10}, {2, 20}, {3, 30}, {4, 40}}
	if fmt.Sprint(got) != fmt.Sprint(want) {
		t.Fatalf("got %v, want %v", got, want)
	}
}

// streamResultParams: the streaming (Query) rows + cost for a parameterized query, fully drained.
func streamResultParams(t *testing.T, s *Session, sql string, params []Value) ([][]Value, int64) {
	t.Helper()
	rows, err := s.queryValues(sql, params)
	if err != nil {
		t.Fatalf("query %q: %v", sql, err)
	}
	var out [][]Value
	for rows.Next() {
		out = append(out, rows.Row())
	}
	if err := rows.Err(); err != nil {
		t.Fatalf("drain %q: %v", sql, err)
	}
	cost := rows.Cost()
	_ = rows.Close()
	return out, cost
}

// intRowsEqual compares int-valued rows against a want matrix.
func intRowsEqual(got [][]Value, want [][]int64) bool {
	if len(got) != len(want) {
		return false
	}
	for i := range got {
		if len(got[i]) != len(want[i]) {
			return false
		}
		for j := range got[i] {
			if got[i][j].Int != want[i][j] {
				return false
			}
		}
	}
	return true
}
