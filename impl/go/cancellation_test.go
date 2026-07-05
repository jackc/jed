package jed

// Cancellation through the cost meter (spec/design/api.md §11.4). Per-core unit tests, NOT the
// shared corpus: cancellation is timing-dependent (CLAUDE.md §10), so it cannot live there. These
// pin the mechanism deterministically — the meter's Guard() honors the cancel poll, and a flipped
// poll aborts a running statement with 57014 (not only at the cursor boundary).

import (
	"context"
	"testing"
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
