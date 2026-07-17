package jed

// generate_series — the engine's first set-returning function, a FROM-clause row source
// (spec/design/functions.md §10, grammar.md §35). These complement the conformance corpus
// (spec/conformance/suites/query/generate_series.test) with finer-grained assertions: the
// generator's PostgreSQL edge cases (NULL → empty, step zero → 22023, descending step, the
// positive-default-step empty case, i64-overflow clean-stop), the synthetic-relation wiring
// (output column name/type, alias + qualified resolution, CROSS JOIN composition), the
// arg-scope rule ($N / correlated outer arg), the generated_row cost contract + the max_cost
// ceiling, and the deferred-form errors. (An SRF is implicitly lateral — a sibling reference
// works, grammar.md §44 — covered by suites/joins/lateral.test.)

import "testing"

func genErrCode(t *testing.T, db dbHandle, sql string) string {
	t.Helper()
	_, err := queryOutcome(db, sql, nil)
	if err == nil {
		t.Fatalf("%q: expected an error", sql)
	}
	ee, ok := err.(*EngineError)
	if !ok {
		t.Fatalf("%q: expected an EngineError, got %v", sql, err)
	}
	return ee.Code()
}

func genInts(t *testing.T, db dbHandle, sql string) []int64 {
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

func TestGenerateSeriesZeroStep(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	_, err := queryOutcome(db, "SELECT * FROM generate_series(1, 5, 0)", nil)
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
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	// PG's single-column function-alias rule: `AS g` (or implicit `g`) renames the column to `g`.
	eqGenInts(t, genInts(t, db, "SELECT * FROM generate_series(1, 3) g"), []int64{1, 2, 3}, "implicit alias")
	out, err := queryOutcome(db, "SELECT * FROM generate_series(1, 3) AS g", nil)
	if err != nil || len(out.ColumnNames) != 1 || out.ColumnNames[0] != "g" {
		t.Errorf("aliased column name: %v (err %v)", out.ColumnNames, err)
	}
	eqGenInts(t, genInts(t, db, "SELECT g.g FROM generate_series(1, 3) AS g"), []int64{1, 2, 3}, "qualified")
	if c := genErrCode(t, db, "SELECT g.generate_series FROM generate_series(1, 3) AS g"); c != "42703" {
		t.Errorf("g.generate_series code = %s, want 42703", c)
	}
	eqGenInts(t, genInts(t, db, "SELECT generate_series.generate_series FROM generate_series(1, 2)"), []int64{1, 2}, "no alias label")
}

func TestGenerateSeriesParam(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	out, err := queryOutcome(db, "SELECT * FROM generate_series(1, $1)", []Value{IntValue(3)})
	if err != nil {
		t.Fatalf("param: %v", err)
	}
	got := make([]int64, len(out.Rows))
	for i, r := range out.Rows {
		got[i] = r[0].Int
	}
	eqGenInts(t, got, []int64{1, 2, 3}, "param")
}

func TestGenerateSeriesCostCeiling(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
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
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	out, err := queryOutcome(db, "SELECT * FROM generate_series(CAST(1 AS i16), CAST(5 AS i32))", nil)
	if err != nil {
		t.Fatalf("mixed width: %v", err)
	}
	if len(out.ColumnTypes) != 1 || out.ColumnTypes[0] != "i32" {
		t.Errorf("column types: %v", out.ColumnTypes)
	}
}

func TestGenerateSeriesI64OverflowStopsCleanly(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	// Stepping past i64::MAX must STOP, not trap: only the last representable element is emitted.
	eqGenInts(t, genInts(t, db,
		"SELECT * FROM generate_series(9223372036854775806, 9223372036854775807, 2)"),
		[]int64{9223372036854775806}, "i64 boundary")
}

func TestGenerateSeriesDeferredAndBadCalls(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	cases := []struct {
		sql, code string
	}{
		{"SELECT generate_series(1, 5)", "42883"},          // SELECT-list SRF deferred
		{"SELECT * FROM generate_series(1)", "42883"},      // wrong arity
		{"SELECT * FROM generate_series('a', 5)", "42883"}, // non-integer arg
		{"SELECT * FROM nope(1, 5)", "42883"},              // unknown table function
	}
	for _, c := range cases {
		if got := genErrCode(t, db, c.sql); got != c.code {
			t.Errorf("%q code = %s, want %s", c.sql, got, c.code)
		}
	}
}
