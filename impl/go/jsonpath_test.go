package jed

// The jsonpath type (spec/design/jsonpath.md, slice P1a) — the per-core checks the conformance
// corpus cannot express (CLAUDE.md §10): the deliberate PG divergences (the deferred P1b constructs
// are 0A000, where PG compiles them; a jsonpath is non-comparable / a jsonpath column is 0A000).
// The agreeing behavior (the canonical render, malformed → 42601) is oracle-clean in
// suites/json/jsonpath_literal.test. Mirrors impl/rust/tests/jsonpath.rs.

import "testing"

func jpErr(t *testing.T, db dbHandle, sql string) string {
	t.Helper()
	_, err := queryOutcome(db, sql, nil)
	if err == nil {
		t.Fatalf("%s: expected an error", sql)
	}
	return err.(*EngineError).Code()
}

// The still-deferred path-expression constructs — item methods .m(), arithmetic, like_regex /
// starts-with top-level predicates, $name variables, non-literal subscripts — are a deferred 0A000
// at compile (P1b added filters ?(comparison) and top-level comparison predicates, but not these).
// PostgreSQL compiles them, so each is a documented divergence; the supported subset is oracle-clean
// in suites/json/jsonpath_literal.test and jsonpath_query.test.
func TestJsonpathP1bConstructsAre0A000(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	for _, path := range []string{
		"$.a.size()",          // item method
		"$.a + 2",             // arithmetic
		`$.a like_regex "x"`,  // a like_regex top-level predicate
		`$.a starts with "x"`, // a starts-with top-level predicate
		"$[$x]",               // a non-literal subscript expression
		"$x",                  // a path variable
	} {
		if got := jpErr(t, db, "SELECT '"+path+"'::jsonpath"); got != "0A000" {
			t.Errorf("path %q should defer 0A000, got %s", path, got)
		}
	}
}

// A jsonpath value is NOT comparable — every comparison / ORDER BY is 42883 (PG ships no opclass).
// A documented contract (jsonpath.md §1); only IS [NOT] NULL applies.
func TestJsonpathIsNotComparable(t *testing.T) {
	db := memDB().Session(SessionOptions{})
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
	db := memDB().Session(SessionOptions{})
	if got := jpErr(t, db, "CREATE TABLE t (p jsonpath)"); got != "0A000" {
		t.Errorf("a jsonpath column should be 0A000, got %s", got)
	}
}

// A jsonpath using a STILL-deferred construct (an item method, like_regex in a filter, like_regex as
// a top-level predicate) is 0A000 — it fails to compile. Filters ?(comparison) and top-level
// comparison predicates ($.a == 1, for jsonb_path_match / @@) now compile (P1b), but item methods /
// like_regex are a follow-on. PostgreSQL evaluates all of these, so each is a documented divergence;
// the supported filter + query + match behavior is oracle-clean in suites/json/jsonpath_query.test.
func TestJsonpathDeferredConstructsAre0A000(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	// An item method.
	if got := jpErr(t, db, "SELECT jsonb_path_query_array('[1,2,3]', '$[*].double()')"); got != "0A000" {
		t.Errorf("jsonb_path_query_array with an item method should be 0A000, got %s", got)
	}
	// like_regex inside a filter (a non-comparison predicate).
	if got := jpErr(t, db, "SELECT jsonb_path_exists('[\"x\"]', '$[*] ? (@ like_regex \"x\")')"); got != "0A000" {
		t.Errorf("like_regex inside a filter should be 0A000, got %s", got)
	}
	// like_regex as a top-level predicate.
	if got := jpErr(t, db, "SELECT jsonb_path_match('[\"x\"]', '$ like_regex \"x\"')"); got != "0A000" {
		t.Errorf("a like_regex top-level predicate should be 0A000, got %s", got)
	}
}

// A malformed jsonpath literal is 42601 (PG's syntax-error class), distinct from the 0A000 of a
// valid-but-unsupported construct. (The agreeing 42601 cases live in the corpus; this pins the
// distinction against the 0A000 ones above.)
func TestMalformedJsonpathIs42601(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	for _, path := range []string{"$.", "$[", "$[1 to"} {
		if got := jpErr(t, db, "SELECT '"+path+"'::jsonpath"); got != "42601" {
			t.Errorf("malformed path %q should be 42601, got %s", path, got)
		}
	}
}
