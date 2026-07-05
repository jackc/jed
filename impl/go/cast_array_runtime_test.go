package jed

// The three array-involving casts — the parts the PG-clean oracle corpus cannot express (the array
// cast follow-ons; spec/design/array.md §7, spec/types/casts.toml). The numeric/text element pairs
// AGREE with PostgreSQL and are oracle-checked in suites/cast/array_casts.test (run on every core);
// this file covers only what that corpus cannot: (a) array → text is EXPLICIT-only (an assignment /
// implicit context stays 42804); (b) the jed-only element casts uuid⇄bytea (succeeding where PG
// errors); (c) the forbidden scalar element pair → 42804 and a composite-element array cast → 0A000;
// (d) runtime text → f32[]/f64[] (the float renderer is in the determinism-exception ledger).
// Mirrors impl/rust/tests/cast_array_runtime.rs.

import "testing"

// castScalar evaluates SELECT <expr> (single row/column) and renders the value.
func castScalar(t *testing.T, db dbHandle, expr string) string {
	t.Helper()
	return castOne(t, db, "SELECT "+expr).Render()
}

// castErr returns the SQLSTATE of a statement expected to error.
func castErr(t *testing.T, db dbHandle, sql string) string {
	t.Helper()
	return castErrCode(t, db, sql)
}

// --- (a) array → text is EXPLICIT-only -----------------------------------------------------------

func TestArrayToTextIsExplicitOnly(t *testing.T) {
	t.Parallel()
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY, label text)")
	// Assignment context: an array value into a text column is a datatype mismatch, NOT a silent
	// array_out (PG would assignment-cast it).
	if got := castErr(t, db, "INSERT INTO t VALUES (1, ARRAY[1,2,3])"); got != "42804" {
		t.Fatalf("INSERT array into text col: want 42804, got %s", got)
	}
	if _, err := queryOutcome(db, "INSERT INTO t VALUES (1, '{1,2,3}')", nil); err != nil {
		t.Fatal(err)
	}
	// Implicit context: comparing a text column to an array value is a mismatch.
	if got := castErr(t, db, "SELECT id FROM t WHERE label = ARRAY[1,2,3]"); got != "42804" {
		t.Fatalf("text = array: want 42804, got %s", got)
	}
	// The explicit cast, by contrast, succeeds.
	if got := castScalar(t, db, "(ARRAY[1,2,3])::text"); got != "{1,2,3}" {
		t.Fatalf("(ARRAY[1,2,3])::text: want {1,2,3}, got %s", got)
	}
}

// --- (b) the jed-only element casts uuid ⇄ bytea (succeed where PG errors) ------------------------

func TestUuidArrayToByteaArrayAndBack(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	round := castScalar(t, db,
		"((ARRAY['00000000-0000-0000-0000-000000000001']::uuid[])::bytea[])::uuid[] = "+
			"ARRAY['00000000-0000-0000-0000-000000000001']::uuid[]")
	if round != "true" {
		t.Fatalf("uuid[] → bytea[] → uuid[] round-trip: want true, got %s", round)
	}
	// A bytea[] element of the wrong width on bytea[] → uuid[] traps 22P02 per element.
	if got := castErr(t, db, `SELECT (ARRAY['\x00'::bytea])::uuid[]`); got != "22P02" {
		t.Fatalf("wrong-width bytea[] → uuid[]: want 22P02, got %s", got)
	}
}

// --- (c) forbidden scalar element pair (42804) + composite-element array cast (0A000) -------------

func TestArrayForbiddenElementPairs(t *testing.T) {
	t.Parallel()
	db := memDB().Session(SessionOptions{})
	// A scalar element pair with no cast → 42804 (PG reports 42846). i32 → timestamp has no cast.
	if got := castErr(t, db, "SELECT (ARRAY[1,2,3]::i32[])::timestamp[]"); got != "42804" {
		t.Fatalf("i32[] → timestamp[]: want 42804, got %s", got)
	}
	// A composite-element array cast is the deferred composite cast surface → 0A000.
	if _, err := queryOutcome(db, "CREATE TYPE addr AS (street text, zip i32)", nil); err != nil {
		t.Fatal(err)
	}
	if got := castErr(t, db, "SELECT (ARRAY[ROW('Main',90210)::addr]::addr[])::text[]"); got != "0A000" {
		t.Fatalf("addr[] → text[]: want 0A000, got %s", got)
	}
	// A bind parameter into an array type stays the container-param narrowing (0A000).
	if got := castErr(t, db, "SELECT $1::i32[]"); got != "0A000" {
		t.Fatalf("$1::i32[]: want 0A000, got %s", got)
	}
}

// --- (d) runtime text → f32[] / f64[] element casts (float renderer is determinism-exempt) -------

func TestRuntimeTextToFloatArrays(t *testing.T) {
	t.Parallel()
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY, s text)")
	if _, err := queryOutcome(db, "INSERT INTO t VALUES (1, '{0.5,0.25,-1.5}')", nil); err != nil {
		t.Fatal(err)
	}
	got := castOne(t, db, "SELECT (s::float8[])::text FROM t WHERE id = 1").Render()
	if got != "{0.5,0.25,-1.5}" {
		t.Fatalf("text → f64[]: want {0.5,0.25,-1.5}, got %s", got)
	}
	// text → f32[] then widen to f64[] (0.5/0.25 are exact in binary32).
	if got := castScalar(t, db, "(((ARRAY['0.5','0.25']::text[])::float4[])::float8[])::text"); got != "{0.5,0.25}" {
		t.Fatalf("text[] → f32[] → f64[]: want {0.5,0.25}, got %s", got)
	}
	// i32[] → f64[] element-wise (numeric → float).
	if got := castScalar(t, db, "((ARRAY[1,2,3]::i32[])::float8[])::text"); got != "{1,2,3}" {
		t.Fatalf("i32[] → f64[]: want {1,2,3}, got %s", got)
	}
}
