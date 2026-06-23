package jed

// Storable json / jsonb columns (spec/design/json.md, slices J1/J1b) — the per-core checks the
// conformance corpus cannot express (CLAUDE.md §10): the deliberate PG divergences (a json/jsonb
// PRIMARY KEY / index / UNIQUE is 0A000 where PG allows a jsonb key) and the on-disk internals (a
// large json/jsonb document spills out-of-line and round-trips through a whole-image serialize +
// reload). The agreeing behavior (store + canonical/verbatim round-trip, NULL) lives in
// suites/json/json_storage.test. Mirrors impl/rust/tests/json.rs.

import (
	"reflect"
	"strings"
	"testing"
)

// errJSON executes sql expecting an error and returns its SQLSTATE code.
func errJSON(t *testing.T, db *Database, sql string) string {
	t.Helper()
	_, err := Execute(db, sql)
	if err == nil {
		t.Fatalf("%s: expected an error", sql)
	}
	ee, ok := err.(*EngineError)
	if !ok {
		t.Fatalf("%s: expected an *EngineError, got %T", sql, err)
	}
	return ee.Code()
}

// TestJsonbPrimaryKeyIsUnsupported: a jsonb PRIMARY KEY is 0A000 — the order-preserving jsonb key
// (encoding.md §2.13) is authored but unexercised this slice (the staged-key narrowing
// text/decimal/bytea/array carried). PG ALLOWS a jsonb PK (it has a jsonb btree opclass), so this
// is a documented divergence.
func TestJsonbPrimaryKeyIsUnsupported(t *testing.T) {
	db := NewDatabase()
	if got := errJSON(t, db, "CREATE TABLE t (k jsonb PRIMARY KEY)"); got != "0A000" {
		t.Errorf("jsonb PRIMARY KEY: got %s, want 0A000", got)
	}
}

// TestJsonPrimaryKeyIsUnsupported: a json PRIMARY KEY is 0A000 — json is never keyable (it is not
// even comparable; PG ships no json opclass at all, so PG rejects it too, but with its own
// undefined-function shape).
func TestJsonPrimaryKeyIsUnsupported(t *testing.T) {
	db := NewDatabase()
	if got := errJSON(t, db, "CREATE TABLE t (k json PRIMARY KEY)"); got != "0A000" {
		t.Errorf("json PRIMARY KEY: got %s, want 0A000", got)
	}
}

// TestJsonbIndexAndUniqueAreUnsupported: a jsonb secondary index / UNIQUE is likewise 0A000 (no key
// encoding exercised yet).
func TestJsonbIndexAndUniqueAreUnsupported(t *testing.T) {
	db := NewDatabase()
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, j jsonb)")
	if got := errJSON(t, db, "CREATE INDEX i ON t (j)"); got != "0A000" {
		t.Errorf("CREATE INDEX on jsonb: got %s, want 0A000", got)
	}
	db2 := NewDatabase()
	if got := errJSON(t, db2, "CREATE TABLE u (id i32 PRIMARY KEY, j jsonb UNIQUE)"); got != "0A000" {
		t.Errorf("jsonb UNIQUE: got %s, want 0A000", got)
	}
}

// TestJsonbCrossFamilyComparisonIs42804: a jsonb comparison with a NON-jsonb family is 42804 (jed's
// cross-family convention, like uuid/bytea/range) — a documented divergence from PostgreSQL, which
// reports 42883 (operator does not exist: jsonb = integer). The agreeing json-non-comparable
// behavior (always 42883) and jsonb × jsonb ordering live in suites/json/json_compare.test.
func TestJsonbCrossFamilyComparisonIs42804(t *testing.T) {
	db := NewDatabase()
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, b jsonb)")
	// jsonb vs an integer / a real text value (not an adaptable string literal): 42804.
	if got := errJSON(t, db, "SELECT id FROM t WHERE b = 5"); got != "42804" {
		t.Errorf("jsonb = int: got %s, want 42804", got)
	}
	if got := errJSON(t, db, "SELECT id FROM t WHERE b = 'x'::text"); got != "42804" {
		t.Errorf("jsonb = text: got %s, want 42804", got)
	}
}

// TestInvalidJSONCastSourceIs42804: casting a non-text/json/jsonb source to json/jsonb is 42804
// (jed's invalid-cast convention, like "cannot cast boolean to X") — a documented divergence from
// PostgreSQL, which reports 42846 (cannot_coerce: cannot cast type integer to jsonb). The supported
// JSON cast matrix (json↔jsonb, json/jsonb→text, text→json/jsonb) is oracle-clean in
// suites/json/json_casts.test.
func TestInvalidJSONCastSourceIs42804(t *testing.T) {
	db := NewDatabase()
	if got := errJSON(t, db, "SELECT 5::jsonb"); got != "42804" {
		t.Errorf("5::jsonb: got %s, want 42804", got)
	}
	if got := errJSON(t, db, "SELECT (1.5)::json"); got != "42804" {
		t.Errorf("(1.5)::json: got %s, want 42804", got)
	}
	if got := errJSON(t, db, "SELECT true::jsonb"); got != "42804" {
		t.Errorf("true::jsonb: got %s, want 42804", got)
	}
}

// TestLargeJsonbSpillsAndRoundTrips: a large jsonb document (a long string node well past
// RECORD_MAX) spills onto an overflow chain and round-trips through a whole-image serialize + reload
// — exercising the jsonb body's spill/value_payload/value_from_payload (the tree decoded from a
// fresh cursor off the gathered chain). The rendered canonical form is preserved exactly.
func TestLargeJsonbSpillsAndRoundTrips(t *testing.T) {
	db := WithPageSize(4096)
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, j jsonb)")
	// A ~6000-byte string node — far above RECORD_MAX (~2034 at page 4096) — forces a spill.
	big := strings.Repeat("a", 6000)
	run(t, db, "INSERT INTO t VALUES (1, '\""+big+"\"')")
	// A second row with a small value, so the table spans the spilled + inline cases.
	run(t, db, "INSERT INTO t VALUES (2, '{\"k\": 42}')")

	image, err := db.ToImage(4096, 1)
	if err != nil {
		t.Fatalf("serialize image: %v", err)
	}
	loaded, err := LoadDatabase(image)
	if err != nil {
		t.Fatalf("load image: %v", err)
	}

	rows := queryRendered(t, loaded, "SELECT id, j FROM t ORDER BY id")
	if rows[0][0] != "1" {
		t.Errorf("row 0 id = %q, want 1", rows[0][0])
	}
	// The canonical render of the big string node.
	if want := "\"" + big + "\""; rows[0][1] != want {
		t.Errorf("row 0 j (len %d) != big string node (len %d)", len(rows[0][1]), len(want))
	}
	if want := []string{"2", "{\"k\": 42}"}; !reflect.DeepEqual(rows[1], want) {
		t.Errorf("row 1 = %v, want %v", rows[1], want)
	}
}

// TestLargeJsonSpillsVerbatim: a large verbatim json document spills and round-trips, preserving the
// input bytes EXACTLY (insignificant whitespace included — the json verbatim contract, §4).
func TestLargeJsonSpillsVerbatim(t *testing.T) {
	db := WithPageSize(4096)
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, j json)")
	// Verbatim text with irregular internal spacing, padded past RECORD_MAX.
	pad := strings.Repeat(" ", 6000)
	verbatim := "{ \"a\" :" + pad + "1 }"
	run(t, db, "INSERT INTO t VALUES (1, '"+strings.ReplaceAll(verbatim, "'", "''")+"')")

	image, err := db.ToImage(4096, 1)
	if err != nil {
		t.Fatalf("serialize image: %v", err)
	}
	loaded, err := LoadDatabase(image)
	if err != nil {
		t.Fatalf("load image: %v", err)
	}
	rows := queryRendered(t, loaded, "SELECT j FROM t WHERE id = 1")
	if rows[0][0] != verbatim { // verbatim bytes, whitespace preserved
		t.Errorf("verbatim json not preserved: got len %d, want len %d", len(rows[0][0]), len(verbatim))
	}
}

// TestJsonbAllNodeKindsRoundTrip: a jsonb column round-trips every node kind
// (object/array/number/string/bool/null) through a serialize + reload, confirming the tagged-node
// value codec decodes back to the canonical render.
func TestJsonbAllNodeKindsRoundTrip(t *testing.T) {
	db := WithPageSize(4096)
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, j jsonb)")
	run(t, db, "INSERT INTO t VALUES (1, '{\"a\": 1, \"b\": [true, false, null], \"c\": \"x\"}')")
	image, err := db.ToImage(4096, 1)
	if err != nil {
		t.Fatalf("serialize image: %v", err)
	}
	loaded, err := LoadDatabase(image)
	if err != nil {
		t.Fatalf("load image: %v", err)
	}
	rows := queryRendered(t, loaded, "SELECT j FROM t WHERE id = 1")
	if want := "{\"a\": 1, \"b\": [true, false, null], \"c\": \"x\"}"; rows[0][0] != want {
		t.Errorf("all-node-kinds render = %q, want %q", rows[0][0], want)
	}
}
