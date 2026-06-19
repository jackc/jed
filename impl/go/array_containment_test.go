package jed

// Array function/operator surface — AF4 (spec/design/array-functions.md §10): the containment /
// overlap operators `@>` (contains), `<@` (contained by), `&&` (overlaps). Every expected value is
// pinned against PostgreSQL 18 (the strict-element-equality NULL rule especially — §10.1 #1).
// Mirrors impl/rust/tests/array_containment.rs.
//
// jed types a bare integer literal / ARRAY[…] constructor as int64, so the tests pair bare arrays
// with int64[] casts; the element hint comes from the FIRST array operand (§5 #8).

import "testing"

func TestContainsBasic(t *testing.T) {
	db := NewDatabase()
	cases := map[string]string{
		"SELECT ARRAY[1,2,3] @> ARRAY[2]":         "true",
		"SELECT ARRAY[1,2,3] @> ARRAY[2,4]":       "false",
		"SELECT ARRAY[1,2,3] @> ARRAY[3,2,1]":     "true", // order irrelevant
		"SELECT ARRAY[1,2,2,3] @> ARRAY[2,2,2]":   "true", // duplicates irrelevant
		"SELECT ARRAY[1,2,3] @> '{}'::int64[]":    "true", // empty contained by anything
		"SELECT '{}'::int64[] @> ARRAY[1]":        "false",
		"SELECT '{}'::int64[] @> '{}'::int64[]":   "true",
		"SELECT ARRAY['a','b','c'] @> ARRAY['b']": "true",
	}
	for sql, want := range cases {
		if got := valArrayFunc(t, db, sql); got != want {
			t.Errorf("%s = %q, want %q", sql, got, want)
		}
	}
}

func TestContainedByAndOverlaps(t *testing.T) {
	db := NewDatabase()
	cases := map[string]string{
		"SELECT ARRAY[2] <@ ARRAY[1,2,3]":    "true",
		"SELECT ARRAY[2,4] <@ ARRAY[1,2,3]":  "false",
		"SELECT '{}'::int64[] <@ ARRAY[1]":   "true",
		"SELECT ARRAY[1,2] && ARRAY[2,3]":    "true",
		"SELECT ARRAY[1,2] && ARRAY[3,4]":    "false",
		"SELECT ARRAY[1,2] && '{}'::int64[]": "false", // empty overlaps nothing
	}
	for sql, want := range cases {
		if got := valArrayFunc(t, db, sql); got != want {
			t.Errorf("%s = %q, want %q", sql, got, want)
		}
	}
}

func TestContainmentStrictNullElement(t *testing.T) {
	db := NewDatabase()
	// STRICT equality — a NULL element matches NOTHING, including another NULL (the inverse of the
	// search/edit functions' NOT DISTINCT FROM). All of these are FALSE, never NULL.
	cases := map[string]string{
		"SELECT ARRAY[1,2,NULL] @> ARRAY[2]":                 "true",
		"SELECT ARRAY[1,2,NULL] @> '{NULL}'::int64[]":        "false",
		"SELECT ARRAY[1,2,3] @> '{NULL}'::int64[]":           "false",
		"SELECT '{NULL,NULL}'::int64[] @> '{NULL}'::int64[]": "false",
		"SELECT ARRAY[1,NULL] && '{NULL}'::int64[]":          "false",
		"SELECT ARRAY[1,NULL] && ARRAY[1]":                   "true",
	}
	for sql, want := range cases {
		if got := valArrayFunc(t, db, sql); got != want {
			t.Errorf("%s = %q, want %q", sql, got, want)
		}
	}
}

func TestContainmentNullWholeArrayPropagates(t *testing.T) {
	db := NewDatabase()
	cases := map[string]string{
		"SELECT NULL::int64[] @> ARRAY[1]": "NULL",
		"SELECT ARRAY[1] @> NULL::int64[]": "NULL",
		"SELECT NULL::int64[] && ARRAY[1]": "NULL",
		"SELECT ARRAY[1] <@ NULL::int64[]": "NULL",
	}
	for sql, want := range cases {
		if got := valArrayFunc(t, db, sql); got != want {
			t.Errorf("%s = %q, want %q", sql, got, want)
		}
	}
}

func TestContainmentPrecedenceAndAdaptation(t *testing.T) {
	db := NewDatabase()
	cases := map[string]string{
		// @> shares ||'s precedence rung (left-assoc): `a || b @> c` is `(a||b) @> c`.
		"SELECT ARRAY[1,2] || ARRAY[3] @> ARRAY[3]": "true",
		"SELECT ARRAY[3] @> ARRAY[1 + 2]":           "true", // binds looser than +
		"SELECT ARRAY[1,2] @> ARRAY[2] = true":      "true", // binds tighter than =
		// The bare ARRAY[…] adapts to a typed (int32[]) array's element type when the typed array is left.
		"SELECT '{1,2,3}'::int32[] @> ARRAY[2]": "true",
		"SELECT '{2}'::int32[] <@ ARRAY[1,2,3]": "true",
	}
	for sql, want := range cases {
		if got := valArrayFunc(t, db, sql); got != want {
			t.Errorf("%s = %q, want %q", sql, got, want)
		}
	}
}

func TestContainmentErrors(t *testing.T) {
	db := NewDatabase()
	errs := map[string]string{
		"SELECT 5 @> ARRAY[1]":                "42883", // non-array operand
		"SELECT ARRAY[1] @> 5":                "42883",
		"SELECT ARRAY[1,2] @> ARRAY['a','b']": "42883", // element-type mismatch
		"SELECT ARRAY[1] && 5":                "42883",
		"SELECT 1 @ 2":                        "42601", // lone @ — no unary-@
		"SELECT 1 & 2":                        "42601", // lone & — no bitwise-and
	}
	for sql, want := range errs {
		if got := errArray(t, db, sql); got != want {
			t.Errorf("%s = %q, want %q", sql, got, want)
		}
	}
}
