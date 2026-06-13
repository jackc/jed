package jed

// generate_series — the engine's first set-returning function, a FROM-clause row source
// (spec/design/functions.md §10, grammar.md §35). These complement the conformance corpus
// (spec/conformance/suites/query/generate_series.test) with finer-grained assertions: the
// generator's PostgreSQL edge cases (NULL → empty, step zero → 22023, descending step, the
// positive-default-step empty case, i64-overflow clean-stop), the synthetic-relation wiring
// (output column name/type, alias + qualified resolution, CROSS JOIN composition), the
// non-LATERAL rule ($N / correlated outer arg vs. a rejected sibling reference), the
// generated_row cost contract + the max_cost ceiling, and the deferred-form errors.

import "testing"

func genErrCode(t *testing.T, db *Database, sql string) string {
	t.Helper()
	_, err := Execute(db, sql)
	if err == nil {
		t.Fatalf("%q: expected an error", sql)
	}
	ee, ok := err.(*EngineError)
	if !ok {
		t.Fatalf("%q: expected an EngineError, got %v", sql, err)
	}
	return ee.Code()
}

func genInts(t *testing.T, db *Database, sql string) []int64 {
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

func eqGenInts(t *testing.T, got, want []int64, ctx string) {
	t.Helper()
	if len(got) != len(want) {
		t.Fatalf("%s: got %v, want %v", ctx, got, want)
	}
	for i := range got {
		if got[i] != want[i] {
			t.Fatalf("%s: got %v, want %v", ctx, got, want)
		}
	}
}

func TestGenerateSeriesTwoArgNamesAndTypes(t *testing.T) {
	db := NewDatabase()
	out, err := Execute(db, "SELECT * FROM generate_series(1, 5)")
	if err != nil {
		t.Fatalf("generate_series(1,5): %v", err)
	}
	if len(out.ColumnNames) != 1 || out.ColumnNames[0] != "generate_series" {
		t.Errorf("column names: %v", out.ColumnNames)
	}
	if len(out.ColumnTypes) != 1 || out.ColumnTypes[0] != "int64" {
		t.Errorf("column types: %v", out.ColumnTypes)
	}
	eqGenInts(t, genInts(t, db, "SELECT * FROM generate_series(1, 5)"), []int64{1, 2, 3, 4, 5}, "two-arg")
	// 5 generated_row + 5 row_produced; the integer-literal args are leaves (no operator_eval).
	if out.Cost != 10 {
		t.Errorf("cost = %d, want 10", out.Cost)
	}
}

func TestGenerateSeriesStepsAndDescending(t *testing.T) {
	db := NewDatabase()
	eqGenInts(t, genInts(t, db, "SELECT * FROM generate_series(1, 10, 2)"), []int64{1, 3, 5, 7, 9}, "step")
	eqGenInts(t, genInts(t, db, "SELECT * FROM generate_series(5, 1, -1)"), []int64{5, 4, 3, 2, 1}, "descending")
	eqGenInts(t, genInts(t, db, "SELECT * FROM generate_series(3, 3)"), []int64{3}, "single")
}

func TestGenerateSeriesEmptyCases(t *testing.T) {
	db := NewDatabase()
	eqGenInts(t, genInts(t, db, "SELECT * FROM generate_series(5, 1)"), []int64{}, "start past stop")
	if c := costOf(t, db, "SELECT * FROM generate_series(5, 1)"); c != 0 {
		t.Errorf("empty cost = %d, want 0", c)
	}
	for _, sql := range []string{
		"SELECT * FROM generate_series(NULL, 5)",
		"SELECT * FROM generate_series(1, NULL)",
		"SELECT * FROM generate_series(1, 5, NULL)",
	} {
		eqGenInts(t, genInts(t, db, sql), []int64{}, sql)
		if c := costOf(t, db, sql); c != 0 {
			t.Errorf("%s cost = %d, want 0", sql, c)
		}
	}
}

func TestGenerateSeriesZeroStep(t *testing.T) {
	db := NewDatabase()
	_, err := Execute(db, "SELECT * FROM generate_series(1, 5, 0)")
	ee, ok := err.(*EngineError)
	if !ok {
		t.Fatalf("expected an EngineError, got %v", err)
	}
	if ee.Code() != "22023" {
		t.Errorf("code = %s, want 22023", ee.Code())
	}
	if ee.Message != "step size cannot be equal to zero" {
		t.Errorf("message = %q", ee.Message)
	}
}

func TestGenerateSeriesAliasAndQualified(t *testing.T) {
	db := NewDatabase()
	// PG's single-column function-alias rule: `AS g` (or implicit `g`) renames the column to `g`.
	eqGenInts(t, genInts(t, db, "SELECT * FROM generate_series(1, 3) g"), []int64{1, 2, 3}, "implicit alias")
	out, err := Execute(db, "SELECT * FROM generate_series(1, 3) AS g")
	if err != nil || len(out.ColumnNames) != 1 || out.ColumnNames[0] != "g" {
		t.Errorf("aliased column name: %v (err %v)", out.ColumnNames, err)
	}
	eqGenInts(t, genInts(t, db, "SELECT g.g FROM generate_series(1, 3) AS g"), []int64{1, 2, 3}, "qualified")
	if c := genErrCode(t, db, "SELECT g.generate_series FROM generate_series(1, 3) AS g"); c != "42703" {
		t.Errorf("g.generate_series code = %s, want 42703", c)
	}
	eqGenInts(t, genInts(t, db, "SELECT generate_series.generate_series FROM generate_series(1, 2)"), []int64{1, 2}, "no alias label")
}

func TestGenerateSeriesComposition(t *testing.T) {
	db := NewDatabase()
	eqGenInts(t, genInts(t, db, "SELECT * FROM generate_series(1, 5) WHERE generate_series > 2"), []int64{3, 4, 5}, "where")
	eqGenInts(t, genInts(t, db, "SELECT * FROM generate_series(1, 5) ORDER BY generate_series DESC LIMIT 2"), []int64{5, 4}, "order-limit")
}

func TestGenerateSeriesCrossJoin(t *testing.T) {
	db := NewDatabase()
	mustExec(t, db, "CREATE TABLE t (id int32 PRIMARY KEY)")
	mustExec(t, db, "INSERT INTO t VALUES (10), (20)")
	rows := query(t, db, "SELECT * FROM t CROSS JOIN generate_series(1, 3) ORDER BY id, generate_series")
	want := [][2]int64{{10, 1}, {10, 2}, {10, 3}, {20, 1}, {20, 2}, {20, 3}}
	if len(rows) != len(want) {
		t.Fatalf("got %d rows, want %d", len(rows), len(want))
	}
	for i, r := range rows {
		if r[0].Int != want[i][0] || r[1].Int != want[i][1] {
			t.Errorf("row %d = (%d,%d), want %v", i, r[0].Int, r[1].Int, want[i])
		}
	}
}

func TestGenerateSeriesParam(t *testing.T) {
	db := NewDatabase()
	out, err := ExecuteParams(db, "SELECT * FROM generate_series(1, $1)", []Value{IntValue(3)})
	if err != nil {
		t.Fatalf("param: %v", err)
	}
	got := make([]int64, len(out.Rows))
	for i, r := range out.Rows {
		got[i] = r[0].Int
	}
	eqGenInts(t, got, []int64{1, 2, 3}, "param")
}

func TestGenerateSeriesCorrelatedOuterArg(t *testing.T) {
	db := NewDatabase()
	mustExec(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, n int32)")
	mustExec(t, db, "INSERT INTO t VALUES (1, 0), (2, 2), (3, 3)")
	// The inner generate_series arg references the outer row's n (non-LATERAL). Counts 0, 2, 3.
	eqGenInts(t, genInts(t, db,
		"SELECT (SELECT count(*) FROM generate_series(1, o.n)) FROM t o ORDER BY id"),
		[]int64{0, 2, 3}, "correlated")
}

func TestGenerateSeriesSiblingReferenceRejected(t *testing.T) {
	db := NewDatabase()
	mustExec(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, n int32)")
	mustExec(t, db, "INSERT INTO t VALUES (1, 3)")
	// A FROM-sibling reference inside the SRF args is NOT visible (no LATERAL) — undefined.
	if c := genErrCode(t, db, "SELECT * FROM t CROSS JOIN generate_series(1, t.n)"); c != "42P01" {
		t.Errorf("sibling ref code = %s, want 42P01", c)
	}
}

func TestGenerateSeriesCostCeiling(t *testing.T) {
	db := NewDatabase()
	if c := costOf(t, db, "SELECT * FROM generate_series(1, 4)"); c != 8 {
		t.Errorf("cost = %d, want 8", c)
	}
	// A runaway series aborts deterministically once accrued cost reaches the ceiling (54P01).
	db.SetMaxCost(50)
	if c := genErrCode(t, db, "SELECT * FROM generate_series(1, 1000000000)"); c != "54P01" {
		t.Errorf("ceiling code = %s, want 54P01", c)
	}
	db.SetMaxCost(0)
}

func TestGenerateSeriesMixedWidthPromotion(t *testing.T) {
	db := NewDatabase()
	out, err := Execute(db, "SELECT * FROM generate_series(CAST(1 AS int16), CAST(5 AS int32))")
	if err != nil {
		t.Fatalf("mixed width: %v", err)
	}
	if len(out.ColumnTypes) != 1 || out.ColumnTypes[0] != "int32" {
		t.Errorf("column types: %v", out.ColumnTypes)
	}
}

func TestGenerateSeriesI64OverflowStopsCleanly(t *testing.T) {
	db := NewDatabase()
	// Stepping past i64::MAX must STOP, not trap: only the last representable element is emitted.
	eqGenInts(t, genInts(t, db,
		"SELECT * FROM generate_series(9223372036854775806, 9223372036854775807, 2)"),
		[]int64{9223372036854775806}, "i64 boundary")
}

func TestGenerateSeriesDeferredAndBadCalls(t *testing.T) {
	db := NewDatabase()
	cases := []struct {
		sql, code string
	}{
		{"SELECT generate_series(1, 5)", "42883"},                // SELECT-list SRF deferred
		{"SELECT * FROM generate_series(1, 5) AS g(n)", "0A000"}, // column-alias list deferred
		{"SELECT * FROM generate_series(1)", "42883"},            // wrong arity
		{"SELECT * FROM generate_series('a', 5)", "42883"},       // non-integer arg
		{"SELECT * FROM nope(1, 5)", "42883"},                    // unknown table function
	}
	for _, c := range cases {
		if got := genErrCode(t, db, c.sql); got != c.code {
			t.Errorf("%q code = %s, want %s", c.sql, got, c.code)
		}
	}
}
