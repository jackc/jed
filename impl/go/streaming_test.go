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
	"fmt"
	"testing"
)

// seededKV builds an in-memory shared db with t(id i32 PK, v i32) holding 1..=n (v = id * 10).
func seededKV(t *testing.T, n int64) *Database {
	t.Helper()
	db := NewDatabase()
	w := db.WriteSession()
	if _, err := w.Execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)", nil); err != nil {
		t.Fatalf("create: %v", err)
	}
	for i := int64(1); i <= n; i++ {
		if _, err := w.Execute(fmt.Sprintf("INSERT INTO t VALUES (%d, %d)", i, i*10), nil); err != nil {
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
	out, err := s.Execute(sql, nil)
	if err != nil {
		t.Fatalf("execute %q: %v", sql, err)
	}
	if out.Kind != OutcomeQuery {
		t.Fatalf("not a query: %q", sql)
	}
	return out.Rows, out.Cost
}

// streamResult: the streaming (Query) rows, fully drained, + final cost.
func streamResult(t *testing.T, s *Session, sql string) ([][]Value, int64) {
	t.Helper()
	rows, err := s.Query(sql, nil)
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
	db := seededKV(t, 1000)
	s := db.Session(SessionOptions{})
	defer s.Close()

	_, fullCost := streamResult(t, s, "SELECT id FROM t ORDER BY id")

	rows, err := s.Query("SELECT id FROM t ORDER BY id", nil)
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
	db := seededKV(t, 3) // version 1, ids 1..=3
	if db.Version() != 1 || db.OldestLiveTxid() != 1 {
		t.Fatalf("seed: version=%d oldest=%d", db.Version(), db.OldestLiveTxid())
	}
	reader := db.Session(SessionOptions{})
	defer reader.Close()

	rows, err := reader.Query("SELECT id FROM t ORDER BY id", nil)
	if err != nil {
		t.Fatal(err)
	}
	if !rows.Next() || rows.Row()[0].Int != 1 {
		t.Fatalf("first row != 1")
	}
	if db.OldestLiveTxid() != 1 {
		t.Fatalf("open cursor must pin v1, oldest=%d", db.OldestLiveTxid())
	}

	// A concurrent writer commits two more rows (version 2) while the cursor is open.
	w := db.WriteSession()
	if _, err := w.Execute("INSERT INTO t VALUES (4, 40), (5, 50)", nil); err != nil {
		t.Fatal(err)
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

	// A fresh streaming read sees the writer's rows.
	fresh, _ := streamResult(t, reader, "SELECT id FROM t ORDER BY id")
	if len(fresh) != 5 {
		t.Fatalf("fresh read = %d rows, want 5", len(fresh))
	}
}

// A mid-drain cost-ceiling abort (§6): the 54P01 surfaces during iteration via Err(), not at Query().
func TestStreamingMidDrainCostAbort(t *testing.T) {
	db := seededKV(t, 1000)
	s := db.Session(SessionOptions{MaxCost: 50})
	defer s.Close()
	rows, err := s.Query("SELECT id FROM t ORDER BY id", nil)
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

// The bare Database.QueryValues convenience streams too: the transient mint-a-session does not strand
// the cursor (it owns its snapshot).
func TestDatabaseQueryConvenienceStreams(t *testing.T) {
	db := seededKV(t, 50)
	rows, err := db.QueryValues("SELECT id, v FROM t ORDER BY id LIMIT 4", nil)
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

// ---- S4: the lazy BUFFERED cursor (a blocking plan; streaming.md §4) ------------------------------

// Every blocking shape (aggregate / non-PK ORDER BY / DISTINCT / window / join / GROUP BY): Query (the
// lazy buffered cursor) must equal Execute (eager) on rows AND total cost under full drain (§6). These
// all route through tryBufferedQuery → bufferedScanCursor, not the streaming fast lane.
func TestBufferedMatchesEager(t *testing.T) {
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
	db := seededKV(t, 1000)
	s := db.Session(SessionOptions{})
	defer s.Close()

	sql := "SELECT id, v + 1 FROM t GROUP BY id, v ORDER BY id"
	fullRows, fullCost := streamResult(t, s, sql)
	if len(fullRows) != 1000 {
		t.Fatalf("full drain = %d rows, want 1000", len(fullRows))
	}

	rows, err := s.Query(sql, nil)
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
	db := seededKV(t, 3) // version 1, ids 1..=3
	if db.OldestLiveTxid() != 1 {
		t.Fatalf("seed: oldest=%d", db.OldestLiveTxid())
	}
	reader := db.Session(SessionOptions{})
	defer reader.Close()

	// A blocking query (ORDER BY v — not PK order) → the buffered cursor. Pull one row (runs the
	// blocking part over the v1 snapshot), keep the cursor live.
	rows, err := reader.Query("SELECT v FROM t ORDER BY v", nil)
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
	if _, err := w.Execute("INSERT INTO t VALUES (4, 40), (5, 50)", nil); err != nil {
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
	db := seededKV(t, 1000)
	s := db.Session(SessionOptions{MaxCost: 50})
	defer s.Close()
	rows, err := s.Query("SELECT v FROM t ORDER BY v", nil)
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
