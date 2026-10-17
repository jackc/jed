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

// The datetime item methods are 0A000 at compile, by decision (jsonpath.md §3.1), and so is the
// like_regex `m` flag (jsonpath.md §4.3) unless `q` makes it moot. PostgreSQL compiles them, so each
// is a documented divergence; the supported surface is oracle-clean in suites/json/.
func TestJsonpathUnsupportedConstructsAre0A000(t *testing.T) {
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
		`$ ? (@ like_regex "^a" flag "m")`, // the multi-line flag
		`$ ? (@ like_regex "^a" flag "ims")`,
	} {
		if got := jpErr(t, db, "SELECT '"+path+"'::jsonpath"); got != "0A000" {
			t.Errorf("path %q should be 0A000, got %s", path, got)
		}
	}
	// Under `q` the pattern is a literal and `m` is ignored, as XQuery specifies.
	rows := queryRows(t, db, `SELECT jsonb_path_exists('"a^b"', '$ ? (@ like_regex "^b" flag "mq")')`)
	if v := rows[0][0]; v.Kind != ValBool || !v.boolVal() {
		t.Errorf("flag mq should match the literal ^b, got %v", v)
	}
}

// A like_regex pattern is jed's regex subset (regex.md §2), not PostgreSQL's ARE: a construct only
// ARE has is 2201B when the path compiles, where PostgreSQL accepts it (jsonpath.md §4.3).
func TestJsonpathLikeRegexSubset(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	for _, pattern := range []string{`\\y`, `\\u0041`, `(?i)a`, `a\\b`} {
		if got := jpErr(t, db, `SELECT '$ ? (@ like_regex "`+pattern+`")'::jsonpath`); got != "2201B" {
			t.Errorf("like_regex %q should be 2201B, got %s", pattern, got)
		}
	}
}

// A `.**` level is an integer literal without PostgreSQL 16+'s `_` separators and radix prefixes, as
// for jsonpath numbers (jsonpath.md §2): 42601 where PostgreSQL accepts them.
func TestJsonpathAnyLevelLiteral(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	for _, path := range []string{"$.**{1_000}", "$.**{0x10}", "$.**{2 to 0b11}"} {
		if got := jpErr(t, db, "SELECT '"+path+"'::jsonpath"); got != "42601" {
			t.Errorf("path %q should be 42601, got %s", path, got)
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

// A jsonpath using a datetime method is 0A000 — it fails to compile. PostgreSQL evaluates it, so it
// is a documented divergence (jsonpath.md §3.1).
func TestJsonpathDeferredConstructsAre0A000(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	if got := jpErr(t, db, `SELECT jsonb_path_query_array('["2020-01-01"]', '$[*].datetime()')`); got != "0A000" {
		t.Errorf("jsonb_path_query_array with a datetime method should be 0A000, got %s", got)
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

// .double() reads a string with jed's float grammar (float.md §4): no hex floats and no digit
// underscores, so "0x10" is 22036 where PostgreSQL's float8in (glibc strtod) accepts it. The integer
// and decimal methods read PostgreSQL's input forms (the corpus).
func TestJsonpathMethodNumberGrammarDivergence(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	for _, c := range []struct{ doc, path string }{
		{`"0x10"`, "$.double()"},
	} {
		sql := "SELECT jsonb_path_query_first('" + c.doc + "', '" + c.path + "')"
		if got := jpErr(t, db, sql); got != "22036" {
			t.Errorf("%s %s should be 22036, got %s", c.doc, c.path, got)
		}
	}
}
