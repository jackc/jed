package jed

// Array function/operator surface — AF5 (spec/design/array-functions.md §11): the ANY/ALL/SOME
// quantified array comparisons (x = ANY(arr), x op ALL(arr)), the array spelling of IN and its
// universal dual. Every expected value is pinned against PostgreSQL 18 (the three-valued NULL rules
// especially). Mirrors impl/rust/tests/array_quantified.rs.

import "testing"

func TestQuantifiedAnyEqualityIsIn(t *testing.T) {
	db := NewDatabase()
	cases := map[string]string{
		"SELECT 1 = ANY(ARRAY[1,2,3])":       "true",
		"SELECT 5 = ANY(ARRAY[1,2,3])":       "false",
		"SELECT 2 = SOME(ARRAY[1,2,3])":      "true", // SOME is the synonym for ANY
		"SELECT 2 = ANY('{1,2,3}'::int64[])": "true",
	}
	for sql, want := range cases {
		if got := valArrayFunc(t, db, sql); got != want {
			t.Errorf("%s = %q, want %q", sql, got, want)
		}
	}
}

func TestQuantifiedAnyThreeValued(t *testing.T) {
	db := NewDatabase()
	cases := map[string]string{
		"SELECT NULL::int64 = ANY(ARRAY[1,2,3])": "NULL",  // NULL x, non-empty → NULL
		"SELECT 1 = ANY(ARRAY[2,NULL])":          "NULL",  // no TRUE, a NULL element → NULL
		"SELECT 2 = ANY(ARRAY[2,NULL])":          "true",  // a TRUE match dominates a NULL
		"SELECT 1 = ANY('{}'::int64[])":          "false", // empty → FALSE
		"SELECT 1 = ANY(NULL::int64[])":          "NULL",  // NULL array → NULL
	}
	for sql, want := range cases {
		if got := valArrayFunc(t, db, sql); got != want {
			t.Errorf("%s = %q, want %q", sql, got, want)
		}
	}
}

func TestQuantifiedAll(t *testing.T) {
	db := NewDatabase()
	cases := map[string]string{
		"SELECT 3 = ALL(ARRAY[3,3,3])":            "true",
		"SELECT 3 = ALL(ARRAY[3,3,4])":            "false",
		"SELECT 3 = ALL(ARRAY[4,NULL])":           "false", // a FALSE element dominates a NULL
		"SELECT 3 = ALL(ARRAY[3,NULL])":           "NULL",  // else a NULL → NULL
		"SELECT 3 = ALL('{}'::int64[])":           "true",  // empty → TRUE (vacuous)
		"SELECT NULL::int64 = ALL('{}'::int64[])": "true",  // empty beats a NULL x
		"SELECT 3 = ALL(NULL::int64[])":           "NULL",  // NULL array → NULL
	}
	for sql, want := range cases {
		if got := valArrayFunc(t, db, sql); got != want {
			t.Errorf("%s = %q, want %q", sql, got, want)
		}
	}
}

func TestQuantifiedOrderingAndShape(t *testing.T) {
	db := NewDatabase()
	cases := map[string]string{
		"SELECT 5 < ANY(ARRAY[1,2,10])": "true",
		"SELECT 5 > ALL(ARRAY[1,2,3])":  "true",
		"SELECT 5 <= ALL(ARRAY[5,6,7])": "true",
		"SELECT 5 >= ANY(ARRAY[9,8,5])": "true",
		"SELECT 5 > ALL(ARRAY[1,2,9])":  "false",
		// The comparison is over the FLATTENED element multiset (any dimensionality).
		"SELECT 3 = ANY(ARRAY[ARRAY[1,2],ARRAY[3,4]])": "true",
		"SELECT 4 = ALL(ARRAY[ARRAY[4,4],ARRAY[4,4]])": "true",
		// A custom lower bound is irrelevant (elements, not subscripts).
		"SELECT 20 = ANY('[5:6]={10,20}'::int64[])": "true",
		// text elements flow through.
		"SELECT 'b' = ANY(ARRAY['a','b','c'])": "true",
	}
	for sql, want := range cases {
		if got := valArrayFunc(t, db, sql); got != want {
			t.Errorf("%s = %q, want %q", sql, got, want)
		}
	}
}

func TestQuantifiedColumnLiteralAdaptation(t *testing.T) {
	db := NewDatabase()
	mustExec(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])")
	mustExec(t, db, "INSERT INTO t VALUES (1, ARRAY[10,20,30]), (2, ARRAY[40,50])")
	cases := map[string]string{
		"SELECT 20 = ANY(xs) FROM t WHERE id = 1":           "true",
		"SELECT count(*) FROM t WHERE 20 = ANY(xs)":         "1",
		"SELECT count(*) FROM t WHERE id = ANY(ARRAY[1,2])": "2",
	}
	for sql, want := range cases {
		if got := valArrayFunc(t, db, sql); got != want {
			t.Errorf("%s = %q, want %q", sql, got, want)
		}
	}
}

func TestQuantifiedErrors(t *testing.T) {
	db := NewDatabase()
	cases := map[string]string{
		"SELECT 1 = ANY(5)":              "42809", // a non-array right side
		"SELECT 1 = ANY(ARRAY['a','b'])": "42883", // incomparable element type
		"SELECT 1 = ANY(NULL)":           "42P18", // a bare untyped NULL operand
		"SELECT 1 = ANY(SELECT 1)":       "0A000", // the subquery quantifier form (deferred)
	}
	for sql, want := range cases {
		if got := errCode(t, db, sql); got != want {
			t.Errorf("%s code = %q, want %q", sql, got, want)
		}
	}
}
