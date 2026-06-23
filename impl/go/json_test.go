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

// TestJSONAccessorOperatorsAreDeferred: the `json` overloads of the accessor operators
// (`-> ->> #> #>>`) are a deferred 0A000 follow-on — they would have to preserve the verbatim
// sub-text (json.md §4), unlike the jsonb operators that work over the canonical node tree.
// PostgreSQL supports them, so this is a documented divergence (the jsonb operators are oracle-clean
// in suites/json/json_access.test). Mirrors impl/rust/tests/json.rs.
func TestJSONAccessorOperatorsAreDeferred(t *testing.T) {
	db := NewDatabase()
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, j json)")
	run(t, db, `INSERT INTO t VALUES (1, '{"a":1}')`)
	if got := errJSON(t, db, "SELECT j -> 'a' FROM t"); got != "0A000" {
		t.Errorf("j -> 'a': got %s, want 0A000", got)
	}
	if got := errJSON(t, db, "SELECT j ->> 'a' FROM t"); got != "0A000" {
		t.Errorf("j ->> 'a': got %s, want 0A000", got)
	}
	if got := errJSON(t, db, "SELECT j #> '{a}' FROM t"); got != "0A000" {
		t.Errorf("j #> '{a}': got %s, want 0A000", got)
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

// TestJsonbPrettyMatchesPG: jsonb_pretty renders the PG indented multi-line form (4-space indent,
// one space after `:`, a container ALWAYS multi-lines — an empty `{}` is `{` newline `}`). Pinned
// against the postgres:18 oracle; the multi-line output can't live in the line-based corpus.
// Mirrors impl/rust/tests/json.rs jsonb_pretty_matches_pg.
func TestJsonbPrettyMatchesPG(t *testing.T) {
	db := NewDatabase()
	q := func(sql string) string {
		t.Helper()
		return queryRendered(t, db, sql)[0][0]
	}
	if got, want := q("SELECT jsonb_pretty('{\"a\":1,\"b\":[1,2]}'::jsonb)"),
		"{\n    \"a\": 1,\n    \"b\": [\n        1,\n        2\n    ]\n}"; got != want {
		t.Errorf("jsonb_pretty nested = %q, want %q", got, want)
	}
	// An empty object/array still multi-lines (PG): `{` newline (indent) `}`.
	if got, want := q("SELECT jsonb_pretty('{}'::jsonb)"), "{\n}"; got != want {
		t.Errorf("jsonb_pretty empty object = %q, want %q", got, want)
	}
	if got, want := q("SELECT jsonb_pretty('{\"a\":{},\"b\":[]}'::jsonb)"),
		"{\n    \"a\": {\n    },\n    \"b\": [\n    ]\n}"; got != want {
		t.Errorf("jsonb_pretty nested empties = %q, want %q", got, want)
	}
}

// TestJSONArrayElementsSrfIsDeferred: the `json` set-returning variants `json_array_elements` /
// `json_array_elements_text` are a deferred 0A000 follow-on (they would have to preserve the verbatim
// element sub-text — json.md §4); the jsonb variants + `json_object_keys` are oracle-clean in
// suites/json/json_srf.test. Mirrors impl/rust/tests/json.rs.
func TestJSONArrayElementsSrfIsDeferred(t *testing.T) {
	db := NewDatabase()
	if got := errJSON(t, db, "SELECT * FROM json_array_elements('[1,2]'::json)"); got != "0A000" {
		t.Errorf("json_array_elements: got %s, want 0A000", got)
	}
	if got := errJSON(t, db, "SELECT * FROM json_array_elements_text('[1,2]'::json)"); got != "0A000" {
		t.Errorf("json_array_elements_text: got %s, want 0A000", got)
	}
}

// TestJSONEachSrfIsDeferred: the `json` two-column variants `json_each` / `json_each_text` are a
// deferred 0A000 follow-on (verbatim sub-text — json.md §4); the jsonb variants jsonb_each /
// jsonb_each_text are oracle-clean in suites/json/json_each.test. Mirrors impl/rust/tests/json.rs.
func TestJSONEachSrfIsDeferred(t *testing.T) {
	db := NewDatabase()
	if got := errJSON(t, db, `SELECT * FROM json_each('{"a":1}'::json)`); got != "0A000" {
		t.Errorf("json_each: got %s, want 0A000", got)
	}
	if got := errJSON(t, db, `SELECT * FROM json_each_text('{"a":1}'::json)`); got != "0A000" {
		t.Errorf("json_each_text: got %s, want 0A000", got)
	}
}

// TestToJsonbUnsupportedSourcesAreDeferred: `to_jsonb` over the type-info-dependent / float-divergent
// sources (float, composite, datetime, uuid, bytea, interval, multidim array) is a deferred 0A000
// follow-on; the supported set (scalars/jsonb/json/1-D arrays) is oracle-clean in
// suites/json/json_to_jsonb.test. Mirrors impl/rust/tests/json.rs.
func TestToJsonbUnsupportedSourcesAreDeferred(t *testing.T) {
	db := NewDatabase()
	if got := errJSON(t, db, "SELECT to_jsonb(1.5::f64)"); got != "0A000" {
		t.Errorf("to_jsonb(float): got %s, want 0A000", got)
	}
	if got := errJSON(t, db, "SELECT to_jsonb('2020-01-01'::date)"); got != "0A000" {
		t.Errorf("to_jsonb(date): got %s, want 0A000", got)
	}
	if got := errJSON(t, db, "SELECT to_jsonb(ARRAY[ARRAY[1,2],ARRAY[3,4]])"); got != "0A000" {
		t.Errorf("to_jsonb(multidim array): got %s, want 0A000", got)
	}
}

// TestJsonAggDeferredElementSourceIs0A000: json[b]_agg over a deferred-source value (float, like
// to_jsonb) is 0A000 — the aggregate reuses the to_jsonb element kernel (valueToNode), so the same
// float/datetime/composite/uuid/bytea/interval sources propagate the deferral
// (json-sql-functions.md §4). The supported element types are oracle-clean in
// suites/json/json_agg.test. Mirrors impl/rust/tests/json.rs json_agg_deferred_element_source_is_0a000.
func TestJsonAggDeferredElementSourceIs0A000(t *testing.T) {
	db := NewDatabase()
	run(t, db, "CREATE TABLE f (id i32 PRIMARY KEY, x f64)")
	run(t, db, "INSERT INTO f VALUES (1, 1.5)")
	if got := errJSON(t, db, "SELECT jsonb_agg(x) FROM f"); got != "0A000" {
		t.Errorf("jsonb_agg(f64): got %s, want 0A000", got)
	}
	if got := errJSON(t, db, "SELECT json_agg(x) FROM f"); got != "0A000" {
		t.Errorf("json_agg(f64): got %s, want 0A000", got)
	}
}

// TestJsonBuildersDeferredElementSourceIs0A000: the json/jsonb construction builders (to_json,
// json[b]_build_array, json[b]_build_object) reuse the to_jsonb element kernel (valueToNode /
// elemJsonText), so a deferred element source (float, like to_jsonb) propagates the 0A000 deferral
// (json-sql-functions.md §2). The supported element types are oracle-clean in
// suites/json/json_builders.test. Mirrors impl/rust/tests/json.rs.
func TestJsonBuildersDeferredElementSourceIs0A000(t *testing.T) {
	db := NewDatabase()
	if got := errJSON(t, db, "SELECT to_json(1.5::f64)"); got != "0A000" {
		t.Errorf("to_json(float): got %s, want 0A000", got)
	}
	if got := errJSON(t, db, "SELECT jsonb_build_array(1.5::f64)"); got != "0A000" {
		t.Errorf("jsonb_build_array(float): got %s, want 0A000", got)
	}
	if got := errJSON(t, db, "SELECT json_build_array(1.5::f64)"); got != "0A000" {
		t.Errorf("json_build_array(float): got %s, want 0A000", got)
	}
	if got := errJSON(t, db, "SELECT jsonb_build_object('k', 1.5::f64)"); got != "0A000" {
		t.Errorf("jsonb_build_object value float: got %s, want 0A000", got)
	}
}

// TestJsonBuildObjectNonScalarKeyIs0A000: a json[b]_build_object KEY of a non-scalar type (a date,
// which objectKeyText does not coerce) is a deferred 0A000 follow-on (json-sql-functions.md §2). A
// NULL key (22023) and the supported key coercions (text/int/decimal/bool) live in the PG-clean
// suites/json/json_builders.test. Mirrors impl/rust/tests/json.rs.
func TestJsonBuildObjectNonScalarKeyIs0A000(t *testing.T) {
	db := NewDatabase()
	if got := errJSON(t, db, "SELECT jsonb_build_object('2020-01-01'::date, 1)"); got != "0A000" {
		t.Errorf("jsonb_build_object(date key): got %s, want 0A000", got)
	}
}

// TestJsonAggCanonicalizesJsonElements: json_agg over a `json` element CANONICALIZES it (the element
// conversion runs through the jsonb node tree via valueToNode), dropping the input whitespace — a
// documented divergence from PostgreSQL, which preserves the verbatim sub-text (`[{ "a" : 1 }]`).
// This is the same verbatim divergence the json SRFs / accessor operators carry (json.md §4); it
// can't live in the PG-clean corpus. Mirrors impl/rust/tests/json.rs json_agg_canonicalizes_json_elements.
func TestJsonAggCanonicalizesJsonElements(t *testing.T) {
	db := NewDatabase()
	run(t, db, "CREATE TABLE j (id i32 PRIMARY KEY, doc json)")
	run(t, db, "INSERT INTO j VALUES (1, '{ \"a\" : 1 }')")
	// jed canonicalizes the element; PG would render the verbatim `[{ "a" : 1 }]`.
	if got, want := queryRendered(t, db, "SELECT json_agg(doc) FROM j")[0][0], "[{\"a\": 1}]"; got != want {
		t.Errorf("json_agg(json) = %q, want %q", got, want)
	}
}

// TestJsonbSetNullPathElementPropagatesNull: a NULL element inside the jsonb_set / jsonb_insert path
// array propagates a SQL NULL result — a documented divergence from PostgreSQL, which raises 22004
// ("path element at position N is null"). jed treats the path strictly, like the `#-` delete-path
// operator's text[] handling. The agreeing behavior (set/insert/no-op/22023/22P02) is oracle-clean in
// suites/json/json_set.test. Mirrors impl/rust/tests/json.rs jsonb_set_null_path_element_propagates_null.
func TestJsonbSetNullPathElementPropagatesNull(t *testing.T) {
	db := NewDatabase()
	if got := queryRendered(t, db, "SELECT jsonb_set('{\"a\":1}', ARRAY['a', NULL], '99')")[0][0]; got != "NULL" {
		t.Errorf("jsonb_set(NULL path element) = %q, want NULL", got)
	}
	if got := queryRendered(t, db, "SELECT jsonb_insert('{\"a\":1}', ARRAY[NULL], '99')")[0][0]; got != "NULL" {
		t.Errorf("jsonb_insert(NULL path element) = %q, want NULL", got)
	}
}
