// The `jsonpath` type (spec/design/jsonpath.md) — the per-core checks the conformance corpus cannot
// express (CLAUDE.md §10): the deliberate PG divergences (the deferred constructs are 0A000, where PG
// compiles them; a jsonpath is non-comparable / a jsonpath column is 0A000; an item method reads a
// string with jed's own number grammar). The agreeing behavior (the canonical render, malformed →
// 42601, the item methods) is oracle-clean in suites/json/jsonpath_literal.test and
// jsonpath_methods.test. Mirrors impl/rust/tests/jsonpath.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { dbWith, errCode } from "./util.ts";

// The still-deferred path-expression constructs — the datetime item methods, arithmetic, like_regex /
// starts with, $name variables — are a 0A000 at compile. PostgreSQL compiles them, so each is a
// documented divergence; the supported subset is oracle-clean in suites/json/jsonpath_literal.test,
// jsonpath_query.test, and jsonpath_methods.test.
test("jsonpath P1b constructs are 0A000", () => {
  const db = dbWith([]);
  for (const path of [
    "$.a.datetime()", // a datetime item method
    '$.a.datetime("HH24")',
    "$.a.date()",
    "$.a.time(3)",
    "$.a.time_tz()",
    "$.a.timestamp()",
    "$.a.timestamp_tz(2)",
    "$.a + 2", // arithmetic
    '$.a like_regex "x"', // a like_regex top-level predicate
    '$.a starts with "x"', // a starts-with top-level predicate
    "$[$x]", // a non-literal subscript expression
    "$x", // a path variable
  ]) {
    assert.equal(
      errCode(() => db.execute(`SELECT '${path}'::jsonpath`)),
      "0A000",
      `path \`${path}\` should defer 0A000`,
    );
  }
});

// A jsonpath using a STILL-deferred construct (a datetime method, like_regex) is 0A000 — it fails to
// compile. PostgreSQL evaluates these, so each is a documented divergence; the supported filter +
// query + match + method behavior is oracle-clean in suites/json/jsonpath_query.test and
// jsonpath_methods.test.
test("jsonpath deferred constructs are 0A000", () => {
  const db = dbWith([]);
  // A datetime item method.
  assert.equal(
    errCode(() => db.execute(`SELECT jsonb_path_query_array('["2020-01-01"]', '$[*].datetime()')`)),
    "0A000",
  );
  // like_regex inside a filter (a non-comparison predicate).
  assert.equal(
    errCode(() => db.execute(`SELECT jsonb_path_exists('["x"]', '$[*] ? (@ like_regex "x")')`)),
    "0A000",
  );
  // like_regex as a top-level predicate.
  assert.equal(
    errCode(() => db.execute(`SELECT jsonb_path_match('["x"]', '$ like_regex "x"')`)),
    "0A000",
  );
});

// A `jsonpath` value is NOT comparable — every comparison / ORDER BY is 42883 (PG ships no opclass).
// A documented contract (jsonpath.md §1); only IS [NOT] NULL applies.
test("jsonpath is not comparable", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => db.execute("SELECT '$.a'::jsonpath = '$.a'::jsonpath")),
    "42883",
  );
  assert.equal(
    errCode(() => db.execute("SELECT '$.a'::jsonpath < '$.b'::jsonpath")),
    "42883",
  );
});

// A `jsonpath` COLUMN is 0A000 — jsonpath is literal-only this slice (P1a, like a J0-stage json
// column). PostgreSQL allows a jsonpath column, so this is a documented divergence.
test("jsonpath column is unsupported", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => db.execute("CREATE TABLE t (p jsonpath)")),
    "0A000",
  );
});

// A malformed jsonpath literal is 42601 (PG's syntax-error class), distinct from the 0A000 of a
// valid-but-unsupported construct. (The agreeing 42601 cases live in the corpus; this pins the
// distinction against the 0A000 ones above.)
test("malformed jsonpath is 42601", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => db.execute("SELECT '$.'::jsonpath")),
    "42601",
  );
  assert.equal(
    errCode(() => db.execute("SELECT '$['::jsonpath")),
    "42601",
  );
  assert.equal(
    errCode(() => db.execute("SELECT '$[1 to'::jsonpath")),
    "42601",
  );
});

// An item method reads a string with jed's OWN number grammars (grammar.md §36): decimal digits only,
// no hex/octal/binary prefixes and no digit underscores, so those strings are 22036 where PostgreSQL
// 16+ (int8in / numeric_in, and glibc strtod behind float8in) accepts them.
test("jsonpath method number grammar divergence", () => {
  const db = dbWith([]);
  for (const [doc, path] of [
    ['"0x10"', "$.bigint()"],
    ['"1_000"', "$.bigint()"],
    ['"0o17"', "$.integer()"],
    ['"0x1A"', "$.number()"],
    ['"1_0"', "$.decimal()"],
    ['"0x10"', "$.double()"],
  ]) {
    assert.equal(
      errCode(() => db.execute(`SELECT jsonb_path_query_first('${doc}', '${path}')`)),
      "22036",
      `${doc} ${path}`,
    );
  }
});
