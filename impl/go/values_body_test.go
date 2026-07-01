package jed

// VALUES-body derived tables — FROM (VALUES (e…),(e…)) [AS] v(c…) (spec/design/grammar.md §42). A
// parenthesized VALUES list used as a FROM relation: a computed relation of literal rows, the
// FROM-position sibling of INSERT … VALUES, reusing the derived-table seam (an anonymous,
// always-inlined single-reference CTE). These complement the conformance corpus
// (spec/conformance/suites/subquery/values_body.test) with finer-grained per-feature assertions:
// the default column1… names + the column-rename list, general constant expressions, per-column
// type unification across rows, composition with WHERE/ORDER BY/JOIN/aggregates, the intrinsic
// cost, and the error / narrowing codes (42601 / 42804 / 42703 / 42803 / 42P18).

import "testing"

func valuesNames(t *testing.T, db dbHandle, sql string) []string {
	t.Helper()
	out, err := db.Execute(sql, nil)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	if out.Kind != OutcomeQuery {
		t.Fatalf("expected a query result for %q", sql)
	}
	return out.ColumnNames
}

func valuesTypes(t *testing.T, db dbHandle, sql string) []string {
	t.Helper()
	out, err := db.Execute(sql, nil)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	return out.ColumnTypes
}

func valuesCost(t *testing.T, db dbHandle, sql string) int64 {
	t.Helper()
	out, err := db.Execute(sql, nil)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	return out.Cost
}

func TestValuesBodyBasicShape(t *testing.T) {
	db := dbWith(t)
	if got := queryIDs(t, db, "SELECT column1 FROM (VALUES (1), (2), (3)) AS v ORDER BY column1"); !eqInts(got, 1, 2, 3) {
		t.Errorf("basic VALUES body got %v", got)
	}
	if got := valuesNames(t, db, "SELECT * FROM (VALUES (1), (2)) AS v"); !eqStrs(got, "column1") {
		t.Errorf("default name got %v", got)
	}
}

func TestValuesBodyMultiColumnAndRename(t *testing.T) {
	db := dbWith(t)
	if got := valuesNames(t, db, "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS v"); !eqStrs(got, "column1", "column2") {
		t.Errorf("two-column default names got %v", got)
	}
	if got := valuesNames(t, db, "SELECT * FROM (VALUES (1, 'a')) AS v(n, s)"); !eqStrs(got, "n", "s") {
		t.Errorf("rename list got %v", got)
	}
	// A partial rename keeps the trailing body name.
	if got := valuesNames(t, db, "SELECT * FROM (VALUES (1, 'a')) AS v(n)"); !eqStrs(got, "n", "column2") {
		t.Errorf("partial rename got %v", got)
	}
	if got := queryIDs(t, db, "SELECT v.n FROM (VALUES (7), (8)) AS v(n) ORDER BY v.n"); !eqInts(got, 7, 8) {
		t.Errorf("qualified by alias got %v", got)
	}
}

func TestValuesBodyColumnTypeUnification(t *testing.T) {
	db := dbWith(t)
	// int + int -> int (all bare integer literals are i64 in jed).
	if got := valuesTypes(t, db, "SELECT column1 FROM (VALUES (1), (2)) AS v"); !eqStrs(got, "i64") {
		t.Errorf("int+int got %v", got)
	}
	// int + decimal -> decimal; the int value coerces.
	if got := valuesTypes(t, db, "SELECT column1 FROM (VALUES (1), (2.5)) AS v"); !eqStrs(got, "decimal") {
		t.Errorf("int+decimal got %v", got)
	}
	rows := query(t, db, "SELECT column1 FROM (VALUES (1), (2.5)) AS v ORDER BY column1")
	if len(rows) != 2 || rows[0][0].Kind != ValDecimal || rows[1][0].Kind != ValDecimal {
		t.Errorf("int+decimal coercion got %v", rows)
	}
	// anything + NULL keeps the other type.
	if got := valuesTypes(t, db, "SELECT column1 FROM (VALUES (1), (NULL)) AS v"); !eqStrs(got, "i64") {
		t.Errorf("int+NULL got %v", got)
	}
	// an all-NULL column is text (unknown -> text).
	if got := valuesTypes(t, db, "SELECT column1 FROM (VALUES (NULL), (NULL)) AS v"); !eqStrs(got, "text") {
		t.Errorf("all-NULL got %v", got)
	}
}

func TestValuesBodyParamTypedBySibling(t *testing.T) {
	db := dbWith(t)
	rows := queryRows(t, db, "SELECT column1 FROM (VALUES (1), ($1)) AS v ORDER BY column1", IntValue(7))
	if len(rows) != 2 || rows[0][0].Int != 1 || rows[1][0].Int != 7 {
		t.Errorf("param typed by sibling got %v", rows)
	}
}

func TestValuesBodyIntrinsicCost(t *testing.T) {
	db := dbWith(t)
	// VALUES body: row_produced per row (3) + outer SELECT row_produced (3) = 6.
	if got := valuesCost(t, db, "SELECT column1 FROM (VALUES (1), (2), (3)) AS v"); got != 6 {
		t.Errorf("cost got %d, want 6", got)
	}
	// (1+1) adds one operator_eval.
	if got := valuesCost(t, db, "SELECT column1 FROM (VALUES (1 + 1)) AS v"); got != 3 {
		t.Errorf("cost got %d, want 3", got)
	}
}

func TestValuesBodyErrors(t *testing.T) {
	db := dbWith(t)
	cases := []struct{ sql, code string }{
		{"SELECT * FROM (VALUES (1), (2, 3)) AS v", "42601"},         // differing arity
		{"SELECT * FROM (VALUES (1), ('a')) AS v", "42804"},          // types do not unify
		{"SELECT * FROM (VALUES (oops)) AS v", "42703"},              // column ref (non-LATERAL)
		{"SELECT * FROM (VALUES (sum(1))) AS v", "42803"},            // aggregate
		{"SELECT * FROM (VALUES ($1)) AS v", "42P18"},                // bare $1, no type
		{"SELECT * FROM (VALUES (1), (2) ORDER BY 1) AS v", "42601"}, // trailing ORDER BY (deferred)
		{"SELECT * FROM (VALUES (1)) AS v(a, b)", "42P10"},           // too many rename aliases
	}
	for _, c := range cases {
		if got := errCode(t, db, c.sql); got != c.code {
			t.Errorf("%q: got %s, want %s", c.sql, got, c.code)
		}
	}
}
