// AF7 (spec/design/array-functions.md §13): the polymorphic array function/operator surface over a
// COMPOSITE element type, plus unnest(composite[]). These complement the oracle corpus
// (suites/expr/array_composite_functions.test, suites/query/unnest_composite.test) with the two
// pieces the corpus can't carry: (a) the ARRAY[ROW(…)] constructor under a composite-column context
// (a jed extension PG rejects without a ::addr cast — the AC1 path), and (b) finer assertions on the
// composite-specific NULL rules. Every expected value is pinned against PostgreSQL 18. The Go core
// mirrors Rust/TS exactly (CLAUDE.md §2).
package jed

import "testing"

func addrDB(t *testing.T) *Session {
	t.Helper()
	db := memDB().Session(SessionOptions{})
	runArray(t, db, "CREATE TYPE addr AS (street text, zip i32)")
	return db
}

// val1 runs a one-row, one-column query and returns the rendered value ("NULL" for SQL-NULL).
func val1(t *testing.T, db dbHandle, sql string) string {
	t.Helper()
	rows := queryRendered(t, db, sql)
	if len(rows) != 1 || len(rows[0]) != 1 {
		t.Fatalf("%s: expected one row/one column, got %d rows", sql, len(rows))
	}
	return rows[0][0]
}

// col1 runs a one-column query and returns the rendered values.
func col1(t *testing.T, db dbHandle, sql string) []string {
	t.Helper()
	rows := queryRendered(t, db, sql)
	out := make([]string, len(rows))
	for i, r := range rows {
		if len(r) != 1 {
			t.Fatalf("%s: expected one column, got %d", sql, len(r))
		}
		out[i] = r[0]
	}
	return out
}

func eq(t *testing.T, got, want, what string) {
	t.Helper()
	if got != want {
		t.Errorf("%s: got %q, want %q", what, got, want)
	}
}

func TestAF7IntrospectorsOverComposite(t *testing.T) {
	db := addrDB(t)
	eq(t, val1(t, db, `SELECT array_length('{"(a,1)","(b,2)"}'::addr[], 1)`), "2", "array_length")
	eq(t, val1(t, db, `SELECT cardinality('{"(a,1)"}'::addr[])`), "1", "cardinality")
	eq(t, val1(t, db, `SELECT array_ndims('{"(a,1)"}'::addr[])`), "1", "array_ndims")
	eq(t, val1(t, db, `SELECT array_dims('{"(a,1)","(b,2)"}'::addr[])`), "[1:2]", "array_dims")
	eq(t, val1(t, db, `SELECT num_nulls(VARIADIC '{"(a,1)",NULL}'::addr[])`), "1", "num_nulls VARIADIC")
	eq(t, val1(t, db, `SELECT num_nonnulls('(a,)'::addr)`), "1", "num_nonnulls (NULL field is present)")
}

func TestAF7ContainmentOverComposite(t *testing.T) {
	db := addrDB(t)
	// A composite element with a NULL FIELD is comparable (record_eq) — @> matches it...
	eq(t, val1(t, db, `SELECT '{"(a,)"}'::addr[] @> '{"(a,)"}'::addr[]`), "true", "@> NULL-field match")
	// ...but a WHOLE-element NULL matches nothing, including another NULL (strict).
	eq(t, val1(t, db, `SELECT '{"(a,1)",NULL}'::addr[] @> '{NULL}'::addr[]`), "false", "@> whole-NULL no match")
	eq(t, val1(t, db, `SELECT '{"(a,1)"}'::addr[] <@ '{"(a,1)","(b,2)"}'::addr[]`), "true", "<@")
	eq(t, val1(t, db, `SELECT '{"(a,1)"}'::addr[] && '{"(a,1)","(b,2)"}'::addr[]`), "true", "&&")
	eq(t, val1(t, db, `SELECT (NULL::addr[] @> '{"(a,1)"}'::addr[]) IS NULL`), "true", "@> NULL whole-array")
}

// The AF7 code change #2: x op ANY/ALL(composite[]) uses the composite TOTAL ORDER, not bare-ROW 3VL.
func TestAF7QuantifiedOverCompositeTotalOrder(t *testing.T) {
	db := addrDB(t)
	eq(t, val1(t, db, `SELECT '(b,2)'::addr = ANY('{"(a,1)","(b,2)"}'::addr[])`), "true", "= ANY present")
	// THE FIX: a composite NULL FIELD is comparable (PG record_eq), so = ANY is TRUE (not bare-ROW NULL).
	eq(t, val1(t, db, `SELECT '(a,)'::addr = ANY('{"(a,)"}'::addr[])`), "true", "= ANY NULL-field")
	eq(t, val1(t, db, `SELECT '(a,)'::addr = ANY('{"(a,2)"}'::addr[])`), "false", "= ANY NULL vs present")
	// A WHOLE-element NULL is still UNKNOWN (strict at the value level).
	eq(t, val1(t, db, `SELECT ('(a,1)'::addr = ANY('{NULL}'::addr[])) IS NULL`), "true", "= ANY whole-NULL UNKNOWN")
	// Ordering quantifiers use the composite total order: the NULL zip sorts last.
	eq(t, val1(t, db, `SELECT '(a,1)'::addr < ANY('{"(a,)"}'::addr[])`), "true", "< ANY NULL sorts last")
	eq(t, val1(t, db, `SELECT '(a,)'::addr > ANY('{"(a,1)"}'::addr[])`), "true", "> ANY NULL sorts last")
	eq(t, val1(t, db, `SELECT '(a,)'::addr = ALL('{"(a,)","(a,)"}'::addr[])`), "true", "= ALL NULL-field")
	// Empty array → ANY FALSE, ALL TRUE; NULL array → NULL.
	eq(t, val1(t, db, `SELECT '(a,1)'::addr = ANY('{}'::addr[])`), "false", "= ANY empty")
	eq(t, val1(t, db, `SELECT '(a,1)'::addr = ALL('{}'::addr[])`), "true", "= ALL empty (vacuous)")
	eq(t, val1(t, db, `SELECT ('(a,1)'::addr = ANY(NULL::addr[])) IS NULL`), "true", "= ANY NULL array")
}

// The AF7 code change #1: unnest(composite[]).
func TestAF7UnnestComposite(t *testing.T) {
	db := addrDB(t)
	out, err := queryOutcome(db, `SELECT * FROM unnest('{"(a,1)","(b,2)"}'::addr[])`, nil)
	if err != nil {
		t.Fatalf("unnest composite: %v", err)
	}
	if len(out.ColumnNames) != 1 || out.ColumnNames[0] != "unnest" {
		t.Errorf("column names: %v", out.ColumnNames)
	}
	if len(out.ColumnTypes) != 1 || out.ColumnTypes[0] != "addr" {
		t.Errorf("column types: %v want [addr]", out.ColumnTypes)
	}
	got := col1(t, db, `SELECT * FROM unnest('{"(a,1)","(b,2)"}'::addr[])`)
	if len(got) != 2 || got[0] != "(a,1)" || got[1] != "(b,2)" {
		t.Errorf("unnest rows: %v", got)
	}
	// A NULL element → a NULL row; empty/NULL array → zero rows.
	got = col1(t, db, `SELECT * FROM unnest('{"(a,1)",NULL}'::addr[])`)
	if len(got) != 2 || got[0] != "(a,1)" || got[1] != "NULL" {
		t.Errorf("unnest NULL element: %v", got)
	}
	eq(t, val1(t, db, `SELECT count(*) FROM unnest('{}'::addr[])`), "0", "unnest empty")
	eq(t, val1(t, db, `SELECT count(*) FROM unnest(NULL::addr[])`), "0", "unnest NULL array")
	// Field access into the composite output column.
	if f := col1(t, db, `SELECT (u).zip FROM unnest('{"(a,1)","(b,2)"}'::addr[]) AS u`); len(f) != 2 || f[0] != "1" || f[1] != "2" {
		t.Errorf("(u).zip: %v", f)
	}
	// ORDER BY the whole composite column.
	if o := col1(t, db, `SELECT * FROM unnest('{"(b,2)","(a,1)"}'::addr[]) AS u ORDER BY u`); len(o) != 2 || o[0] != "(a,1)" || o[1] != "(b,2)" {
		t.Errorf("ORDER BY composite: %v", o)
	}
	// A non-array argument is still 42883.
	eq(t, errArray(t, db, `SELECT * FROM unnest('(a,1)'::addr)`), "42883", "unnest non-array")
}

// The jed extension: ARRAY[ROW(…)] under a composite-column context (not in the PG corpus).
func TestAF7ArrayRowConstructorUnderColumnContext(t *testing.T) {
	db := addrDB(t)
	runArray(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, items addr[])")
	// The ARRAY[ROW(…)] constructor takes the column's composite element type as context (no ::addr).
	runArray(t, db, "INSERT INTO t VALUES (1, ARRAY[ROW('Main', 90210), ROW('Side', 5)])")
	eq(t, val1(t, db, "SELECT (SELECT count(*) FROM unnest(o.items)) FROM t o ORDER BY id"), "2", "unnest stored column")
	eq(t, val1(t, db, "SELECT array_length(items, 1) FROM t ORDER BY id"), "2", "array_length stored")
	// The other operand must be a typed composite literal (a bare ARRAY[ROW]/ROW does not adapt).
	eq(t, val1(t, db, `SELECT items @> '{"(Side,5)"}'::addr[] FROM t ORDER BY id`), "true", "@> stored")
	eq(t, val1(t, db, `SELECT '(Side,5)'::addr = ANY(items) FROM t ORDER BY id`), "true", "= ANY stored")
}
