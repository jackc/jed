package jed

// Data-modifying (writable) CTEs (spec/design/writable-cte.md) — the per-core slice that the
// PostgreSQL-clean conformance corpus (cte/data_modifying.test, cte/with_dml.test,
// cte/data_modifying_errors.test) cannot express: the command tag of a data-modifying primary (the
// outcomeStatement affected-row count, which the corpus's `statement ok` does not assert), and jed's
// deterministic last-write-wins resolution of an update/update or update/delete of the SAME row — a
// documented divergence on a case PostgreSQL leaves unspecified (§7). Mirrors
// impl/rust/tests/writable_cte.rs and impl/ts/tests/writable_cte.test.ts.

import (
	"sort"
	"testing"
)

// wcRun executes sql and returns its outcome, failing the test on error.
func wcRun(t *testing.T, db dbHandle, sql string) outcome {
	t.Helper()
	out, err := queryOutcome(db, sql, nil)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	return out
}

// wcRows executes sql and returns its result rows, failing if it is not a query result.
func wcRows(t *testing.T, db dbHandle, sql string) []storedRow {
	t.Helper()
	out := wcRun(t, db, sql)
	if out.Kind != outcomeQuery {
		t.Fatalf("expected a query result for %q", sql)
	}
	rows := make([]storedRow, len(out.Rows))
	for i, r := range out.Rows {
		rows[i] = storedRow(r)
	}
	return rows
}

// wcAffected executes sql and returns its statement-shaped affected-row count, failing if it is not
// a statement result.
func wcAffected(t *testing.T, db dbHandle, sql string) int64 {
	t.Helper()
	out := wcRun(t, db, sql)
	if out.Kind != outcomeStatement || !out.HasRowsAffected {
		t.Fatalf("expected a statement result for %q", sql)
	}
	return out.RowsAffected
}

// wcInts collects the first column of each row as a sorted []int64.
func wcInts(rows []storedRow) []int64 {
	v := make([]int64, len(rows))
	for i, r := range rows {
		v[i] = r[0].Int
	}
	sort.Slice(v, func(a, b int) bool { return v[a] < v[b] })
	return v
}

func wcSetup(t *testing.T) *Session {
	t.Helper()
	return dbWith(
		t,
		"CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
		"INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
	)
}

// --- the command tag of a data-modifying primary (the result is the PRIMARY's, §4) ------------

func TestWithOnInsertPrimaryNoReturningReportsAffectedCount(t *testing.T) {
	t.Parallel()
	db := wcSetup(t)
	wcRun(t, db, "CREATE TABLE dst (x i32)")
	// A WITH feeding an INSERT primary with no RETURNING is a STATEMENT whose count is the primary's
	// inserted-row count (a CTE's own count is never surfaced — §4).
	if n := wcAffected(t, db,
		"WITH src AS (SELECT id FROM t WHERE id <= 2) INSERT INTO dst SELECT id FROM src"); n != 2 {
		t.Errorf("affected got %d want 2", n)
	}
	if got := wcInts(wcRows(t, db, "SELECT x FROM dst")); !eqInts(got, 1, 2) {
		t.Errorf("dst got %v want [1 2]", got)
	}
}

func TestWithOnDeletePrimaryNoReturningReportsAffectedCount(t *testing.T) {
	t.Parallel()
	db := wcSetup(t)
	if n := wcAffected(t, db,
		"WITH old AS (SELECT id FROM t WHERE id >= 2) DELETE FROM t WHERE id IN (SELECT id FROM old)"); n != 2 {
		t.Errorf("affected got %d want 2", n)
	}
	if got := wcInts(wcRows(t, db, "SELECT id FROM t")); !eqInts(got, 1) {
		t.Errorf("t got %v want [1]", got)
	}
}

func TestWithOnUpdatePrimaryNoReturningReportsAffectedCount(t *testing.T) {
	t.Parallel()
	db := wcSetup(t)
	if n := wcAffected(t, db,
		"WITH hi AS (SELECT id FROM t WHERE v >= 20) UPDATE t SET v = v + 1 WHERE id IN (SELECT id FROM hi)"); n != 2 {
		t.Errorf("affected got %d want 2", n)
	}
}

func TestDataModifyingCteCountNotSurfacedUnderSelectPrimary(t *testing.T) {
	t.Parallel()
	db := wcSetup(t)
	// The data-modifying CTE inserts 1 row, but the SELECT primary's result is what is returned — and
	// it reads the PRE-statement table (the pin, §2), so count is 3, not 4.
	rows := wcRows(t, db,
		"WITH ins AS (INSERT INTO t VALUES (4, 40) RETURNING *) SELECT count(*) FROM t")
	if len(rows) != 1 || rows[0][0].Int != 3 {
		t.Fatalf("count under pin got %v want [[3]]", rows)
	}
	// ...and the insert still landed (always to completion, §3).
	rows = wcRows(t, db, "SELECT count(*) FROM t")
	if len(rows) != 1 || rows[0][0].Int != 4 {
		t.Fatalf("post-statement count got %v want [[4]]", rows)
	}
}

// --- jed's deterministic last-write-wins on a same-row conflict (PG-unspecified, §7) ----------

func TestSameRowTwoUpdatesLastWriteWins(t *testing.T) {
	t.Parallel()
	db := wcSetup(t)
	// Two CTEs update id=1. Each reads the PIN (pre-statement v=10) and returns its own new value, so
	// BOTH return a row; the writes apply in lexical order, last-write-wins, so the table ends at the
	// SECOND CTE's value. PostgreSQL applies and returns only ONE (unspecified which) — the documented
	// divergence.
	got := wcInts(wcRows(t, db,
		"WITH a AS (UPDATE t SET v = 100 WHERE id = 1 RETURNING v), "+
			"b AS (UPDATE t SET v = 200 WHERE id = 1 RETURNING v) "+
			"SELECT v FROM a UNION ALL SELECT v FROM b"))
	if !eqInts(got, 100, 200) {
		t.Errorf("both updates compute RETURNING from the pin: got %v want [100 200]", got)
	}
	// The committed value is the second (lexically later) write.
	rows := wcRows(t, db, "SELECT v FROM t WHERE id = 1")
	if len(rows) != 1 || rows[0][0].Int != 200 {
		t.Fatalf("committed v got %v want [[200]]", rows)
	}
}

func TestSameRowUpdateThenDeleteDeleteWins(t *testing.T) {
	t.Parallel()
	db := wcSetup(t)
	// CTE a updates id=1 to 100; CTE b deletes id=1. Both read the pin (the pre-statement row), so a
	// returns 100 and b returns the pre-statement old value 10; b's delete applies after a's update,
	// so the row is gone at the end (delete wins).
	upd := wcInts(wcRows(t, db,
		"WITH a AS (UPDATE t SET v = 100 WHERE id = 1 RETURNING v) SELECT v FROM a"))
	if !eqInts(upd, 100) {
		t.Errorf("update returning got %v want [100]", upd)
	}
	// Reset and run the combined conflict.
	db = wcSetup(t)
	got := wcInts(wcRows(t, db,
		"WITH a AS (UPDATE t SET v = 100 WHERE id = 1 RETURNING v), "+
			"b AS (DELETE FROM t WHERE id = 1 RETURNING v) "+
			"SELECT v FROM a UNION ALL SELECT v FROM b"))
	if !eqInts(got, 10, 100) {
		t.Errorf("a returns the new value, b the pre-statement old value: got %v want [10 100]", got)
	}
	// id=1 is gone (the delete applied last).
	if ids := wcInts(wcRows(t, db, "SELECT id FROM t")); !eqInts(ids, 2, 3) {
		t.Errorf("surviving ids got %v want [2 3]", ids)
	}
}
