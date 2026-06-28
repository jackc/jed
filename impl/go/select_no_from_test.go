package jed

// FROM-less SELECT — the select list evaluates over ONE virtual zero-column row, no table
// access (spec/design/grammar.md §34). These complement the conformance corpus
// (spec/conformance/suites/query/select_no_from.test) with finer-grained assertions: the
// virtual-row pipeline (WHERE / aggregates / DISTINCT / HAVING / LIMIT compose), the zero-scan
// cost contract (SELECT 1 = exactly 1 row_produced — spec/design/cost.md §3), composition in
// set operations / subqueries (correlated included) / INSERT ... SELECT, and the error surface
// (SELECT * → 42601 with PostgreSQL's exact message; a bare column — including the
// `SELECT distinct` lookahead consequence — → 42703; an untyped $1 → 42P18).

import (
	"sort"
	"testing"
)

func costOf(t *testing.T, db *engine, sql string) int64 {
	t.Helper()
	out, err := execute(db, sql)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	return out.Cost
}

func TestNoFromLiteralSelect(t *testing.T) {
	db := newEngine()
	out, err := execute(db, "SELECT 1")
	if err != nil {
		t.Fatalf("SELECT 1: %v", err)
	}
	if len(out.ColumnNames) != 1 || out.ColumnNames[0] != "?column?" {
		t.Errorf("column names: %v", out.ColumnNames)
	}
	if len(out.Rows) != 1 || len(out.Rows[0]) != 1 || out.Rows[0][0].Int != 1 {
		t.Errorf("rows: %v", out.Rows)
	}
	// No relation, no scan: zero page_read/storage_row_read — just the one row_produced.
	if out.Cost != 1 {
		t.Errorf("SELECT 1 cost = %d, want 1", out.Cost)
	}
}

func TestNoFromExpressionCost(t *testing.T) {
	db := newEngine()
	rows := query(t, db, "SELECT 1 + 2")
	if len(rows) != 1 || rows[0][0].Int != 3 {
		t.Errorf("rows: %v", rows)
	}
	// 1 operator_eval (the `+` node) + 1 row_produced.
	if c := costOf(t, db, "SELECT 1 + 2"); c != 2 {
		t.Errorf("SELECT 1 + 2 cost = %d, want 2", c)
	}
}

func TestNoFromWhereFiltersTheVirtualRow(t *testing.T) {
	db := newEngine()
	if rows := query(t, db, "SELECT 1 WHERE false"); len(rows) != 0 {
		t.Errorf("WHERE false rows: %v", rows)
	}
	// The constant filter is a leaf (no operator_eval) and no row is produced.
	if c := costOf(t, db, "SELECT 1 WHERE false"); c != 0 {
		t.Errorf("SELECT 1 WHERE false cost = %d, want 0", c)
	}
	if rows := query(t, db, "SELECT 1 WHERE 1 = 1"); len(rows) != 1 || rows[0][0].Int != 1 {
		t.Errorf("WHERE 1 = 1 rows: %v", rows)
	}
	if c := costOf(t, db, "SELECT 1 WHERE 1 = 1"); c != 2 { // the `=` + the produced row
		t.Errorf("SELECT 1 WHERE 1 = 1 cost = %d, want 2", c)
	}
}

func TestNoFromAggregatesFoldTheSingleGroup(t *testing.T) {
	db := newEngine()
	// The virtual row is the one input row of the whole-table group (aggregates.md §4).
	if rows := query(t, db, "SELECT count(*)"); len(rows) != 1 || rows[0][0].Int != 1 {
		t.Errorf("count(*) rows: %v", rows)
	}
	if c := costOf(t, db, "SELECT count(*)"); c != 2 { // 1 aggregate_accumulate + 1 row_produced
		t.Errorf("SELECT count(*) cost = %d, want 2", c)
	}
	// A false WHERE empties the input but the single group still emits.
	if rows := query(t, db, "SELECT count(*) WHERE false"); len(rows) != 1 || rows[0][0].Int != 0 {
		t.Errorf("count(*) WHERE false rows: %v", rows)
	}
	if c := costOf(t, db, "SELECT count(*) WHERE false"); c != 1 {
		t.Errorf("SELECT count(*) WHERE false cost = %d, want 1", c)
	}
	if rows := query(t, db, "SELECT max(5)"); len(rows) != 1 || rows[0][0].Int != 5 {
		t.Errorf("max(5) rows: %v", rows)
	}
	// HAVING filters the single group away.
	if rows := query(t, db, "SELECT 1 HAVING false"); len(rows) != 0 {
		t.Errorf("HAVING false rows: %v", rows)
	}
}

func TestNoFromDistinctAndWindow(t *testing.T) {
	db := newEngine()
	if rows := query(t, db, "SELECT DISTINCT 1"); len(rows) != 1 || rows[0][0].Int != 1 {
		t.Errorf("DISTINCT rows: %v", rows)
	}
	if rows := query(t, db, "SELECT 1 LIMIT 0"); len(rows) != 0 {
		t.Errorf("LIMIT 0 rows: %v", rows)
	}
	if rows := query(t, db, "SELECT 1 OFFSET 1"); len(rows) != 0 {
		t.Errorf("OFFSET 1 rows: %v", rows)
	}
}

func TestNoFromSetOperationOperands(t *testing.T) {
	db := newEngine()
	rows := query(t, db, "SELECT 1 UNION SELECT 2")
	got := make([]int64, len(rows))
	for i, r := range rows {
		got[i] = r[0].Int
	}
	sort.Slice(got, func(i, j int) bool { return got[i] < got[j] })
	if !eqInts(got, 1, 2) {
		t.Errorf("UNION rows: %v", got)
	}
	// Each operand costs 1; the combine is unmetered (cost.md §3).
	if c := costOf(t, db, "SELECT 1 UNION SELECT 2"); c != 2 {
		t.Errorf("UNION cost = %d, want 2", c)
	}
}

func TestNoFromSubqueries(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE t (id i32 PRIMARY KEY)",
		"INSERT INTO t VALUES (1), (2)",
	)
	// Uncorrelated FROM-less inner: folded once.
	if rows := query(t, db, "SELECT (SELECT 1)"); len(rows) != 1 || rows[0][0].Int != 1 {
		t.Errorf("uncorrelated rows: %v", rows)
	}
	// Correlated FROM-less inner: the zero-relation scope resolves o.id purely outward,
	// re-executed per outer row.
	if got := queryIDs(t, db, "SELECT (SELECT o.id) FROM t o ORDER BY id"); !eqInts(got, 1, 2) {
		t.Errorf("correlated rows: %v", got)
	}
	// 1 page_read + 2 storage_row_read + per outer row (×2): the subquery node's
	// operator_eval + the inner row_produced; + 2 outer row_produced = 9.
	if c := costOf(t, db, "SELECT (SELECT o.id) FROM t o ORDER BY id"); c != 9 {
		t.Errorf("correlated cost = %d, want 9", c)
	}
}

func TestNoFromInsertSelectSource(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)")
	out, err := execute(db, "INSERT INTO t SELECT 3")
	if err != nil {
		t.Fatalf("INSERT INTO t SELECT 3: %v", err)
	}
	if out.Cost != 1 { // exactly the embedded SELECT's cost
		t.Errorf("INSERT ... SELECT cost = %d, want 1", out.Cost)
	}
	if got := queryIDs(t, db, "SELECT id FROM t"); !eqInts(got, 3) {
		t.Errorf("rows after insert: %v", got)
	}
}

func TestNoFromStarIs42601WithPGMessage(t *testing.T) {
	db := newEngine()
	_, err := execute(db, "SELECT *")
	if err == nil {
		t.Fatal("SELECT *: expected an error")
	}
	ee, ok := err.(*EngineError)
	if !ok {
		t.Fatalf("SELECT *: not an EngineError: %v", err)
	}
	if ee.Code() != "42601" {
		t.Errorf("SELECT * code = %s, want 42601", ee.Code())
	}
	if ee.Message != "SELECT * with no tables specified is not valid" {
		t.Errorf("SELECT * message = %q", ee.Message)
	}
}

func TestNoFromBareColumnsResolveNothing(t *testing.T) {
	db := newEngine()
	if c := errCode(t, db, "SELECT nope"); c != "42703" {
		t.Errorf("SELECT nope = %s, want 42703", c)
	}
	// The DISTINCT two-token lookahead is unchanged: at end of input the word is a column
	// reference, not the modifier (grammar.md §34 — previously died at the FROM expect).
	if c := errCode(t, db, "SELECT distinct"); c != "42703" {
		t.Errorf("SELECT distinct = %s, want 42703", c)
	}
	if c := errCode(t, db, "SELECT from"); c != "42703" {
		t.Errorf("SELECT from = %s, want 42703", c)
	}
	// GROUP BY / ORDER BY keys are table columns only — always 42703 on a lone FROM-less SELECT.
	if c := errCode(t, db, "SELECT 1 GROUP BY nope"); c != "42703" {
		t.Errorf("GROUP BY nope = %s, want 42703", c)
	}
	if c := errCode(t, db, "SELECT 1 ORDER BY nope"); c != "42703" {
		t.Errorf("ORDER BY nope = %s, want 42703", c)
	}
}

func TestNoFromParams(t *testing.T) {
	db := newEngine()
	if c := paramErrCode(t, db, "SELECT $1", IntValue(7)); c != "42P18" {
		t.Errorf("SELECT $1 = %s, want 42P18", c)
	}
	// The sibling-operand rule (grammar.md §5) works without a FROM.
	rows := queryRows(t, db, "SELECT $1 + 1", IntValue(7))
	if len(rows) != 1 || rows[0][0].Int != 8 {
		t.Errorf("SELECT $1 + 1 rows: %v", rows)
	}
}
