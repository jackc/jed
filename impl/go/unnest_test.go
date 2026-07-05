// unnest — the polymorphic set-returning function (AF3, spec/design/array-functions.md §9), the
// engine's second FROM-clause SRF after generate_series. These complement the conformance corpus
// (spec/conformance/suites/query/unnest.test) with finer-grained assertions: the generator's
// output column name/type (the bound element type), the NULL/empty semantics, multidimensional
// flattening, the generated_row cost contract + the max_cost ceiling, and the deferred-form /
// strictness errors NOT in the oracle corpus (the SELECT-list position 42883, the bare-untyped-NULL
// 42P18, a wrong arity / non-array 42883). The Go core mirrors Rust/TS exactly (CLAUDE.md §2).
package jed

import (
	"strconv"
	"strings"
	"testing"
)

func unnestInts(t *testing.T, db dbHandle, sql string) []int64 {
	t.Helper()
	rows := query(t, db, sql)
	out := make([]int64, len(rows))
	for i, r := range rows {
		if len(r) != 1 {
			t.Fatalf("%q: expected one column, got %d", sql, len(r))
		}
		out[i] = r[0].Int
	}
	return out
}

func TestUnnestNamesAndElementType(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	// An untyped ARRAY[…] literal is i64[] (jed's literal typing).
	out, err := queryOutcome(db, "SELECT * FROM unnest(ARRAY[10,20,30])", nil)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if len(out.ColumnNames) != 1 || out.ColumnNames[0] != "unnest" {
		t.Errorf("column names: %v", out.ColumnNames)
	}
	if len(out.ColumnTypes) != 1 || out.ColumnTypes[0] != "i64" {
		t.Errorf("column types: %v", out.ColumnTypes)
	}
	// A typed '{…}'::i32[] literal pins the element type.
	out, _ = queryOutcome(db, "SELECT * FROM unnest('{1,2,3}'::i32[])", nil)
	if out.ColumnTypes[0] != "i32" {
		t.Errorf("i32[] element type = %s, want i32", out.ColumnTypes[0])
	}
	// A text[] argument → a text column.
	out, _ = queryOutcome(db, "SELECT * FROM unnest(ARRAY['a','b'])", nil)
	if out.ColumnTypes[0] != "text" {
		t.Errorf("text[] element type = %s, want text", out.ColumnTypes[0])
	}
}

func TestUnnestEmptyAndNullArraysYieldZeroRows(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	for _, sql := range []string{
		"SELECT * FROM unnest('{}'::i32[])",
		"SELECT * FROM unnest(NULL::i32[])",
	} {
		out, err := queryOutcome(db, sql, nil)
		if err != nil {
			t.Fatalf("%q: %v", sql, err)
		}
		if len(out.Rows) != 0 {
			t.Errorf("%q: expected 0 rows, got %d", sql, len(out.Rows))
		}
		if out.Cost != 0 {
			t.Errorf("%q: cost = %d, want 0", sql, out.Cost)
		}
	}
}

func TestUnnestAliasRenamesColumn(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	got := unnestInts(t, db, "SELECT g.g FROM unnest(ARRAY[7,8]) AS g ORDER BY g.g")
	eqGenInts(t, got, []int64{7, 8}, "alias")
	if code := genErrCode(t, db, "SELECT g.unnest FROM unnest(ARRAY[7,8]) AS g"); code != "42703" {
		t.Errorf("g.unnest code = %s, want 42703", code)
	}
}

func TestUnnestCorrelatedOuterArg(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, xs i32[])")
	mustExec(t, db, "INSERT INTO t VALUES (1, ARRAY[10,20]), (2, '{30}'), (3, NULL), (4, '{}')")
	// A correlated OUTER column resolves into the SRF arg of an enclosing-query subquery (the SRF is
	// the subquery's sole/first FROM item, so its args see the enclosing query — functions.md §10).
	rows := query(t, db, "SELECT id, (SELECT count(*) FROM unnest(o.xs)) AS n FROM t o ORDER BY id")
	want := [][2]int64{{1, 2}, {2, 1}, {3, 0}, {4, 0}}
	if len(rows) != 4 {
		t.Fatalf("correlated unnest: got %v", rows)
	}
	for i, w := range want {
		if rows[i][0].Int != w[0] || rows[i][1].Int != w[1] {
			t.Errorf("row %d: got (%d,%d), want %v", i, rows[i][0].Int, rows[i][1].Int, w)
		}
	}
	// A SIBLING FROM table's column IS now in scope for the SRF arg — an SRF is implicitly lateral
	// (grammar.md §44; the rows are pinned by suites/joins/lateral.test). Here we only assert the
	// prior non-LATERAL 42703/42P01 rejection is lifted: the bare and qualified forms succeed and
	// explode each row's array (1→{10,20}, 2→{30}, NULL/empty → no rows ⇒ 3 rows).
	if n := len(query(t, db, "SELECT id, u FROM t CROSS JOIN unnest(xs) AS u")); n != 3 {
		t.Errorf("implicitly-lateral bare sibling: got %d rows, want 3", n)
	}
	if n := len(query(t, db, "SELECT id, u FROM t CROSS JOIN unnest(t.xs) AS u")); n != 3 {
		t.Errorf("implicitly-lateral qualified sibling: got %d rows, want 3", n)
	}
}

func TestUnnestStrictnessAndDeferredErrors(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	// A non-array argument has no anyarray overload; unnest is single-arity.
	for _, sql := range []string{
		"SELECT * FROM unnest(5)",
		"SELECT * FROM unnest('hi')",
		"SELECT * FROM unnest(ARRAY[1], ARRAY[2])",
	} {
		if code := genErrCode(t, db, sql); code != "42883" {
			t.Errorf("%q code = %s, want 42883", sql, code)
		}
	}
	// A bare untyped NULL leaves ELEM undeterminable — jed's polymorphic posture (out of the corpus).
	if code := genErrCode(t, db, "SELECT * FROM unnest(NULL)"); code != "42P18" {
		t.Errorf("bare NULL code = %s, want 42P18", code)
	}
	// The SELECT-list SRF position is deferred (like generate_series) → 42883.
	if code := genErrCode(t, db, "SELECT unnest(ARRAY[1,2,3])"); code != "42883" {
		t.Errorf("SELECT-list unnest code = %s, want 42883", code)
	}
}

func TestUnnestGeneratedRowCostAndCeiling(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	// '{…}'::i32[] is a const (no operator_eval): 3 generated_row + 3 row_produced.
	out, _ := queryOutcome(db, "SELECT * FROM unnest('{1,2,3}'::i32[])", nil)
	if out.Cost != 6 {
		t.Errorf("cost = %d, want 6", out.Cost)
	}
	// A large array aborts deterministically once accrued cost reaches the ceiling (54P01), before
	// the whole thing materializes — the guard fires mid-generation, like generate_series.
	parts := make([]string, 1000)
	for i := range parts {
		parts[i] = strconv.Itoa(i + 1)
	}
	sql := "SELECT * FROM unnest('{" + strings.Join(parts, ",") + "}'::i32[])"
	db.SetMaxCost(50)
	if code := genErrCode(t, db, sql); code != "54P01" {
		t.Errorf("ceiling abort code = %s, want 54P01", code)
	}
}
