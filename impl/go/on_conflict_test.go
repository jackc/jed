// INSERT ... ON CONFLICT (UPSERT) — the pieces the oracle corpus
// (spec/conformance/suites/dml/insert_on_conflict.test) cannot express: the jed-specific
// divergences from PostgreSQL (spec/design/upsert.md §9) and the affected-row count (the
// command-tag count a `statement ok` does not assert). The PG-agreeing behavior — DO NOTHING /
// DO UPDATE, arbiter inference / ON CONSTRAINT, the 21000 second-affect rule, non-arbiter 23505 —
// is the corpus's job. Mirrors impl/rust/tests/on_conflict.rs and impl/ts/tests/on_conflict.test.ts.
package jed

import "testing"

func ocDB(t *testing.T, sql ...string) *Database {
	t.Helper()
	db := NewDatabase()
	for _, s := range sql {
		if _, err := Execute(db, s); err != nil {
			t.Fatalf("setup %q: %v", s, err)
		}
	}
	return db
}

func ocErr(t *testing.T, db *Database, sql string) string {
	t.Helper()
	_, err := Execute(db, sql)
	if err == nil {
		t.Fatalf("expected an error from %q", sql)
	}
	return err.(*EngineError).Code()
}

func ocAffected(t *testing.T, db *Database, sql string) (int64, bool) {
	t.Helper()
	out, err := Execute(db, sql)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	if out.Kind != OutcomeStatement {
		t.Fatalf("expected a statement outcome from %q", sql)
	}
	return out.RowsAffected, out.HasRowsAffected
}

// DIVERGENCE (upsert.md §9): assigning a PRIMARY KEY column in DO UPDATE is 0A000 — the standing
// UPDATE narrowing (the storage key never changes). PostgreSQL allows it.
func TestOnConflictDoUpdatePKColumnUnsupported(t *testing.T) {
	db := ocDB(t, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)", "INSERT INTO t VALUES (1, 10)")
	if got := ocErr(t, db, "INSERT INTO t VALUES (1, 5) ON CONFLICT (id) DO UPDATE SET id = excluded.id + 100"); got != "0A000" {
		t.Fatalf("got %s, want 0A000", got)
	}
}

// DIVERGENCE: `DO UPDATE SET col = DEFAULT` is not supported — the RHS is a general expression, and
// DEFAULT is not reserved (§3), so a bare DEFAULT resolves as a column reference → 42703. PostgreSQL
// supports SET col = DEFAULT.
func TestOnConflictDoUpdateSetDefaultUnsupported(t *testing.T) {
	db := ocDB(t, "CREATE TABLE t (id i32 PRIMARY KEY, v i32 DEFAULT 7)", "INSERT INTO t VALUES (1, 10)")
	if got := ocErr(t, db, "INSERT INTO t VALUES (1, 5) ON CONFLICT (id) DO UPDATE SET v = DEFAULT"); got != "42703" {
		t.Fatalf("got %s, want 42703", got)
	}
}

// DIVERGENCE: a GENERATED ALWAYS identity column can only be set to DEFAULT (jed has no
// SET = DEFAULT), so any DO UPDATE assignment to one is 428C9 — the standing UPDATE rule.
func TestOnConflictDoUpdateGeneratedAlwaysRejected(t *testing.T) {
	db := ocDB(t,
		"CREATE TABLE t (id i32 GENERATED ALWAYS AS IDENTITY, k i32 PRIMARY KEY, v i32)",
		"INSERT INTO t (k, v) VALUES (1, 10)")
	if got := ocErr(t, db, "INSERT INTO t (k, v) VALUES (1, 5) ON CONFLICT (k) DO UPDATE SET id = 99"); got != "428C9" {
		t.Fatalf("got %s, want 428C9", got)
	}
}

// The affected-row count (api.md §4) the corpus's `statement ok` cannot assert: an ON CONFLICT
// counts the inserted + updated rows; rows skipped by DO NOTHING (or a DO UPDATE WHERE that is
// false) are not counted.
func TestOnConflictAffectedRowCounts(t *testing.T) {
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
