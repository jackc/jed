package jed

// The RETURNING clause (spec/design/grammar.md §32, cost.md §3) — covers what the corpus
// suite (dml/returning.test) cannot: the Outcome variant split (Statement vs Query),
// output column names, pinned costs (the projection charge, the touched-set growth, the
// fold-once/correlated split), the ceiling's all-or-nothing abort, $N binding, and
// transactional behavior. Mirrored in impl/rust/tests/returning.rs and
// impl/ts/tests/returning.test.ts.

import (
	"fmt"
	"reflect"
	"strings"
	"testing"
)

func retRun(t *testing.T, db *Database, sql string) Outcome {
	t.Helper()
	o, err := Execute(db, sql)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	return o
}

func retCost(t *testing.T, db *Database, sql string) int64 {
	t.Helper()
	return retRun(t, db, sql).Cost
}

func retRows(t *testing.T, db *Database, sql string) [][]Value {
	t.Helper()
	o := retRun(t, db, sql)
	if o.Kind != OutcomeQuery {
		t.Fatalf("expected a query result for %q", sql)
	}
	return o.Rows
}

func retErrCode(t *testing.T, db *Database, sql string) string {
	t.Helper()
	_, err := Execute(db, sql)
	if err == nil {
		t.Fatalf("expected an error from %q", sql)
	}
	return err.(*EngineError).Code()
}

// retGrid renders rows as "a,b|c,d" for compact comparison.
func retGrid(rows [][]Value) string {
	parts := make([]string, 0, len(rows))
	for _, r := range rows {
		cells := make([]string, 0, len(r))
		for _, v := range r {
			if v.Kind == ValNull {
				cells = append(cells, "NULL")
			} else {
				cells = append(cells, fmt.Sprintf("%d", v.Int))
			}
		}
		parts = append(parts, strings.Join(cells, ","))
	}
	return strings.Join(parts, "|")
}

func retSetup(t *testing.T) *Database {
	t.Helper()
	db := NewDatabase()
	for _, s := range []string{
		"CREATE TABLE t (id int32 PRIMARY KEY, v int32 DEFAULT 7, w int32)",
		"INSERT INTO t VALUES (1, 10, 100), (2, 20, 200), (3, 30, 300)",
	} {
		if _, err := Execute(db, s); err != nil {
			t.Fatalf("setup %q: %v", s, err)
		}
	}
	return db
}

func TestInsertValuesReturningRowsAndVariant(t *testing.T) {
	db := retSetup(t)
	// Without RETURNING an INSERT stays a bare statement outcome.
	if o := retRun(t, db, "INSERT INTO t VALUES (10, 1, 2)"); o.Kind != OutcomeStatement {
		t.Fatalf("plain INSERT must be a statement outcome")
	}
	// With it, the stored rows project back — including multi-row and the `*` glob with
	// the DEFAULT fill-in (v = 7) and the omitted column (w = NULL).
	if g := retGrid(retRows(t, db, "INSERT INTO t VALUES (11, 5, 6) RETURNING id, v")); g != "11,5" {
		t.Fatalf("got %s", g)
	}
	if g := retGrid(retRows(t, db, "INSERT INTO t (id) VALUES (12), (13) RETURNING *")); g != "12,7,NULL|13,7,NULL" {
		t.Fatalf("got %s", g)
	}
}

func TestReturningOutputNamesAndExpressions(t *testing.T) {
	db := retSetup(t)
	// §8 naming: ?column? for an expression, the AS label, the canonical name for a
	// bare/qualified column. Expressions evaluate against the stored row.
	o := retRun(t, db, "INSERT INTO t VALUES (14, 5, 0) RETURNING v + 1, v * 2 AS dbl, t.w, id")
	if want := []string{"?column?", "dbl", "w", "id"}; !reflect.DeepEqual(o.ColumnNames, want) {
		t.Fatalf("names: got %v want %v", o.ColumnNames, want)
	}
	if g := retGrid(retRows(t, db, "DELETE FROM t WHERE id = 14 RETURNING v * 10, abs(w - 1)")); g != "50,1" {
		t.Fatalf("got %s", g)
	}
}

func TestInsertSelectReturning(t *testing.T) {
	db := retSetup(t)
	retRun(t, db, "CREATE TABLE src (a int32)")
	retRun(t, db, "INSERT INTO src VALUES (40), (41)")
	// RETURNING belongs to the INSERT: it projects the INSERTED rows (defaults filled).
	if g := retGrid(retRows(t, db, "INSERT INTO t (id) SELECT a FROM src RETURNING id, v")); g != "40,7|41,7" {
		t.Fatalf("got %s", g)
	}
	// The word `returning` is never an IMPLICIT source alias (the §15 stop set) — but an
	// explicit `AS returning` alias still parses, and the clause follows it.
	if g := retGrid(retRows(t, db, "INSERT INTO t (id) SELECT a + 100 FROM src AS returning RETURNING id")); g != "140|141" {
		t.Fatalf("got %s", g)
	}
}

func TestUpdateReturningNewValues(t *testing.T) {
	db := retSetup(t)
	if g := retGrid(retRows(t, db, "UPDATE t SET v = v + 1 WHERE id <= 2 RETURNING id, v")); g != "1,11|2,21" {
		t.Fatalf("got %s", g)
	}
	// Zero matched rows: still a QUERY outcome — empty rows, names intact.
	o := retRun(t, db, "UPDATE t SET v = 0 WHERE id = 999 RETURNING id")
	if o.Kind != OutcomeQuery || len(o.Rows) != 0 || !reflect.DeepEqual(o.ColumnNames, []string{"id"}) {
		t.Fatalf("zero-row RETURNING must still be a query result: %+v", o)
	}
}

func TestDeleteReturningOldValues(t *testing.T) {
	db := retSetup(t)
	if g := retGrid(retRows(t, db, "DELETE FROM t WHERE w = 200 RETURNING id, v, w")); g != "2,20,200" {
		t.Fatalf("got %s", g)
	}
	if g := retGrid(retRows(t, db, "SELECT id FROM t ORDER BY id")); g != "1|3" {
		t.Fatalf("got %s", g)
	}
}

func TestReturningErrorCodes(t *testing.T) {
	db := retSetup(t)
	cases := []struct{ sql, code string }{
		// Resolution precedes execution: the unknown column beats the would-be PK duplicate.
		{"INSERT INTO t VALUES (1, 0, 0) RETURNING nosuch", "42703"},
		// Aggregates are forbidden in RETURNING (PG 42803).
		{"INSERT INTO t VALUES (90, 0, 0) RETURNING sum(v)", "42803"},
		{"UPDATE t SET v = 1 RETURNING count(*)", "42803"},
		// An unknown qualifier is 42P01.
		{"INSERT INTO t VALUES (91, 0, 0) RETURNING other.v", "42P01"},
		// old/new are RETURNING-only (grammar.md §32): elsewhere they are ordinary unknown
		// qualifiers (42P01, as in PG); an unknown column under them is 42703.
		{"UPDATE t SET v = old.v + 1 WHERE id = 1", "42P01"},
		{"DELETE FROM t WHERE new.v = 1", "42P01"},
		{"UPDATE t SET v = 1 RETURNING old.nosuch", "42703"},
		// An empty item list, and any trailing clause after RETURNING, are 42601.
		{"DELETE FROM t RETURNING", "42601"},
		{"DELETE FROM t WHERE id = 1 RETURNING id ORDER BY id", "42601"},
		// `returning` is no longer an implicit alias ANYWHERE (the §15 stop set): in a
		// plain SELECT it is now trailing junk, as in PostgreSQL (which reserves the word).
		{"SELECT v FROM t returning", "42601"},
	}
	for _, c := range cases {
		if got := retErrCode(t, db, c.sql); got != c.code {
			t.Errorf("%q: got %s want %s", c.sql, got, c.code)
		}
	}
	// Nothing above wrote anything.
	if g := retGrid(retRows(t, db, "SELECT count(*) FROM t")); g != "3" {
		t.Fatalf("got %s", g)
	}
}

func TestReturningSubqueriesPreStatementSnapshot(t *testing.T) {
	db := retSetup(t)
	// Uncorrelated subqueries fold once and read the PRE-statement snapshot (probed
	// against PG 18): the count excludes the two rows being inserted...
	if g := retGrid(retRows(t, db,
		"INSERT INTO t VALUES (50, 0, 0), (51, 0, 0) RETURNING id, (SELECT count(*) FROM t)")); g != "50,3|51,3" {
		t.Fatalf("got %s", g)
	}
	// ... an UPDATE's subquery sees pre-update values (sum over old v: 10+20+30) ...
	if g := retGrid(retRows(t, db,
		"UPDATE t SET v = 0 WHERE id = 1 RETURNING (SELECT sum(v) FROM t WHERE w IS NOT NULL)")); g != "60" {
		t.Fatalf("got %s", g)
	}
	// ... and a DELETE's sees the row still present (5 rows live at this point).
	if g := retGrid(retRows(t, db,
		"DELETE FROM t WHERE id = 1 RETURNING (SELECT count(*) FROM t WHERE w IS NOT NULL)")); g != "5" {
		t.Fatalf("got %s", g)
	}
	// A correlated subquery's outer reference reads the row being RETURNED (here the
	// deleted row: its neighbor id+1 = 3 has w = 300).
	if g := retGrid(retRows(t, db,
		"DELETE FROM t WHERE id = 2 RETURNING (SELECT s.w FROM t AS s WHERE s.id = t.id + 1)")); g != "300" {
		t.Fatalf("got %s", g)
	}
}

func TestReturningCosts(t *testing.T) {
	db := retSetup(t)
	// A plain VALUES insert still costs zero; RETURNING adds row_produced per stored row
	// plus the items' metered evaluation (bare columns are leaves).
	costs := []struct {
		sql  string
		want int64
	}{
		{"INSERT INTO t VALUES (60, 1, 1)", 0},
		{"INSERT INTO t VALUES (61, 1, 1) RETURNING id, v", 1},
		// 2 x (row_produced + one operator_eval)
		{"INSERT INTO t VALUES (62, 1, 1), (63, 2, 2) RETURNING v + 1", 4},
		// UPDATE/DELETE under a PK point bound: page_read(1) + storage_row_read(1) + the
		// residual filter eval(1), plus the projection (row_produced 1, leaves 0).
		{"UPDATE t SET v = 9 WHERE id = 1", 3},
		{"UPDATE t SET v = 8 WHERE id = 1 RETURNING v", 4},
		{"DELETE FROM t WHERE id = 60 RETURNING v", 4},
	}
	for _, c := range costs {
		if got := retCost(t, db, c.sql); got != c.want {
			t.Errorf("%q: cost %d want %d", c.sql, got, c.want)
		}
	}
}

func TestReturningSubqueryCosts(t *testing.T) {
	// Fresh 3-row table: an uncorrelated RETURNING subquery folds ONCE.
	// Inner `SELECT max(v) FROM t`: page_read 1 + 3 row reads + 3 accumulates +
	// 1 row_produced = 8. Two returned rows add 2 x row_produced (the folded constant is
	// a leaf): total 10.
	db := retSetup(t)
	if got := retCost(t, db,
		"INSERT INTO t VALUES (64, 1, 1), (65, 1, 2) RETURNING (SELECT max(v) FROM t)"); got != 10 {
		t.Fatalf("uncorrelated: cost %d want 10", got)
	}
	// A correlated one re-runs per RETURNED row: outer DELETE bound = page 1 + row 1 +
	// filter 1 + row_produced 1; the subquery node charges operator_eval 1 + the inner
	// bounded count (page 1 + row 1 + filter 1 + accumulate 1 + row_produced 1 = 5).
	db = retSetup(t)
	if got := retCost(t, db,
		"DELETE FROM t WHERE id = 1 RETURNING (SELECT count(*) FROM t AS s WHERE s.id = t.id)"); got != 10 {
		t.Fatalf("correlated: cost %d want 10", got)
	}
}

func TestReturningCeilingAbortIsAllOrNothing(t *testing.T) {
	db := retSetup(t)
	// The two-row insert with RETURNING costs 4 (pinned above). A ceiling of 2 aborts
	// during the projection pass — BEFORE phase 2 — so nothing is inserted.
	db.SetMaxCost(2)
	if got := retErrCode(t, db, "INSERT INTO t VALUES (70, 1, 1), (71, 2, 2) RETURNING v + 1"); got != "54P01" {
		t.Fatalf("got %s want 54P01", got)
	}
	db.SetMaxCost(0)
	if g := retGrid(retRows(t, db, "SELECT count(*) FROM t")); g != "3" {
		t.Fatalf("ceiling abort must write nothing; got %s rows", g)
	}
}

func TestReturningBindParams(t *testing.T) {
	db := retSetup(t)
	// A $N in the RETURNING list types from context like anywhere else (api.md §5).
	o, err := ExecuteParams(db, "INSERT INTO t VALUES (80, 3, 0) RETURNING v + $1", []Value{IntValue(5)})
	if err != nil {
		t.Fatalf("bind: %v", err)
	}
	if o.Kind != OutcomeQuery || retGrid(o.Rows) != "8" {
		t.Fatalf("got %+v", o)
	}
	// A parameter no context types is 42P18.
	if _, err := ExecuteParams(db, "INSERT INTO t VALUES (81, 3, 0) RETURNING $1", []Value{IntValue(5)}); err == nil {
		t.Fatalf("an untypable parameter must fail")
	} else if code := err.(*EngineError).Code(); code != "42P18" {
		t.Fatalf("got %s want 42P18", code)
	}
}

func TestReturningGrowsTheTouchedSet(t *testing.T) {
	// A compressed large value charges value_decompress only when RETURNING reads it
	// (the §32 touched-set rule). 100_000 raw bytes at page_size 8192 (C = 8180):
	// ceil(100000/8180) = 13 slabs.
	big := "INSERT INTO big VALUES (1, 0, '" + strings.Repeat("x", 100_000) + "')"
	fresh := func() *Database {
		db := NewDatabase()
		retRun(t, db, "CREATE TABLE big (id int32 PRIMARY KEY, w int32, t text)")
		retRun(t, db, big)
		return db
	}
	// RETURNING only fixed-width columns: no decompression (page 1 + row 1 + filter 1 +
	// row_produced 1).
	if got := retCost(t, fresh(), "DELETE FROM big WHERE id = 1 RETURNING id, w"); got != 4 {
		t.Fatalf("fixed-width: cost %d want 4", got)
	}
	// RETURNING the compressed column adds its 13 slabs.
	if got := retCost(t, fresh(), "DELETE FROM big WHERE id = 1 RETURNING t"); got != 17 {
		t.Fatalf("compressed: cost %d want 17", got)
	}
	// UPDATE: an ASSIGNED column's returned value is the freshly computed one — not a
	// storage read, so no decompression (and the shrunken row re-stores inline-plain:
	// no compression attempt either).
	if got := retCost(t, fresh(), "UPDATE big SET t = 'short' WHERE id = 1 RETURNING t"); got != 4 {
		t.Fatalf("assigned: cost %d want 4", got)
	}
	// RETURNING an UNASSIGNED compressed column is a logical read: the rewrite's own
	// 13 value_compress attempts (the over-RECORD_MAX row re-stores) + the projection's
	// 13 value_decompress + row_produced, over the 3-unit bounded scan.
	if got := retCost(t, fresh(), "UPDATE big SET w = 1 WHERE id = 1"); got != 16 {
		t.Fatalf("rewrite only: cost %d want 16", got)
	}
	if got := retCost(t, fresh(), "UPDATE big SET w = 1 WHERE id = 1 RETURNING t"); got != 30 {
		t.Fatalf("unassigned: cost %d want 30", got)
	}
}

func TestReturningInTransactions(t *testing.T) {
	db := retSetup(t)
	retRun(t, db, "BEGIN")
	if g := retGrid(retRows(t, db, "INSERT INTO t VALUES (95, 1, 1) RETURNING id")); g != "95" {
		t.Fatalf("got %s", g)
	}
	retRun(t, db, "ROLLBACK")
	if g := retGrid(retRows(t, db, "SELECT count(*) FROM t")); g != "3" {
		t.Fatalf("got %s", g)
	}
	// A write statement stays a write statement: 25006 in a READ ONLY block.
	retRun(t, db, "BEGIN READ ONLY")
	if got := retErrCode(t, db, "DELETE FROM t WHERE id = 1 RETURNING id"); got != "25006" {
		t.Fatalf("got %s want 25006", got)
	}
	retRun(t, db, "ROLLBACK")
}

func TestOldNewQualifiersPerStatement(t *testing.T) {
	db := retSetup(t)
	// INSERT: old is the all-NULL row (the key included); new = bare = the stored row.
	if g := retGrid(retRows(t, db, "INSERT INTO t VALUES (40, 4, 44) RETURNING old.v, new.v, v, old.id")); g != "NULL,4,4,NULL" {
		t.Fatalf("got %s", g)
	}
	// UPDATE: old = pre-assignment, new = bare = post; expressions span both versions;
	// case-insensitive like any identifier.
	if g := retGrid(retRows(t, db, "UPDATE t SET v = v + 5 WHERE id = 1 RETURNING OLD.v, New.v, v, new.v - old.v")); g != "10,15,15,5" {
		t.Fatalf("got %s", g)
	}
	// An unassigned column's two versions agree.
	if g := retGrid(retRows(t, db, "UPDATE t SET v = 0 WHERE id = 2 RETURNING old.w, new.w")); g != "200,200" {
		t.Fatalf("got %s", g)
	}
	// DELETE: old = bare = the deleted row; new is the all-NULL row.
	if g := retGrid(retRows(t, db, "DELETE FROM t WHERE id = 3 RETURNING old.v, new.v, v")); g != "30,NULL,30" {
		t.Fatalf("got %s", g)
	}
	// INSERT ... SELECT takes the same mapping.
	retRun(t, db, "CREATE TABLE src2 (a int32)")
	retRun(t, db, "INSERT INTO src2 VALUES (60)")
	if g := retGrid(retRows(t, db, "INSERT INTO t (id) SELECT a FROM src2 RETURNING old.v, new.v")); g != "NULL,7" {
		t.Fatalf("got %s", g)
	}
}

func TestOldNewNamingAndStar(t *testing.T) {
	db := retSetup(t)
	// §8: the qualifier never leaks into the output name (old.v is named v, like PG).
	o := retRun(t, db, "UPDATE t SET v = 1 WHERE id = 1 RETURNING old.v, new.w")
	if want := []string{"v", "w"}; !reflect.DeepEqual(o.ColumnNames, want) {
		t.Fatalf("names: got %v want %v", o.ColumnNames, want)
	}
	// The pseudo-relations are qualifier-only: `*` still expands exactly the table's columns.
	o = retRun(t, db, "INSERT INTO t (id) VALUES (41) RETURNING *")
	if want := []string{"id", "v", "w"}; !reflect.DeepEqual(o.ColumnNames, want) {
		t.Fatalf("star names: got %v want %v", o.ColumnNames, want)
	}
}

func TestOldNewShadowedByTableName(t *testing.T) {
	// A target table literally named old (or new) keeps the ordinary table-qualified
	// meaning — the row-version pseudo-relation is suppressed (PG-probed).
	db := NewDatabase()
	retRun(t, db, "CREATE TABLE old (x int32)")
	if g := retGrid(retRows(t, db, "INSERT INTO old VALUES (1) RETURNING old.x")); g != "1" {
		t.Fatalf("got %s", g) // the inserted value, NOT the NULL old side
	}
	if g := retGrid(retRows(t, db, "UPDATE old SET x = x + 1 RETURNING old.x")); g != "2" {
		t.Fatalf("got %s", g) // bare semantics = the NEW value
	}
	// The other qualifier still works alongside the shadowed one.
	if g := retGrid(retRows(t, db, "UPDATE old SET x = x + 1 RETURNING new.x")); g != "3" {
		t.Fatalf("got %s", g)
	}
	if g := retGrid(retRows(t, db, "DELETE FROM old RETURNING old.x")); g != "3" {
		t.Fatalf("got %s", g) // bare semantics = the deleted value
	}
	retRun(t, db, "CREATE TABLE new (x int32)")
	if g := retGrid(retRows(t, db, "INSERT INTO new VALUES (9) RETURNING new.x")); g != "9" {
		t.Fatalf("got %s", g)
	}
	if g := retGrid(retRows(t, db, "DELETE FROM new RETURNING new.x")); g != "9" {
		t.Fatalf("got %s", g) // table wins: the deleted value, NOT the NULL new side
	}
}

func TestOldNewInSubqueries(t *testing.T) {
	db := retSetup(t)
	retRun(t, db, "CREATE TABLE s2 (a int32, b int32)")
	retRun(t, db, "INSERT INTO s2 VALUES (1, 500)")
	// old/new resolve inside item subqueries like any outer reference (probed; jed has no
	// FROM-less SELECT, so the single-row s2 anchors the scalar subqueries).
	if g := retGrid(retRows(t, db,
		"UPDATE t SET v = v * 2 WHERE id = 2 RETURNING (SELECT old.v + 0 FROM s2), (SELECT old.v + s2.b FROM s2)")); g != "20,520" {
		t.Fatalf("got %s", g)
	}
	if g := retGrid(retRows(t, db,
		"DELETE FROM t WHERE id = 1 RETURNING (SELECT new.v FROM s2), (SELECT count(*) FROM s2 WHERE s2.a = old.id)")); g != "NULL,1" {
		t.Fatalf("got %s", g)
	}
}

func TestOldNewTouchedSet(t *testing.T) {
	// The touched-set sides (cost.md §3): old.col is ALWAYS a storage read — even when the
	// column is assigned; a DELETE's new.col is the constant NULL row and reads nothing.
	// Compressed 100k text at page_size 8192 = 13 slabs.
	big := "INSERT INTO big VALUES (1, 0, '" + strings.Repeat("x", 100_000) + "')"
	fresh := func() *Database {
		db := NewDatabase()
		retRun(t, db, "CREATE TABLE big (id int32 PRIMARY KEY, w int32, t text)")
		retRun(t, db, big)
		return db
	}
	// RETURNING the ASSIGNED column's old version forces the decompress the new version
	// avoided (4 there — see TestReturningGrowsTheTouchedSet): 3-unit bounded scan +
	// 13 value_decompress + row_produced (the shrunken rewrite attempts no compression).
	if got := retCost(t, fresh(), "UPDATE big SET t = 'short' WHERE id = 1 RETURNING old.t"); got != 17 {
		t.Fatalf("assigned old side: cost %d want 17", got)
	}
	// An unassigned column's old side costs the same as its new side (both storage reads):
	// 3 + 13 decompress + 13 rewrite-compress + 1 row_produced.
	if got := retCost(t, fresh(), "UPDATE big SET w = 1 WHERE id = 1 RETURNING old.t"); got != 30 {
		t.Fatalf("unassigned old side: cost %d want 30", got)
	}
	// DELETE RETURNING new.t reads nothing (NULL side): the 4-unit shape, value NULL.
	db := fresh()
	o := retRun(t, db, "DELETE FROM big WHERE id = 1 RETURNING new.t")
	if o.Kind != OutcomeQuery || retGrid(o.Rows) != "NULL" || o.Cost != 4 {
		t.Fatalf("delete new side: got %+v", o)
	}
}
