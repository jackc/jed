package jed

// Array function/operator surface — AF6 (spec/design/array-functions.md §12): the VARIADIC call
// syntax + variadic overload resolution, spent on the engine's first VARIADIC built-ins
// num_nulls / num_nonnulls (count the NULL / non-NULL arguments → int32). Every expected value is
// pinned against PostgreSQL 18. Mirrors impl/rust/tests/array_variadic.rs.

import "testing"

func TestVariadicSpreadForm(t *testing.T) {
	db := NewDatabase()
	cases := map[string]string{
		"SELECT num_nulls(1, NULL, 3)":            "1",
		"SELECT num_nonnulls(1, NULL, 3)":         "2",
		"SELECT num_nulls(NULL)":                  "1", // a single NULL arg — never NULL (non-strict)
		"SELECT num_nonnulls(NULL)":               "0",
		"SELECT num_nulls(1, 'a', true, NULL)":    "1", // heterogeneous (the "any" element family)
		"SELECT num_nonnulls(1, 'a', true, NULL)": "3",
		"SELECT num_nulls(ARRAY[1,NULL,3])":       "0", // a single non-VARIADIC array is ONE value
		"SELECT num_nonnulls(ARRAY[1,NULL,3])":    "1",
	}
	for sql, want := range cases {
		if got := valArrayFunc(t, db, sql); got != want {
			t.Errorf("%s = %q, want %q", sql, got, want)
		}
	}
}

func TestVariadicArrayForm(t *testing.T) {
	db := NewDatabase()
	cases := map[string]string{
		"SELECT num_nulls(VARIADIC ARRAY[1,NULL,3])":             "1",
		"SELECT num_nonnulls(VARIADIC ARRAY[1,NULL,3])":          "2",
		"SELECT num_nulls(VARIADIC '{}'::int32[])":               "0",    // empty array → 0
		"SELECT num_nulls(VARIADIC '{{1,2},{NULL,4}}'::int32[])": "1",    // multidim flattens
		"SELECT num_nulls(VARIADIC NULL::int32[])":               "NULL", // NULL whole-array → NULL
		"SELECT num_nonnulls(VARIADIC NULL::int32[])":            "NULL",
	}
	for sql, want := range cases {
		if got := valArrayFunc(t, db, sql); got != want {
			t.Errorf("%s = %q, want %q", sql, got, want)
		}
	}
}

func TestVariadicErrors(t *testing.T) {
	db := NewDatabase()
	errs := map[string]string{
		"SELECT num_nulls(VARIADIC 5)":           "42804", // non-array VARIADIC operand
		"SELECT num_nulls(VARIADIC NULL)":        "42804", // bare untyped NULL, not 42P18
		"SELECT num_nulls()":                     "42883", // spread needs ≥1 arg
		"SELECT abs(VARIADIC ARRAY[1])":          "42883", // VARIADIC on a non-variadic function
		"SELECT num_nulls(x => 1)":               "42883", // named notation (no parameter names)
		"SELECT num_nulls(VARIADIC ARRAY[1], 2)": "42601", // VARIADIC must be last
	}
	for sql, want := range errs {
		if got := errArray(t, db, sql); got != want {
			t.Errorf("%s = %q, want %q", sql, got, want)
		}
	}
}

func TestVariadicOverColumn(t *testing.T) {
	db := NewDatabase()
	runArray(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])")
	runArray(t, db, "INSERT INTO t VALUES (1, ARRAY[1,NULL,3]), (2, '{}'), (3, NULL)")
	got := queryRendered(t, db, "SELECT num_nulls(VARIADIC xs) FROM t ORDER BY id")
	want := [][]string{{"1"}, {"0"}, {"NULL"}}
	if len(got) != len(want) {
		t.Fatalf("got %v, want %v", got, want)
	}
	for i := range want {
		if got[i][0] != want[i][0] {
			t.Errorf("row %d = %q, want %q", i, got[i][0], want[i][0])
		}
	}
}
