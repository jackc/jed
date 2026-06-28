// The `jsonpath` type (spec/design/jsonpath.md, slice P1a) — the per-core checks the conformance
// corpus cannot express (CLAUDE.md §10): the deliberate PG divergences (the deferred P1b constructs
// are 0A000, where PG compiles them; a jsonpath is non-comparable / a jsonpath column is 0A000).
// The agreeing behavior (the canonical render, malformed → 42601) is oracle-clean in
// suites/json/jsonpath_literal.test. Mirrors impl/rust/tests/jsonpath.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { execute } from "../src/tooling.ts";
import { dbWith, errCode } from "./util.ts";

// The still-deferred path-expression constructs — item methods .m(), arithmetic, like_regex /
// starts with, $name variables — are a 0A000 at compile (P1b added filters ?(comparison) AND
// top-level predicates `$.a == 1`, but not these). PostgreSQL compiles them, so each is a documented
// divergence; the supported subset is oracle-clean in suites/json/jsonpath_literal.test and
// jsonpath_query.test.
test("jsonpath P1b constructs are 0A000", () => {
  const db = dbWith([]);
  for (const path of [
    "$.a.size()", // item method
    "$.a + 2", // arithmetic
    '$.a like_regex "x"', // a like_regex top-level predicate
    '$.a starts with "x"', // a starts-with top-level predicate
    "$[$x]", // a non-literal subscript expression
    "$x", // a path variable
  ]) {
    assert.equal(
      errCode(() => execute(db, `SELECT '${path}'::jsonpath`)),
      "0A000",
      `path \`${path}\` should defer 0A000`,
    );
  }
});

// A jsonpath using a STILL-deferred construct (an item method, like_regex, arithmetic) is 0A000 — it
// fails to compile. Filters ?(comparison) and top-level predicates (`$.a == 1`, for jsonb_path_match
// / @@) now compile (P1b), but item methods / like_regex / arithmetic are a follow-on. PostgreSQL
// evaluates all of these, so each is a documented divergence; the supported filter + query + match
// behavior is oracle-clean in suites/json/jsonpath_query.test.
test("jsonpath deferred constructs are 0A000", () => {
  const db = dbWith([]);
  // An item method.
  assert.equal(
    errCode(() => execute(db, "SELECT jsonb_path_query_array('[1,2,3]', '$[*].double()')")),
    "0A000",
  );
  // like_regex inside a filter (a non-comparison predicate).
  assert.equal(
    errCode(() => execute(db, `SELECT jsonb_path_exists('["x"]', '$[*] ? (@ like_regex "x")')`)),
    "0A000",
  );
  // like_regex as a top-level predicate.
  assert.equal(
    errCode(() => execute(db, `SELECT jsonb_path_match('["x"]', '$ like_regex "x"')`)),
    "0A000",
  );
});

// A `jsonpath` value is NOT comparable — every comparison / ORDER BY is 42883 (PG ships no opclass).
// A documented contract (jsonpath.md §1); only IS [NOT] NULL applies.
test("jsonpath is not comparable", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => execute(db, "SELECT '$.a'::jsonpath = '$.a'::jsonpath")),
    "42883",
  );
  assert.equal(
    errCode(() => execute(db, "SELECT '$.a'::jsonpath < '$.b'::jsonpath")),
    "42883",
  );
});

// A `jsonpath` COLUMN is 0A000 — jsonpath is literal-only this slice (P1a, like a J0-stage json
// column). PostgreSQL allows a jsonpath column, so this is a documented divergence.
test("jsonpath column is unsupported", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => execute(db, "CREATE TABLE t (p jsonpath)")),
    "0A000",
  );
});

// A malformed jsonpath literal is 42601 (PG's syntax-error class), distinct from the 0A000 of a
// valid-but-unsupported construct. (The agreeing 42601 cases live in the corpus; this pins the
// distinction against the 0A000 ones above.)
test("malformed jsonpath is 42601", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => execute(db, "SELECT '$.'::jsonpath")),
    "42601",
  );
  assert.equal(
    errCode(() => execute(db, "SELECT '$['::jsonpath")),
    "42601",
  );
  assert.equal(
    errCode(() => execute(db, "SELECT '$[1 to'::jsonpath")),
    "42601",
  );
});
