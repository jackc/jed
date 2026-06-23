// Storable json / jsonb columns + jsonb comparison/ordering (spec/design/json.md, slices J1/J1b/J2)
// — the per-core checks the conformance corpus cannot express (CLAUDE.md §10): the deliberate PG
// divergences. The agreeing json-non-comparable behavior (always 42883) and jsonb × jsonb ordering
// live in suites/json/json_compare.test; storage round-trips in suites/json/json_storage.test.
// Mirrors impl/rust/tests/json.rs and impl/go/json_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import { execute } from "../src/lib.ts";
import { dbWith, errCode } from "./util.ts";

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
