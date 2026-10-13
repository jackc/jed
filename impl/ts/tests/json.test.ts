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
