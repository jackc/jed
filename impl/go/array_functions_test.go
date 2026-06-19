package jed

// Array function/operator surface — AF1 (spec/design/array-functions.md): the polymorphic
// anyarray/anyelement resolution plus the introspection (array_ndims/array_length/array_lower/
// array_upper/cardinality/array_dims) and builder (array_append/array_prepend/array_cat) functions.
// Every expected value is pinned against PostgreSQL 18. Mirrors impl/rust/tests/array_functions.rs.

import "testing"

// valArrayFunc runs a one-column, one-row scalar query and returns the rendered value.
func valArrayFunc(t *testing.T, db *Database, sql string) string {
	t.Helper()
	rows := queryRendered(t, db, sql)
	if len(rows) != 1 || len(rows[0]) != 1 {
		t.Fatalf("%s: expected one row/one column, got %v", sql, rows)
	}
	return rows[0][0]
}

func TestArrayFuncMultidimAndCustomLB(t *testing.T) {
	db := NewDatabase()
	cases := map[string]string{
		"SELECT array_lower('[2:4]={7,8,9}'::int32[], 1)":          "2",
		"SELECT array_upper('[2:4]={7,8,9}'::int32[], 1)":          "4",
		"SELECT array_dims('[2:4]={7,8,9}'::int32[])":              "[2:4]",
		"SELECT array_ndims(ARRAY[ARRAY[1,2,3],ARRAY[4,5,6]])":     "2",
		"SELECT array_length(ARRAY[ARRAY[1,2,3],ARRAY[4,5,6]], 2)": "3",
		"SELECT cardinality(ARRAY[ARRAY[1,2,3],ARRAY[4,5,6]])":     "6",
		"SELECT array_dims(ARRAY[ARRAY[1,2,3],ARRAY[4,5,6]])":      "[1:2][1:3]",
	}
	for sql, want := range cases {
		if got := valArrayFunc(t, db, sql); got != want {
			t.Errorf("%s = %q, want %q", sql, got, want)
		}
	}
}

func TestArrayFuncErrors(t *testing.T) {
	db := NewDatabase()
	cases := map[string]string{
		"SELECT array_append(ARRAY[ARRAY[1,2],ARRAY[3,4]], 9)":     "22000",
		"SELECT array_prepend(9, ARRAY[ARRAY[1,2],ARRAY[3,4]])":    "22000",
		"SELECT array_cat(ARRAY[ARRAY[1,2]], ARRAY[ARRAY[3,4,5]])": "2202E",
		"SELECT array_cat(ARRAY[1,2], ARRAY['a','b'])":             "42883",
		"SELECT array_length(5, 1)":                                "42883",
		"SELECT array_append(ARRAY[1,2], 'x')":                     "42883",
	}
	for sql, want := range cases {
		if got := errArray(t, db, sql); got != want {
			t.Errorf("%s = %q, want %q", sql, got, want)
		}
	}
}
