// INSERT ... ON CONFLICT (UPSERT) — the pieces the oracle corpus
// (spec/conformance/suites/dml/insert_on_conflict.test) cannot express: the jed-specific
// divergences from PostgreSQL (spec/design/upsert.md §9) and the affected-row count (the
// command-tag count a `statement ok` does not assert). The PG-agreeing behavior — DO NOTHING /
// DO UPDATE, arbiter inference / ON CONSTRAINT, the 21000 second-affect rule, non-arbiter 23505 —
// is the corpus's job. Mirrors impl/rust/tests/on_conflict.rs and impl/ts/tests/on_conflict.test.ts.
package jed

import "testing"

func ocDB(t *testing.T, sql ...string) *Session {
	t.Helper()
	db := memDB().Session(SessionOptions{})
	for _, s := range sql {
		if _, err := queryOutcome(db, s, nil); err != nil {
			t.Fatalf("setup %q: %v", s, err)
		}
	}
	return db
}

func ocAffected(t *testing.T, db dbHandle, sql string) (int64, bool) {
	t.Helper()
	out, err := queryOutcome(db, sql, nil)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	if out.Kind != outcomeStatement {
		t.Fatalf("expected a statement outcome from %q", sql)
	}
	return out.RowsAffected, out.HasRowsAffected
}

// The affected-row count (api.md §4) the corpus's `statement ok` cannot assert: an ON CONFLICT
// counts the inserted + updated rows; rows skipped by DO NOTHING (or a DO UPDATE WHERE that is
// false) are not counted.
func TestOnConflictAffectedRowCounts(t *testing.T) {
	t.Parallel()
	db := ocDB(t, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)", "INSERT INTO t VALUES (1, 10), (2, 20)")
	check := func(sql string, want int64) {
		t.Helper()
		got, has := ocAffected(t, db, sql)
		if !has || got != want {
			t.Fatalf("%q: got (%d, %v), want %d", sql, got, has, want)
		}
	}
	// DO NOTHING over a batch: id 1 conflicts (skip), id 3 inserts → 1 affected.
	check("INSERT INTO t VALUES (1, 99), (3, 30) ON CONFLICT DO NOTHING", 1)
	// DO UPDATE: id 2 updates, id 4 inserts → 2 affected.
	check("INSERT INTO t VALUES (2, 22), (4, 40) ON CONFLICT (id) DO UPDATE SET v = excluded.v", 2)
	// All conflict and are skipped → 0 affected.
	check("INSERT INTO t VALUES (1, 0), (2, 0) ON CONFLICT DO NOTHING", 0)
	// A DO UPDATE WHERE that is false updates nothing → 0 affected.
	check("INSERT INTO t VALUES (1, 7) ON CONFLICT (id) DO UPDATE SET v = excluded.v WHERE false", 0)
}
