package jed

// Phase 1: the general expression evaluator — integer arithmetic (+ - * / %, unary
// minus), the expression-only boolean type, comparisons-as-values, AND/OR/NOT Kleene
// connectives, operator precedence, and parentheses. These complement the conformance
// corpus (spec/conformance/suites/expr/) with finer-grained per-feature assertions.

import "testing"

// scalar runs a single-row, single-column query and returns the lone value.
func scalar(t *testing.T, db *engine, sql string) Value {
	t.Helper()
	rows := query(t, db, sql)
	if len(rows) != 1 || len(rows[0]) != 1 {
		t.Fatalf("%q: expected one row of one column, got %v", sql, rows)
	}
	return rows[0][0]
}

func TestComparisonsProjectBooleans(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE t (id i32 PRIMARY KEY, a i32, b i32)",
		"INSERT INTO t VALUES (1, 5, 5)",
		"INSERT INTO t VALUES (2, 5, 9)",
		"INSERT INTO t VALUES (3, 5, NULL)",
	)
	rows := query(t, db, "SELECT a = b FROM t ORDER BY id")
	want := []Value{BoolValue(true), BoolValue(false), NullValue()}
	for i, w := range want {
		if rows[i][0] != w {
			t.Errorf("row %d = %v, want %v", i, rows[i][0], w)
		}
	}
	if got := scalar(t, db, "SELECT TRUE FROM t WHERE id = 1"); got != BoolValue(true) {
		t.Errorf("TRUE = %v", got)
	}
	if BoolValue(true).Render() != "true" || BoolValue(false).Render() != "false" {
		t.Errorf("boolean render mismatch")
	}
}
