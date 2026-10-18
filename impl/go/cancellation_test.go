package jed

// Cancellation through the cost meter (spec/design/api.md §11.4). Per-core unit tests, NOT the
// shared corpus: cancellation is timing-dependent (CLAUDE.md §10), so it cannot live there. These
// pin the mechanism deterministically — the meter's Guard() honors the cancel poll, and a flipped
// poll aborts a running statement with 57014 (not only at the cursor boundary).

import (
	"context"
	"fmt"
	"sync"
	"testing"
	"time"
)

// cancelCode extracts a *EngineError's SQLSTATE, failing if err is not one.
func cancelCode(t *testing.T, err error) string {
	t.Helper()
	if err == nil {
		t.Fatal("expected an error, got nil")
	}
	ee, ok := err.(*EngineError)
	if !ok {
		t.Fatalf("not an *EngineError: %v", err)
	}
	return ee.Code()
}

// The meter's Guard aborts with 57014 the instant the cancel poll returns true, independently of
// the cost ceilings (a zero-limit meter never aborts on cost).
func TestMeterGuardCancel(t *testing.T) {
	t.Parallel()
	m := newMeter()
	if err := m.Guard(); err != nil {
		t.Fatalf("no cancel set: Guard should pass, got %v", err)
	}

	flag := false
	m.cancel = func() bool { return flag }
	if err := m.Guard(); err != nil {
		t.Fatalf("cancel=false: Guard should pass, got %v", err)
	}
	flag = true
	if code := cancelCode(t, m.Guard()); code != "57014" {
		t.Fatalf("cancel=true: want 57014, got %s", code)
	}
}

// A context already canceled at the API entry aborts with 57014 before any work (the cheap
// boundary poll).
func TestCancelBeforeRun(t *testing.T) {
	t.Parallel()
	db := memDB()
	if _, err := db.Exec(context.Background(), "CREATE TABLE t (id i32 PRIMARY KEY)"); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if code := cancelCode(t, mustErr(db.Exec(ctx, "INSERT INTO t VALUES (1)"))); code != "57014" {
		t.Fatalf("Exec: want 57014, got %s", code)
	}
	_, qerr := db.Query(ctx, "SELECT id FROM t")
	if code := cancelCode(t, qerr); code != "57014" {
		t.Fatalf("Query: want 57014, got %s", code)
	}
}

// A cancel poll armed on the running engine aborts mid-execution through the meter's Guard — NOT
// the cursor boundary. Proven white-box: session.cancel is set directly (bypassing ctx), so only
// the in-statement Guard can produce the 57014.
func TestCancelDuringExecution(t *testing.T) {
	t.Parallel()
	db := memDB()
	if _, err := db.Exec(context.Background(), "CREATE TABLE t (id i32 PRIMARY KEY)"); err != nil {
		t.Fatal(err)
	}
	for i := 1; i <= 20; i++ {
		if _, err := db.Exec(context.Background(), "INSERT INTO t VALUES ($1)", i); err != nil {
			t.Fatal(err)
		}
	}

	s := db.Session(SessionOptions{})
	defer s.Close()
	// Always-cancel: the first Guard during the scan aborts. The ctx path is untouched here, so a
	// 57014 can only come from the meter consulting session.cancel. Since S4 (streaming.md §6) Query
	// returns a LAZY cursor — a bare scan buffers its input on the first pull — so building the cursor
	// no longer runs the scan; the meter Guard trips during the drain and the 57014 surfaces via Err().
	s.engine.session.cancel = func() bool { return true }
	rows, err := s.queryValues("SELECT id FROM t", nil)
	if err != nil {
		t.Fatalf("building the lazy cursor should not error, got %v", err)
	}
	for rows.Next() {
	}
	if code := cancelCode(t, rows.Err()); code != "57014" {
		t.Fatalf("want 57014 from meter Guard, got %s", code)
	}

	// Cleared: the same query completes normally.
	s.engine.session.cancel = nil
	rows, err = s.queryValues("SELECT id FROM t", nil)
	if err != nil {
		t.Fatalf("uncanceled query should succeed, got %v", err)
	}
	n := 0
	for rows.Next() {
		n++
	}
	if n != 20 {
		t.Fatalf("uncanceled query rows = %d, want 20", n)
	}
}

func mustErr(_ Result, err error) error { return err }

// A host callback is a deterministic barrier inside the executor, after Query and the
// Next entry check. Cancellation must be seen by a subsequent engine checkpoint in
// THAT Next, not by a later call. No sleeps and no dependence on scan speed.
func TestCursorCancellationInFlight(t *testing.T) {
	for _, handle := range []string{"database", "session", "transaction"} {
		for _, prepared := range []bool{false, true} {
			for _, shape := range []string{"stream", "aggregate", "deferred", "exec", "write"} {
				t.Run(fmt.Sprintf("%s/prepared=%v/%s", handle, prepared, shape), func(t *testing.T) {
					entered, release := make(chan struct{}), make(chan struct{})
					var once sync.Once
					gate := NewHostFunction("cancel_gate", []string{"i32"}, "i32", func(a []Value) (Value, error) {
						once.Do(func() { close(entered); <-release })
						return a[0], nil
					})
					db := memDBIn(envWith(t, gate))
					defer db.Close()
					bg := context.Background()
					for _, sql := range []string{"CREATE TABLE t (id i32 PRIMARY KEY)", "INSERT INTO t VALUES (1),(2),(3)", "CREATE TABLE writes (id i32 PRIMARY KEY)"} {
						if _, err := db.Exec(bg, sql); err != nil {
							t.Fatal(err)
						}
					}
					ctx, cancel := context.WithCancel(bg)
					defer cancel()
					sql := "SELECT id FROM t WHERE cancel_gate(id) > 0 ORDER BY id LIMIT 3"
					switch shape {
					case "aggregate":
						sql = "SELECT sum(cancel_gate(id)) FROM t"
					case "deferred":
						sql = "SELECT cancel_gate(id) FROM t UNION ALL SELECT 4"
					case "write":
						sql = "INSERT INTO writes SELECT cancel_gate(id) FROM t"
					}
					stmt, err := db.Prepare(sql)
					if err != nil {
						t.Fatal(err)
					}
					exercise := func(q Queryer) error {
						if handle == "transaction" {
							if _, err := q.Exec(bg, "INSERT INTO writes VALUES (99)"); err != nil {
								t.Fatal(err)
							}
						}
						done := make(chan error, 1)
						var rows *Rows
						if shape == "exec" || shape == "write" {
							go func() {
								var err error
								if prepared {
									_, err = q.ExecPrepared(ctx, stmt)
								} else {
									_, err = q.Exec(ctx, sql)
								}
								done <- err
							}()
						} else {
							if prepared {
								rows, err = q.QueryPrepared(ctx, stmt)
							} else {
								rows, err = q.Query(ctx, sql)
							}
							if err != nil {
								t.Fatal(err)
							}
							defer rows.Close()
							go func() {
								if rows.Next() {
									done <- fmt.Errorf("Next returned a row after cancellation")
								} else {
									done <- rows.Err()
								}
							}()
						}
						select {
						case <-entered:
						case <-time.After(5 * time.Second):
							t.Fatal("execution did not reach barrier")
						}
						cancel()
						close(release)
						var runErr error
						select {
						case runErr = <-done:
						case <-time.After(5 * time.Second):
							t.Fatal("cancellation did not return")
						}
						if code := cancelCode(t, runErr); code != "57014" {
							t.Fatalf("want 57014, got %s", code)
						}
						if rows != nil {
							if !rows.closed || rows.onClose != nil {
								t.Fatal("terminal error retained cursor pin")
							}
							if rows.Next() {
								t.Fatal("terminal cursor resumed")
							}
						}
						if handle == "transaction" {
							if code := cancelCode(t, mustErr(q.Exec(bg, "SELECT 1"))); code != "25P02" {
								t.Fatalf("transaction not failed: %s", code)
							}
							return runErr
						}
						if _, err := q.Exec(bg, "SELECT 1"); err != nil {
							t.Fatalf("next operation poisoned: %v", err)
						}
						return nil
					}
					switch handle {
					case "database":
						exercise(db)
					case "session":
						s := db.Session(SessionOptions{})
						defer s.Close()
						exercise(s)
					case "transaction":
						if code := cancelCode(t, db.Update(func(tx *Transaction) error { return exercise(tx) })); code != "57014" {
							t.Fatal(code)
						}
					}
					var n int64
					if err := db.QueryRow(bg, "SELECT count(*) FROM writes").Scan(&n); err != nil || n != 0 {
						t.Fatalf("write rollback: %d %v", n, err)
					}
				})
			}
		}
	}
}

func TestCursorCancellationLifetime(t *testing.T) {
	db := memDB()
	defer db.Close()
	s := db.Session(SessionOptions{})
	defer s.Close()
	bg := context.Background()
	ctx, cancel := context.WithCancel(bg)
	defer cancel()
	a, err := s.Query(ctx, "SELECT i FROM generate_series(1, 3) AS g(i)")
	if err != nil {
		t.Fatal(err)
	}
	b, err := s.Query(bg, "SELECT 42")
	if err != nil {
		t.Fatal(err)
	}
	cancel()
	if a.Next() || cancelCode(t, a.Err()) != "57014" {
		t.Fatal("canceled cursor continued")
	}
	if !b.Next() {
		t.Fatalf("second cursor contaminated: %v", b.Err())
	}
	if b.Next() || b.Err() != nil || !b.closed || b.onClose != nil {
		t.Fatal("exhaustion failed cleanup")
	}
	if b.Next() || b.Err() != nil {
		t.Fatal("exhaustion not terminal")
	}
	for _, sql := range []string{"SELECT missing FROM absent", "SELECT 1"} {
		ctx, cancel := context.WithCancel(bg)
		r, err := s.Query(ctx, sql)
		if err == nil {
			r.Close()
			cancel()
			if r.Next() || r.Err() != nil {
				t.Fatal("closed cursor observed cancellation")
			}
		} else {
			cancel()
		}
		if s.engine.session.cancel != nil {
			t.Fatal("session retained operation cancellation")
		}
		if _, err := s.Exec(bg, "SELECT 1"); err != nil {
			t.Fatal(err)
		}
	}
	ctx, cancel = context.WithCancel(bg)
	exhausted, err := s.Query(ctx, "SELECT 1")
	if err != nil {
		t.Fatal(err)
	}
	for exhausted.Next() {
	}
	cancel()
	if exhausted.Next() || exhausted.Err() != nil || !exhausted.closed {
		t.Fatal("late cancellation changed exhausted cursor")
	}

}

// Batch eligibility is independent of cancellation, but never of deterministic ceilings.
// The second poll must happen INSIDE the scan, before all rows are processed. Partial work must still accrue to the lifetime gauge.
func TestCancellationBatchCheckpoints(t *testing.T) {
	db := memDB()
	defer db.Close()
	bg := context.Background()
	for _, sql := range []string{"CREATE TABLE t (id i32 PRIMARY KEY, v i32)", "INSERT INTO t SELECT i, i % 7 FROM generate_series(1, 2000) AS g(i)"} {
		if _, err := db.Exec(bg, sql); err != nil {
			t.Fatal(err)
		}
	}
	for _, sql := range []string{"SELECT sum(v), count(*) FROM t", "SELECT v FROM t", "SELECT v, count(*) FROM t GROUP BY v"} {
		t.Run(sql, func(t *testing.T) {
			s := db.Session(SessionOptions{})
			defer s.Close()
			polls := 0
			s.engine.session.cancel = func() bool { polls++; return polls == 2 }
			r, err := s.queryValues(sql, nil)
			if err != nil {
				t.Fatal(err)
			}
			defer r.Close()
			if r.Next() || cancelCode(t, r.Err()) != "57014" {
				t.Fatal("batch did not cancel")
			}
			if polls != 2 || r.Cost() <= 0 || r.Cost() >= 2000 {
				t.Fatalf("scan ran to completion before cancel: polls=%d cost=%d", polls, r.Cost())
			}
			if *s.engine.session.lifetimeTotal != r.Cost() {
				t.Fatal("canceled work lost from lifetime total")
			}
			s.engine.session.cancel = nil
		})
	}
}

func TestCancellationPreservesCostLimits(t *testing.T) {
	db := memDB()
	defer db.Close()
	bg := context.Background()
	for _, sql := range []string{"CREATE TABLE t (id i32 PRIMARY KEY, v i32)", "INSERT INTO t SELECT i, i % 7 FROM generate_series(1, 300) AS g(i)"} {
		if _, err := db.Exec(bg, sql); err != nil {
			t.Fatal(err)
		}
	}
	ctx, cancel := context.WithCancel(bg)
	defer cancel()
	for _, sql := range []string{"SELECT sum(v), count(*) FROM t", "SELECT v FROM t", "SELECT v, count(*) FROM t GROUP BY v"} {
		for _, setting := range []string{"", "SET max_cost = 50", "SET lifetime_max_cost = 50"} {
			var wantCost int64
			var wantRows int
			var wantErr string
			for i, c := range []context.Context{bg, ctx} {
				s := db.Session(SessionOptions{})
				if setting != "" {
					if setting == "SET max_cost = 50" {
						s.SetMaxCost(50)
					} else {
						s.SetLifetimeMaxCost(50)
					}
				}
				r, err := s.Query(c, sql)
				if err != nil {
					t.Fatal(err)
				}
				n := 0
				for r.Next() {
					n++
				}
				code := ""
				if r.Err() != nil {
					code = cancelCode(t, r.Err())
				}
				if i == 0 {
					wantCost, wantRows, wantErr = r.Cost(), n, code
				} else if wantCost != r.Cost() || wantRows != n || wantErr != code {
					t.Fatalf("%s %s cost/rows/error differ", sql, setting)
				}
				r.Close()
				s.Close()
			}
		}
	}
}
