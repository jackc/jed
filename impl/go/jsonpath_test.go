package jed

// The jsonpath type (spec/design/jsonpath.md) — the per-core checks the conformance corpus cannot
// express (CLAUDE.md §10): the deliberate PG divergences (the deferred constructs are 0A000, where PG
// compiles them; a jsonpath is non-comparable / a jsonpath column is 0A000; an item method reads a
// string with jed's own number grammar). The agreeing behavior (the canonical render, malformed →
// 42601, the item methods) is oracle-clean in suites/json/jsonpath_literal.test and
// jsonpath_methods.test. Mirrors impl/rust/tests/jsonpath.rs.

import "testing"

func jpErr(t *testing.T, db dbHandle, sql string) string {
	t.Helper()
	_, err := queryOutcome(db, sql, nil)
	if err == nil {
		t.Fatalf("%s: expected an error", sql)
	}
	return err.(*EngineError).Code()
}

// The still-deferred path-expression constructs — the datetime item methods, arithmetic, like_regex /
// starts-with top-level predicates, $name variables, non-literal subscripts — are a deferred 0A000
// at compile. PostgreSQL compiles them, so each is a documented divergence; the supported subset is
// oracle-clean in suites/json/jsonpath_literal.test, jsonpath_query.test, and jsonpath_methods.test.
func TestJsonpathP1bConstructsAre0A000(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	for _, path := range []string{
		"$.a.datetime()", // a datetime item method
		`$.a.datetime("HH24")`,
		"$.a.date()",
		"$.a.time(3)",
		"$.a.time_tz()",
		"$.a.timestamp()",
		"$.a.timestamp_tz(2)",
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
	t.Parallel()
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
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	if got := jpErr(t, db, "CREATE TABLE t (p jsonpath)"); got != "0A000" {
		t.Errorf("a jsonpath column should be 0A000, got %s", got)
	}
}

// A jsonpath using a STILL-deferred construct (a datetime method, like_regex in a filter, like_regex
// as a top-level predicate) is 0A000 — it fails to compile. PostgreSQL evaluates these, so each is a
// documented divergence; the supported filter + query + match + method behavior is oracle-clean in
// suites/json/jsonpath_query.test and jsonpath_methods.test.
func TestJsonpathDeferredConstructsAre0A000(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	// A datetime item method.
	if got := jpErr(t, db, `SELECT jsonb_path_query_array('["2020-01-01"]', '$[*].datetime()')`); got != "0A000" {
		t.Errorf("jsonb_path_query_array with a datetime method should be 0A000, got %s", got)
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
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	for _, path := range []string{"$.", "$[", "$[1 to"} {
		if got := jpErr(t, db, "SELECT '"+path+"'::jsonpath"); got != "42601" {
			t.Errorf("malformed path %q should be 42601, got %s", path, got)
		}
	}
}

// An item method reads a string with jed's OWN number grammars (grammar.md §36): decimal digits only,
// no hex/octal/binary prefixes and no digit underscores, so those strings are 22036 where PostgreSQL
// 16+ (int8in / numeric_in, and glibc strtod behind float8in) accepts them.
func TestJsonpathMethodNumberGrammarDivergence(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	for _, c := range []struct{ doc, path string }{
		{`"0x10"`, "$.bigint()"},
		{`"1_000"`, "$.bigint()"},
		{`"0o17"`, "$.integer()"},
		{`"0x1A"`, "$.number()"},
		{`"1_0"`, "$.decimal()"},
		{`"0x10"`, "$.double()"},
	} {
		sql := "SELECT jsonb_path_query_first('" + c.doc + "', '" + c.path + "')"
		if got := jpErr(t, db, sql); got != "22036" {
			t.Errorf("%s %s should be 22036, got %s", c.doc, c.path, got)
		}
	}
}
