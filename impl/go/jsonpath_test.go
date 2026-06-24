package jed

// The jsonpath type (spec/design/jsonpath.md, slice P1a) — the per-core checks the conformance
// corpus cannot express (CLAUDE.md §10): the deliberate PG divergences (the deferred P1b constructs
// are 0A000, where PG compiles them; a jsonpath is non-comparable / a jsonpath column is 0A000).
// The agreeing behavior (the canonical render, malformed → 42601) is oracle-clean in
// suites/json/jsonpath_literal.test. Mirrors impl/rust/tests/jsonpath.rs.

import "testing"

func jpErr(t *testing.T, db *Database, sql string) string {
	t.Helper()
	_, err := Execute(db, sql)
	if err == nil {
		t.Fatalf("%s: expected an error", sql)
	}
	return err.(*EngineError).Code()
}

// The P1b path-expression constructs — filters ?(…), item methods .m(), arithmetic, like_regex,
// and the @/$name filter-context primaries — are a deferred 0A000 at compile (P1a parses only the
// structural-accessor subset). PostgreSQL compiles them, so each is a documented divergence; the
// supported subset is oracle-clean in suites/json/jsonpath_literal.test.
func TestJsonpathP1bConstructsAre0A000(t *testing.T) {
	db := NewDatabase()
	for _, path := range []string{
		"$.a ? (@ > 1)", // filter
		"$.a.size()",    // item method
		"$.a + 2",       // arithmetic
		"$[$x]",         // a non-literal subscript expression
		"$x",            // a path variable
	} {
		if got := jpErr(t, db, "SELECT '"+path+"'::jsonpath"); got != "0A000" {
			t.Errorf("path %q should defer 0A000, got %s", path, got)
		}
	}
}

// A jsonpath value is NOT comparable — every comparison / ORDER BY is 42883 (PG ships no opclass).
// A documented contract (jsonpath.md §1); only IS [NOT] NULL applies.
func TestJsonpathIsNotComparable(t *testing.T) {
	db := NewDatabase()
	if got := jpErr(t, db, "SELECT '$.a'::jsonpath = '$.a'::jsonpath"); got != "42883" {
		t.Errorf("jsonpath = jsonpath should be 42883, got %s", got)
	}
	if got := jpErr(t, db, "SELECT '$.a'::jsonpath < '$.b'::jsonpath"); got != "42883" {
		t.Errorf("jsonpath < jsonpath should be 42883, got %s", got)
	}
}

// A jsonpath COLUMN is 0A000 — jsonpath is literal-only this slice (P1a, like a J0-stage json
// column). PostgreSQL allows a jsonpath column, so this is a documented divergence.
func TestJsonpathColumnIsUnsupported(t *testing.T) {
	db := NewDatabase()
	if got := jpErr(t, db, "CREATE TABLE t (p jsonpath)"); got != "0A000" {
		t.Errorf("a jsonpath column should be 0A000, got %s", got)
	}
}

// A malformed jsonpath literal is 42601 (PG's syntax-error class), distinct from the 0A000 of a
// valid-but-unsupported construct. (The agreeing 42601 cases live in the corpus; this pins the
// distinction against the 0A000 ones above.)
func TestMalformedJsonpathIs42601(t *testing.T) {
	db := NewDatabase()
	for _, path := range []string{"$.", "$[", "$[1 to"} {
		if got := jpErr(t, db, "SELECT '"+path+"'::jsonpath"); got != "42601" {
			t.Errorf("malformed path %q should be 42601, got %s", path, got)
		}
	}
}
