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
func errJSON(t *testing.T, db dbHandle, sql string) string {
	t.Helper()
	_, err := db.Execute(sql, nil)
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
	db := memDB().Session(SessionOptions{})
	if got := errJSON(t, db, "CREATE TABLE t (k jsonb PRIMARY KEY)"); got != "0A000" {
		t.Errorf("jsonb PRIMARY KEY: got %s, want 0A000", got)
	}
}

// TestJsonPrimaryKeyIsUnsupported: a json PRIMARY KEY is 0A000 — json is never keyable (it is not
// even comparable; PG ships no json opclass at all, so PG rejects it too, but with its own
// undefined-function shape).
func TestJsonPrimaryKeyIsUnsupported(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	if got := errJSON(t, db, "CREATE TABLE t (k json PRIMARY KEY)"); got != "0A000" {
		t.Errorf("json PRIMARY KEY: got %s, want 0A000", got)
	}
}

// TestJsonbIndexAndUniqueAreUnsupported: a jsonb secondary index / UNIQUE is likewise 0A000 (no key
// encoding exercised yet).
func TestJsonbIndexAndUniqueAreUnsupported(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, j jsonb)")
	if got := errJSON(t, db, "CREATE INDEX i ON t (j)"); got != "0A000" {
		t.Errorf("CREATE INDEX on jsonb: got %s, want 0A000", got)
	}
	db2 := memDB().Session(SessionOptions{})
	if got := errJSON(t, db2, "CREATE TABLE u (id i32 PRIMARY KEY, j jsonb UNIQUE)"); got != "0A000" {
		t.Errorf("jsonb UNIQUE: got %s, want 0A000", got)
	}
}

// TestJsonbCrossFamilyComparisonIs42804: a jsonb comparison with a NON-jsonb family is 42804 (jed's
// cross-family convention, like uuid/bytea/range) — a documented divergence from PostgreSQL, which
// reports 42883 (operator does not exist: jsonb = integer). The agreeing json-non-comparable
// behavior (always 42883) and jsonb × jsonb ordering live in suites/json/json_compare.test.
func TestJsonbCrossFamilyComparisonIs42804(t *testing.T) {
	db := memDB().Session(SessionOptions{})
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
	db := memDB().Session(SessionOptions{})
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
	db := memDB().Session(SessionOptions{})
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
	db := newInMemoryWithPageSize(4096).Session(SessionOptions{})
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
	loaded, err := loadEngine(image)
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
	db := newInMemoryWithPageSize(4096).Session(SessionOptions{})
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, j json)")
	// Verbatim text with irregular internal spacing, padded past RECORD_MAX.
	pad := strings.Repeat(" ", 6000)
	verbatim := "{ \"a\" :" + pad + "1 }"
	run(t, db, "INSERT INTO t VALUES (1, '"+strings.ReplaceAll(verbatim, "'", "''")+"')")

	image, err := db.ToImage(4096, 1)
	if err != nil {
		t.Fatalf("serialize image: %v", err)
	}
	loaded, err := loadEngine(image)
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
	db := newInMemoryWithPageSize(4096).Session(SessionOptions{})
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, j jsonb)")
	run(t, db, "INSERT INTO t VALUES (1, '{\"a\": 1, \"b\": [true, false, null], \"c\": \"x\"}')")
	image, err := db.ToImage(4096, 1)
	if err != nil {
		t.Fatalf("serialize image: %v", err)
	}
	loaded, err := loadEngine(image)
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
	db := memDB().Session(SessionOptions{})
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
	db := memDB().Session(SessionOptions{})
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
	db := memDB().Session(SessionOptions{})
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
	db := memDB().Session(SessionOptions{})
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
	db := memDB().Session(SessionOptions{})
	run(t, db, "CREATE TABLE f (id i32 PRIMARY KEY, x f64)")
	run(t, db, "INSERT INTO f VALUES (1, 1.5)")
	if got := errJSON(t, db, "SELECT jsonb_agg(x) FROM f"); got != "0A000" {
		t.Errorf("jsonb_agg(f64): got %s, want 0A000", got)
	}
	if got := errJSON(t, db, "SELECT json_agg(x) FROM f"); got != "0A000" {
		t.Errorf("json_agg(f64): got %s, want 0A000", got)
	}
}

// TestJsonObjectAggDeferredValueSourceIs0A000: json[b]_object_agg over a deferred-source VALUE
// (float, like to_jsonb) is 0A000 — the value conversion reuses the to_jsonb element kernel
// (valueToNode). PG supports it, so this is a documented divergence; the supported value types are
// oracle-clean in suites/json/json_object_agg.test. Mirrors impl/rust/tests/json.rs
// json_object_agg_deferred_value_source_is_0a000.
func TestJsonObjectAggDeferredValueSourceIs0A000(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	run(t, db, "CREATE TABLE f (id i32 PRIMARY KEY, k text, x f64)")
	run(t, db, "INSERT INTO f VALUES (1, 'a', 1.5)")
	if got := errJSON(t, db, "SELECT jsonb_object_agg(k, x) FROM f"); got != "0A000" {
		t.Errorf("jsonb_object_agg(text, f64): got %s, want 0A000", got)
	}
	if got := errJSON(t, db, "SELECT json_object_agg(k, x) FROM f"); got != "0A000" {
		t.Errorf("json_object_agg(text, f64): got %s, want 0A000", got)
	}
}

// TestJsonBuildersDeferredElementSourceIs0A000: the json/jsonb construction builders (to_json,
// json[b]_build_array, json[b]_build_object) reuse the to_jsonb element kernel (valueToNode /
// elemJsonText), so a deferred element source (float, like to_jsonb) propagates the 0A000 deferral
// (json-sql-functions.md §2). The supported element types are oracle-clean in
// suites/json/json_builders.test. Mirrors impl/rust/tests/json.rs.
func TestJsonBuildersDeferredElementSourceIs0A000(t *testing.T) {
	db := memDB().Session(SessionOptions{})
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
	db := memDB().Session(SessionOptions{})
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
	db := memDB().Session(SessionOptions{})
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
	db := memDB().Session(SessionOptions{})
	if got := queryRendered(t, db, "SELECT jsonb_set('{\"a\":1}', ARRAY['a', NULL], '99')")[0][0]; got != "NULL" {
		t.Errorf("jsonb_set(NULL path element) = %q, want NULL", got)
	}
	if got := queryRendered(t, db, "SELECT jsonb_insert('{\"a\":1}', ARRAY[NULL], '99')")[0][0]; got != "NULL" {
		t.Errorf("jsonb_insert(NULL path element) = %q, want NULL", got)
	}
}

// TestArrayToJsonMultidimIs0A000: array_to_json of a MULTIDIMENSIONAL array is a deferred 0A000 (the
// to_jsonb multidim deferral) — a documented divergence from PostgreSQL, which renders nested arrays.
// The 1-D case is oracle-clean in suites/json/json_builders.test. Mirrors impl/rust/tests/json.rs
// array_to_json_multidim_is_0a000.
func TestArrayToJsonMultidimIs0A000(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	if got := errJSON(t, db, "SELECT array_to_json(ARRAY[ARRAY[1,2],ARRAY[3,4]])"); got != "0A000" {
		t.Errorf("array_to_json(multidim): got %s, want 0A000", got)
	}
}

// TestJsonSerializeJsonbDivergesFromPG: JSON_SERIALIZE over a jsonb value renders its canonical text
// — a documented divergence from PostgreSQL 18, which returns SQL NULL for a jsonb input (a PG quirk;
// only json input serializes). The json-input behavior is oracle-clean in suites/json/json_ctor.test.
// Mirrors impl/rust/tests/json.rs json_serialize_jsonb_diverges_from_pg.
func TestJsonSerializeJsonbDivergesFromPG(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	if got, want := queryRendered(t, db, `SELECT JSON_SERIALIZE('{"b":2,"a":1}'::jsonb)`)[0][0],
		`{"a": 1, "b": 2}`; got != want { // jed: the jsonb canonical text; PG 18: NULL
		t.Errorf("JSON_SERIALIZE(jsonb) = %q, want %q", got, want)
	}
}

// TestJsonScalarDeferredTypesAre0A000: JSON_SCALAR over a non-basic scalar (date / float / uuid / …)
// is a deferred 0A000 — only integer/decimal/boolean/text coerce this slice. PostgreSQL renders any
// scalar's text as a JSON string, so this is a documented divergence (the basic scalars are
// oracle-clean in suites/json/json_ctor.test). Mirrors impl/rust/tests/json.rs
// json_scalar_deferred_types_are_0a000.
func TestJsonScalarDeferredTypesAre0A000(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	if got := errJSON(t, db, "SELECT JSON_SCALAR('2020-01-01'::date)"); got != "0A000" {
		t.Errorf("JSON_SCALAR(date): got %s, want 0A000", got)
	}
	if got := errJSON(t, db, "SELECT JSON_SCALAR(1.5::f64)"); got != "0A000" {
		t.Errorf("JSON_SCALAR(f64): got %s, want 0A000", got)
	}
}

// TestJsonRecordCompositeArrayColumnIs0A000: a composite or array COLUMN in a record function's
// column-definition list is a deferred 0A000 (only scalar / json / jsonb columns coerce this slice).
// PostgreSQL supports them, so this is a documented divergence; the scalar columns are oracle-clean
// in suites/json/json_record.test. Mirrors impl/rust/tests/json.rs
// json_record_composite_array_column_is_0a000.
func TestJsonRecordCompositeArrayColumnIs0A000(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	run(t, db, "CREATE TYPE addr AS (street text, zip i32)")
	if got := errJSON(t, db, `SELECT * FROM jsonb_to_record('{"a":1}') AS t(a addr)`); got != "0A000" {
		t.Errorf("composite record column: got %s, want 0A000", got)
	}
	if got := errJSON(t, db, `SELECT * FROM jsonb_to_record('{"a":1}') AS t(a i32[])`); got != "0A000" {
		t.Errorf("array record column: got %s, want 0A000", got)
	}
}

// TestJsonPopulateNonCompositeAndComplexFieldDivergences: the R2 populate-record divergences the
// oracle corpus cannot express. A non-composite first argument → 42804 (PG's "first argument must be
// a composite type"); a composite whose FIELD is an array → 0A000 at row coercion (the same
// composite/array deferral R1 carries — only scalar / json / jsonb fields coerce this slice). The
// scalar-field cases are oracle-clean in suites/json/json_populate.test. Mirrors
// impl/rust/tests/json.rs json_populate_non_composite_and_complex_field_divergences.
func TestJsonPopulateNonCompositeAndComplexFieldDivergences(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	run(t, db, "CREATE TYPE addr AS (street text, zip i32)")
	run(t, db, "CREATE TYPE poly AS (name text, pts i32[])")
	if got := errJSON(t, db, `SELECT * FROM jsonb_populate_record(NULL::i32, '{"a":1}')`); got != "42804" {
		t.Errorf("non-composite base: got %s, want 42804", got)
	}
	if got := errJSON(t, db, `SELECT * FROM jsonb_populate_record(NULL::poly, '{"name":"x","pts":[1,2]}')`); got != "0A000" {
		t.Errorf("composite with array field: got %s, want 0A000", got)
	}
}

// TestSrfRenameOnlyColumnListIsDeferred: a rename-only column-alias list AS g(col) (no types) on a
// table function is a deferred 0A000 (only the TYPED column-definition list AS t(col type, …) — C0 —
// is parsed). PostgreSQL accepts a rename list on an SRF, so this is a documented divergence. Mirrors
// impl/rust/tests/json.rs srf_rename_only_column_list_is_deferred.
func TestSrfRenameOnlyColumnListIsDeferred(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	if got := errJSON(t, db, `SELECT * FROM jsonb_to_recordset('[{"a":1}]') AS t(a, b)`); got != "0A000" {
		t.Errorf("rename-only column list: got %s, want 0A000", got)
	}
}

// TestJSONQueryFnDeferredClausesAre0A000: the SQL/JSON query functions' deferred sub-clauses (S2,
// json-sql-functions.md §5) each resolve to 0A000 — PASSING path vars, a DEFAULT-expr behavior,
// JSON_QUERY OMIT QUOTES, and a JSON_QUERY non-json RETURNING. A per-core test (the oracle corpus is
// PG-clean, so a jed-defers case cannot live there). Mirrors impl/rust/tests/json.rs
// json_query_fn_deferred_clauses_are_0a000.
func TestJSONQueryFnDeferredClausesAre0A000(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	for _, sql := range []string{
		`SELECT JSON_VALUE('{"a":1}', '$.a' PASSING 1 AS x)`,
		`SELECT JSON_VALUE('{"a":1}', '$.b' DEFAULT 'z' ON EMPTY)`,
		`SELECT JSON_QUERY('{"a":1}', '$.a' OMIT QUOTES)`,
		`SELECT JSON_QUERY('{"a":1}', '$.a' RETURNING int)`,
	} {
		if got := errJSON(t, db, sql); got != "0A000" {
			t.Errorf("%s: got %s, want 0A000", sql, got)
		}
	}
}

// TestJSONTableDeferredFeaturesAre0A000: the deferred T1 sub-features of JSON_TABLE are 0A000 — an
// explicit PLAN, PASSING, an array column, a WRAPPER on a scalar column, OMIT QUOTES; an unknown
// column type is 42704. PostgreSQL supports the first set, so each is a documented divergence; the
// supported subset is oracle-clean in suites/json/json_table.test. Mirrors impl/rust/tests/json.rs
// json_table_deferred_features_are_0a000.
func TestJSONTableDeferredFeaturesAre0A000(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	for _, sql := range []string{
		`SELECT * FROM JSON_TABLE('{}', '$' COLUMNS (x i32 PATH '$.x') PLAN DEFAULT (x))`,
		`SELECT * FROM JSON_TABLE('{}', '$' PASSING 1 AS y COLUMNS (x i32 PATH '$.x'))`,
		`SELECT * FROM JSON_TABLE('{}', '$' COLUMNS (x i32[] PATH '$.x'))`,
		`SELECT * FROM JSON_TABLE('{}', '$' COLUMNS (x i32 PATH '$.x' WITH WRAPPER))`,
		`SELECT * FROM JSON_TABLE('{}', '$' COLUMNS (x i32 PATH '$.x' OMIT QUOTES))`,
	} {
		if got := errJSON(t, db, sql); got != "0A000" {
			t.Errorf("%s: got %s, want 0A000", sql, got)
		}
	}
	if got := errJSON(t, db, `SELECT * FROM JSON_TABLE('{}', '$' COLUMNS (x nosuchtype PATH '$.x'))`); got != "42704" {
		t.Errorf("unknown column type: got %s, want 42704", got)
	}
}
