// Storable json / jsonb columns + jsonb comparison/ordering (spec/design/json.md, slices J1/J1b/J2)
// — the per-core checks the conformance corpus cannot express (CLAUDE.md §10): the deliberate PG
// divergences. The agreeing json-non-comparable behavior (always 42883) and jsonb × jsonb ordering
// live in suites/json/json_compare.test; storage round-trips in suites/json/json_storage.test.
// Mirrors impl/rust/tests/json.rs and impl/go/json_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import { dbWith, errCode, query } from "./util.ts";

// A `jsonb` comparison with a NON-jsonb family is 42804 (jed's cross-family convention, like
// uuid/bytea/range) — a documented divergence from PostgreSQL, which reports 42883 (operator does
// not exist: jsonb = integer). The agreeing json-non-comparable behavior (always 42883) and
// jsonb × jsonb ordering live in suites/json/json_compare.test.
test("jsonb cross-family comparison is 42804", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, b jsonb)"]);
  // jsonb vs an integer / a real text value (not an adaptable string literal): 42804.
  assert.equal(
    errCode(() => db.execute("SELECT id FROM t WHERE b = 5")),
    "42804",
  );
  assert.equal(
    errCode(() => db.execute("SELECT id FROM t WHERE b = 'x'::text")),
    "42804",
  );
});

// Casting a non-text/json/jsonb source to json/jsonb is 42804 (jed's invalid-cast convention, like
// "cannot cast boolean to X") — a documented divergence from PostgreSQL, which reports 42846
// (cannot_coerce: cannot cast type integer to jsonb). The supported JSON cast matrix (json↔jsonb,
// json/jsonb→text, text→json/jsonb) is oracle-clean in suites/json/json_casts.test.
test("invalid json cast source is 42804", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => db.execute("SELECT 5::jsonb")),
    "42804",
  );
  assert.equal(
    errCode(() => db.execute("SELECT (1.5)::json")),
    "42804",
  );
  assert.equal(
    errCode(() => db.execute("SELECT true::jsonb")),
    "42804",
  );
});

// The `json` overloads of the accessor operators (`-> ->> #> #>>`) are a deferred 0A000 follow-on
// — they would have to preserve the verbatim sub-text (json.md §4), unlike the jsonb operators that
// work over the canonical node tree. PostgreSQL supports them, so this is a documented divergence
// (the jsonb operators are oracle-clean in suites/json/json_access.test). Mirrors
// impl/rust/tests/json.rs and impl/go/json_test.go.
test("json accessor operators are deferred", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, j json)",
    `INSERT INTO t VALUES (1, '{"a":1}')`,
  ]);
  assert.equal(
    errCode(() => db.execute("SELECT j -> 'a' FROM t")),
    "0A000",
  );
  assert.equal(
    errCode(() => db.execute("SELECT j ->> 'a' FROM t")),
    "0A000",
  );
  assert.equal(
    errCode(() => db.execute("SELECT j #> '{a}' FROM t")),
    "0A000",
  );
});

// jsonb_pretty renders the PG indented multi-line form (4-space indent, one space after `:`, a
// container ALWAYS multi-lines — an empty `{}` is `{` newline `}`). Pinned against the postgres:18
// oracle; the multi-line output can't live in the line-based corpus. Mirrors impl/rust/tests/json.rs
// jsonb_pretty_matches_pg and impl/go/json_test.go TestJsonbPrettyMatchesPG.
test("jsonb_pretty matches PG", () => {
  const db = dbWith([]);
  const pretty = (sql: string): string => query(db, sql)[0]![0]!;
  assert.equal(
    pretty(`SELECT jsonb_pretty('{"a":1,"b":[1,2]}'::jsonb)`),
    '{\n    "a": 1,\n    "b": [\n        1,\n        2\n    ]\n}',
  );
  // An empty object/array still multi-lines (PG): `{` newline (indent) `}`.
  assert.equal(pretty(`SELECT jsonb_pretty('{}'::jsonb)`), "{\n}");
  assert.equal(
    pretty(`SELECT jsonb_pretty('{"a":{},"b":[]}'::jsonb)`),
    '{\n    "a": {\n    },\n    "b": [\n    ]\n}',
  );
});

// The `json` set-returning variants `json_array_elements` / `json_array_elements_text` are a
// deferred 0A000 follow-on (they would have to preserve the verbatim element sub-text — json.md §4);
// the jsonb variants + `json_object_keys` are oracle-clean in suites/json/json_srf.test. Mirrors
// impl/rust/tests/json.rs json_array_elements_srf_is_deferred and impl/go/json_test.go
// TestJSONArrayElementsSrfIsDeferred.
test("json_array_elements SRF is deferred", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => db.execute("SELECT * FROM json_array_elements('[1,2]'::json)")),
    "0A000",
  );
  assert.equal(
    errCode(() => db.execute("SELECT * FROM json_array_elements_text('[1,2]'::json)")),
    "0A000",
  );
});

// The `json` two-column SRFs `json_each` / `json_each_text` are a deferred 0A000 follow-on (they
// would have to preserve the verbatim member sub-text — json.md §4); the jsonb variants are
// oracle-clean in suites/json/json_each.test. PostgreSQL supports the json variants, so this is a
// documented divergence (the json_array_elements precedent). Mirrors impl/rust/tests/json.rs
// json_each_srf_is_deferred and impl/go/json_test.go TestJSONEachSrfIsDeferred.
test("json_each SRF is deferred", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => db.execute(`SELECT * FROM json_each('{"a":1}'::json)`)),
    "0A000",
  );
  assert.equal(
    errCode(() => db.execute(`SELECT * FROM json_each_text('{"a":1}'::json)`)),
    "0A000",
  );
});

// `to_jsonb` over the type-info-dependent / float-divergent sources (float, composite, datetime,
// uuid, bytea, interval, multidim array) is a deferred 0A000 follow-on; the supported set
// (scalars/jsonb/json/1-D arrays) is oracle-clean in suites/json/json_to_jsonb.test. Mirrors
// impl/rust/tests/json.rs to_jsonb_unsupported_sources_are_deferred and impl/go/json_test.go
// TestToJsonbUnsupportedSourcesAreDeferred.
test("to_jsonb unsupported sources are deferred", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => db.execute("SELECT to_jsonb(1.5::f64)")),
    "0A000",
  );
  assert.equal(
    errCode(() => db.execute("SELECT to_jsonb('2020-01-01'::date)")),
    "0A000",
  );
  assert.equal(
    errCode(() => db.execute("SELECT to_jsonb(ARRAY[ARRAY[1,2],ARRAY[3,4]])")),
    "0A000",
  );
});

// The json/jsonb construction builders (to_json / json[b]_build_array / _object) reuse the `to_jsonb`
// element kernel (valueToNode), so a deferred-source element (float, like to_jsonb) is `0A000`. PG
// supports these sources, so this is a documented divergence; the supported set is oracle-clean in
// suites/json/json_builders.test. Mirrors impl/rust/tests/json.rs
// json_builder_deferred_element_source_is_0a000 and impl/go/json_test.go.
test("json builder deferred element source is 0A000", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => db.execute("SELECT to_json(1.5::f64)")),
    "0A000",
  );
  assert.equal(
    errCode(() => db.execute("SELECT jsonb_build_array(1.5::f64)")),
    "0A000",
  );
  assert.equal(
    errCode(() => db.execute("SELECT json_build_array(1.5::f64)")),
    "0A000",
  );
  assert.equal(
    errCode(() => db.execute("SELECT jsonb_build_object('k', 1.5::f64)")),
    "0A000",
  );
});

// A non-scalar `json[b]_build_object` KEY (e.g. a date) is a deferred `0A000` — only text / integer /
// decimal / boolean keys coerce to text this slice. PostgreSQL renders any type's text output as the
// key, so this is a documented divergence (the text/int/bool key coercions are oracle-clean in the
// suite). Mirrors impl/rust/tests/json.rs json_build_object_non_scalar_key_is_0a000 and
// impl/go/json_test.go.
test("json_build_object non-scalar key is 0A000", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => db.execute("SELECT jsonb_build_object('2020-01-01'::date, 1)")),
    "0A000",
  );
});

// `json[b]_agg` over a deferred-source value (float, like to_jsonb) is `0A000` — the aggregate
// reuses the `to_jsonb` element kernel, so the same float/datetime/composite/uuid/bytea/interval
// sources propagate the deferral (json-sql-functions.md §4, B4). The supported element types are
// oracle-clean in suites/json/json_agg.test. Mirrors impl/rust/tests/json.rs
// json_agg_deferred_element_source_is_0a000 and impl/go/json_test.go.
test("json_agg deferred element source is 0A000", () => {
  const db = dbWith([
    "CREATE TABLE f (id i32 PRIMARY KEY, x f64)",
    "INSERT INTO f VALUES (1, 1.5)",
  ]);
  assert.equal(
    errCode(() => db.execute("SELECT jsonb_agg(x) FROM f")),
    "0A000",
  );
  assert.equal(
    errCode(() => db.execute("SELECT json_agg(x) FROM f")),
    "0A000",
  );
});

// `json[b]_object_agg` over a deferred-source VALUE (float, like to_jsonb) is `0A000` — the value
// conversion reuses the to_jsonb element kernel, so the same float/datetime/composite/uuid/bytea/
// interval sources propagate the deferral (json-sql-functions.md §4, B4). PG supports it, so this is
// a documented divergence; the supported value types are oracle-clean in
// suites/json/json_object_agg.test. Mirrors impl/rust/tests/json.rs
// json_object_agg_deferred_value_source_is_0a000 and impl/go/json_test.go.
test("json_object_agg deferred value source is 0A000", () => {
  const db = dbWith([
    "CREATE TABLE f (id i32 PRIMARY KEY, k text, x f64)",
    "INSERT INTO f VALUES (1, 'a', 1.5)",
  ]);
  assert.equal(
    errCode(() => db.execute("SELECT jsonb_object_agg(k, x) FROM f")),
    "0A000",
  );
  assert.equal(
    errCode(() => db.execute("SELECT json_object_agg(k, x) FROM f")),
    "0A000",
  );
});

// `json_agg` over a `json` element CANONICALIZES it (the element conversion runs through the jsonb
// node tree), dropping the input whitespace — a documented divergence from PostgreSQL, which
// preserves the verbatim sub-text (`[{ "a" : 1 }]`). This is the same verbatim divergence the json
// SRFs / accessor operators carry (json.md §4); it can't live in the PG-clean corpus. Mirrors
// impl/rust/tests/json.rs json_agg_canonicalizes_json_elements and impl/go/json_test.go.
test("json_agg canonicalizes json elements", () => {
  const db = dbWith([
    "CREATE TABLE j (id i32 PRIMARY KEY, doc json)",
    `INSERT INTO j VALUES (1, '{ "a" : 1 }')`,
  ]);
  // jed canonicalizes the element; PG would render the verbatim `[{ "a" : 1 }]`.
  assert.deepEqual(query(db, "SELECT json_agg(doc) FROM j"), [['[{"a": 1}]']]);
});

// A NULL element inside the `jsonb_set` / `jsonb_insert` path array propagates a SQL NULL result — a
// documented divergence from PostgreSQL, which raises `22004` ("path element at position N is null").
// jed treats the path strictly, like the `#-` delete-path operator's text[] handling. The agreeing
// behavior (set/insert/no-op/22023/22P02) is oracle-clean in suites/json/json_set.test. Mirrors
// impl/rust/tests/json.rs jsonb_set_null_path_element_propagates_null and impl/go/json_test.go.
test("jsonb_set null path element propagates null", () => {
  const db = dbWith([]);
  assert.deepEqual(query(db, `SELECT jsonb_set('{"a":1}', ARRAY['a', NULL], '99')`), [["NULL"]]);
  assert.deepEqual(query(db, `SELECT jsonb_insert('{"a":1}', ARRAY[NULL], '99')`), [["NULL"]]);
});

// array_to_json of a MULTIDIMENSIONAL array is a deferred 0A000 (the to_jsonb multidim deferral) — a
// documented divergence from PostgreSQL, which renders nested arrays. The 1-D case is oracle-clean in
// suites/json/json_builders.test. Mirrors impl/rust/tests/json.rs array_to_json_multidim_is_0a000 and
// impl/go/json_test.go.
test("array_to_json multidim is 0A000", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => db.execute("SELECT array_to_json(ARRAY[ARRAY[1,2],ARRAY[3,4]])")),
    "0A000",
  );
});

// JSON_SERIALIZE over a `jsonb` value renders its canonical text — a documented divergence from
// PostgreSQL 18, which returns SQL NULL for a jsonb input (a PG quirk; only `json` input serializes).
// The json-input behavior is oracle-clean in suites/json/json_ctor.test. Mirrors
// impl/rust/tests/json.rs json_serialize_jsonb_diverges_from_pg and impl/go/json_test.go.
test("JSON_SERIALIZE of jsonb diverges from pg", () => {
  const db = dbWith([]);
  // jed: the jsonb canonical (spaced, key-sorted) text; PG 18: NULL.
  assert.deepEqual(query(db, `SELECT JSON_SERIALIZE('{"b":2,"a":1}'::jsonb)`), [
    ['{"a": 1, "b": 2}'],
  ]);
});

// JSON_SCALAR over a non-basic scalar (date / float / uuid / …) is a deferred 0A000 — only
// integer/decimal/boolean/text coerce this slice. PostgreSQL renders any scalar's text as a JSON
// string, so this is a documented divergence (the basic scalars are oracle-clean in the suite).
// Mirrors impl/rust/tests/json.rs json_scalar_deferred_types_are_0a000 and impl/go/json_test.go.
test("JSON_SCALAR of deferred types is 0A000", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => db.execute("SELECT JSON_SCALAR('2020-01-01'::date)")),
    "0A000",
  );
  assert.equal(
    errCode(() => db.execute("SELECT JSON_SCALAR(1.5::f64)")),
    "0A000",
  );
});

// A composite or array COLUMN in a record function's column-definition list (R1) is a deferred 0A000
// (only scalar / json / jsonb columns coerce this slice). PostgreSQL supports them, so this is a
// documented divergence; the scalar columns are oracle-clean in suites/json/json_record.test.
// Mirrors impl/rust/tests/json.rs json_record_composite_array_column_is_0a000.
test("json_to_record composite/array column is 0A000", () => {
  const db = dbWith(["CREATE TYPE addr AS (street text, zip i32)"]);
  assert.equal(
    errCode(() => db.execute(`SELECT * FROM jsonb_to_record('{"a":1}') AS t(a addr)`)),
    "0A000",
  );
  assert.equal(
    errCode(() => db.execute(`SELECT * FROM jsonb_to_record('{"a":1}') AS t(a i32[])`)),
    "0A000",
  );
});

// The R2 populate-record divergences the oracle corpus cannot express. A non-composite first argument
// → 42804 (PG's "first argument must be a composite type"); a composite whose FIELD is an array →
// 0A000 at row coercion (the same composite/array deferral R1 carries — only scalar / json / jsonb
// fields coerce this slice). The scalar-field cases are oracle-clean in suites/json/json_populate.test.
// Mirrors impl/rust/tests/json.rs json_populate_non_composite_and_complex_field_divergences.
test("json_populate non-composite base and array field divergences", () => {
  const db = dbWith([
    "CREATE TYPE addr AS (street text, zip i32)",
    "CREATE TYPE poly AS (name text, pts i32[])",
  ]);
  assert.equal(
    errCode(() => db.execute(`SELECT * FROM jsonb_populate_record(NULL::i32, '{"a":1}')`)),
    "42804",
  );
  assert.equal(
    errCode(() =>
      db.execute(`SELECT * FROM jsonb_populate_record(NULL::poly, '{"name":"x","pts":[1,2]}')`),
    ),
    "0A000",
  );
});

// A rename-only column-alias list `AS g(col)` (no types) on a table function is a deferred 0A000
// (only the TYPED column-definition list `AS t(col type, …)` — C0 — is parsed). PostgreSQL accepts a
// rename list on an SRF, so this is a documented divergence. Mirrors
// impl/rust/tests/json.rs srf_rename_only_column_list_is_deferred.
test("srf rename-only column list is deferred", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => db.execute(`SELECT * FROM jsonb_to_recordset('[{"a":1}]') AS t(a, b)`)),
    "0A000",
  );
});

// The deferred S2 sub-clauses of the SQL/JSON query functions are 0A000 — PASSING (path vars), ON
// ERROR/EMPTY DEFAULT expr, JSON_QUERY OMIT QUOTES, and JSON_QUERY RETURNING a non-json type.
// PostgreSQL supports all of these, so each is a documented divergence; the supported subset is
// oracle-clean in suites/json/json_query_fns.test. Mirrors impl/rust/tests/json.rs
// json_query_fn_deferred_clauses_are_0a000.
test("json query function deferred clauses are 0A000", () => {
  const db = dbWith([]);
  for (const sql of [
    `SELECT JSON_VALUE('{"a":1}', '$.a' PASSING 1 AS x)`,
    `SELECT JSON_VALUE('{"a":1}', '$.b' DEFAULT 'z' ON EMPTY)`,
    `SELECT JSON_QUERY('{"a":1}', '$.a' OMIT QUOTES)`,
    `SELECT JSON_QUERY('{"a":1}', '$.a' RETURNING int)`,
  ]) {
    assert.equal(
      errCode(() => db.execute(sql)),
      "0A000",
      `${sql} should defer 0A000`,
    );
  }
});

// The deferred T1 sub-features of JSON_TABLE are 0A000 — an explicit PLAN, PASSING, an array column,
// a WRAPPER on a scalar column, OMIT QUOTES; an unknown column type is 42704. PostgreSQL supports the
// first set, so each is a documented divergence; the supported subset is oracle-clean in
// suites/json/json_table.test. Mirrors impl/rust/tests/json.rs json_table_deferred_features_are_0a000.
test("json_table deferred features are 0A000", () => {
  const db = dbWith([]);
  for (const sql of [
    `SELECT * FROM JSON_TABLE('{}', '$' COLUMNS (x i32 PATH '$.x') PLAN DEFAULT (x))`,
    `SELECT * FROM JSON_TABLE('{}', '$' PASSING 1 AS y COLUMNS (x i32 PATH '$.x'))`,
    `SELECT * FROM JSON_TABLE('{}', '$' COLUMNS (x i32[] PATH '$.x'))`,
    `SELECT * FROM JSON_TABLE('{}', '$' COLUMNS (x i32 PATH '$.x' WITH WRAPPER))`,
    `SELECT * FROM JSON_TABLE('{}', '$' COLUMNS (x i32 PATH '$.x' OMIT QUOTES))`,
  ]) {
    assert.equal(
      errCode(() => db.execute(sql)),
      "0A000",
      `${sql} should defer 0A000`,
    );
  }
  assert.equal(
    errCode(() =>
      db.execute(`SELECT * FROM JSON_TABLE('{}', '$' COLUMNS (x nosuchtype PATH '$.x'))`),
    ),
    "42704",
  );
});
