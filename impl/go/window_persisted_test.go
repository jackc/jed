package jed

// A windowed aggregate whose ARGUMENT column is referenced ONLY inside the OVER call must still read
// that column from a persisted (lazily-faulted) leaf. The touched-set collector has to descend into
// each window function's args / FILTER (spec/design/window.md §5.2; large-values.md §14) — otherwise
// the lazy/masked scan leaves the operand column unfetched and the aggregate folds NULL. This is the
// on-disk read-path regression the in-memory conformance corpus cannot express (CLAUDE.md §10): it
// only surfaced through the window_running_sum benchmark, which reads a committed file. Mirrors
// impl/rust/tests/window_persisted.rs and impl/ts/tests/window_persisted.test.ts.

import (
	"fmt"
	"path/filepath"
	"testing"
)

// seedPersistedWindow creates a small-page file table whose `amount` column is not referenced outside
// the window functions under test, then commits + reopens so its rows fault in lazily. 40 rows over
// 256-byte pages span several leaves, so the masked scan is genuinely exercised.
func seedPersistedWindow(t *testing.T) *engine {
	t.Helper()
	path := filepath.Join(t.TempDir(), "window_persisted.jed")
	db, err := create(path, databaseOptions{PageSize: lazyPageSize})
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, grp i32, amount i32)")
	for i := 1; i <= 40; i++ {
		mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, %d, %d)", i, i%3, i*10))
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	db, err = open(path)
	if err != nil {
		t.Fatal(err)
	}
	return db
}

func TestWindowRunningAggregateOverPersistedColumn(t *testing.T) {
	db := seedPersistedWindow(t)
	defer db.Close()

	// Running SUM over the default frame — amount enters the touched set ONLY through the window arg.
	rows := queryRows(t, db, "SELECT id, sum(amount) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id")
	if len(rows) != 40 {
		t.Fatalf("got %d rows, want 40", len(rows))
	}
	var want int64
	for i, r := range rows {
		want += int64((i + 1) * 10)
		if r[1].Kind != ValInt || r[1].Int != want {
			t.Fatalf("row %d running sum = %s (kind %d), want %d — operand column read as NULL?", i+1, r[1].Render(), r[1].Kind, want)
		}
	}
}

// The windowed TOP-N optimization (spec/design/window.md §5.2) reads only the first OFFSET+LIMIT
// scan rows, then folds the window over that prefix — over a persisted file it must still resolve the
// operand column from the faulted leaves it touches (the touched-set mask). This is the on-disk
// read-path check the in-memory corpus (window/topn.test) cannot express — the window_running_sum
// benchmark's regression twin.
func TestWindowTopNOverPersistedColumn(t *testing.T) {
	db := seedPersistedWindow(t)
	defer db.Close()

	rows := queryRows(t, db, "SELECT id, sum(amount) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id LIMIT 5")
	if len(rows) != 5 {
		t.Fatalf("got %d rows, want 5", len(rows))
	}
	var want int64
	for i, r := range rows {
		want += int64((i + 1) * 10)
		if r[1].Kind != ValInt || r[1].Int != want {
			t.Fatalf("row %d running sum = %s (kind %d), want %d — operand column read as NULL from disk?", i+1, r[1].Render(), r[1].Kind, want)
		}
	}
}

func TestWindowAggregateFilterAndOffsetOverPersistedColumn(t *testing.T) {
	db := seedPersistedWindow(t)
	defer db.Close()

	// A bounded moving MAX (frame path) plus a partitioned running COUNT of a column argument (which
	// COUNT skips only on NULL) and an offset function whose value is a persisted column.
	rows := queryRows(t, db, "SELECT id, max(amount) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), lag(amount) OVER (ORDER BY id) FROM t ORDER BY id LIMIT 3")
	// max over {row1}=10, {row1,row2}=20, {row2,row3}=30 ; lag: NULL, 10, 20.
	wantMax := []int64{10, 20, 30}
	wantLag := []struct {
		null bool
		v    int64
	}{{true, 0}, {false, 10}, {false, 20}}
	for i, r := range rows {
		if r[1].Kind != ValInt || r[1].Int != wantMax[i] {
			t.Fatalf("row %d moving max = %s, want %d", i+1, r[1].Render(), wantMax[i])
		}
		if wantLag[i].null {
			if r[2].Kind != ValNull {
				t.Fatalf("row %d lag = %s, want NULL", i+1, r[2].Render())
			}
		} else if r[2].Kind != ValInt || r[2].Int != wantLag[i].v {
			t.Fatalf("row %d lag = %s, want %d", i+1, r[2].Render(), wantLag[i].v)
		}
	}

	// FILTER routes its predicate column through spec.filter; a running SUM of amount for even ids.
	f := queryRows(t, db, "SELECT id, sum(amount) FILTER (WHERE amount % 20 = 0) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id LIMIT 4")
	// amounts 10,20,30,40 ; only 20 and 40 pass amount%20=0 ; running: NULL,20,20,60.
	wantF := []struct {
		null bool
		v    int64
	}{{true, 0}, {false, 20}, {false, 20}, {false, 60}}
	for i, r := range f {
		if wantF[i].null {
			if r[1].Kind != ValNull {
				t.Fatalf("filter row %d = %s, want NULL", i+1, r[1].Render())
			}
		} else if r[1].Kind != ValInt || r[1].Int != wantF[i].v {
			t.Fatalf("filter row %d = %s, want %d", i+1, r[1].Render(), wantF[i].v)
		}
	}
}
