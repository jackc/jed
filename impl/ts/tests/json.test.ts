// Storable json / jsonb columns + jsonb comparison/ordering (spec/design/json.md, slices J1/J1b/J2)
// — the per-core checks the conformance corpus cannot express (CLAUDE.md §10): the deliberate PG
// divergences. The agreeing json-non-comparable behavior (always 42883) and jsonb × jsonb ordering
// live in suites/json/json_compare.test; storage round-trips in suites/json/json_storage.test.
// Mirrors impl/rust/tests/json.rs and impl/go/json_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import { execute } from "../src/lib.ts";
import { dbWith, errCode, query } from "./util.ts";

// A `jsonb` comparison with a NON-jsonb family is 42804 (jed's cross-family convention, like
// uuid/bytea/range) — a documented divergence from PostgreSQL, which reports 42883 (operator does
// not exist: jsonb = integer). The agreeing json-non-comparable behavior (always 42883) and
// jsonb × jsonb ordering live in suites/json/json_compare.test.
test("jsonb cross-family comparison is 42804", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, b jsonb)"]);
  // jsonb vs an integer / a real text value (not an adaptable string literal): 42804.
  assert.equal(
    errCode(() => execute(db, "SELECT id FROM t WHERE b = 5")),
    "42804",
  );
  assert.equal(
    errCode(() => execute(db, "SELECT id FROM t WHERE b = 'x'::text")),
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
    errCode(() => execute(db, "SELECT 5::jsonb")),
    "42804",
  );
  assert.equal(
    errCode(() => execute(db, "SELECT (1.5)::json")),
    "42804",
  );
  assert.equal(
    errCode(() => execute(db, "SELECT true::jsonb")),
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
    errCode(() => execute(db, "SELECT j -> 'a' FROM t")),
    "0A000",
  );
  assert.equal(
    errCode(() => execute(db, "SELECT j ->> 'a' FROM t")),
    "0A000",
  );
  assert.equal(
    errCode(() => execute(db, "SELECT j #> '{a}' FROM t")),
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
    errCode(() => execute(db, "SELECT * FROM json_array_elements('[1,2]'::json)")),
    "0A000",
  );
  assert.equal(
    errCode(() => execute(db, "SELECT * FROM json_array_elements_text('[1,2]'::json)")),
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
    errCode(() => execute(db, "SELECT to_jsonb(1.5::f64)")),
    "0A000",
  );
  assert.equal(
    errCode(() => execute(db, "SELECT to_jsonb('2020-01-01'::date)")),
    "0A000",
  );
  assert.equal(
    errCode(() => execute(db, "SELECT to_jsonb(ARRAY[ARRAY[1,2],ARRAY[3,4]])")),
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
    errCode(() => execute(db, "SELECT jsonb_agg(x) FROM f")),
    "0A000",
  );
  assert.equal(
    errCode(() => execute(db, "SELECT json_agg(x) FROM f")),
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
