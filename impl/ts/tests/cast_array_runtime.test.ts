// The three array-involving casts — the parts the PG-clean oracle corpus cannot express (the array
// cast follow-ons; spec/design/array.md §7, spec/types/casts.toml). The numeric/text element pairs
// AGREE with PostgreSQL and are oracle-checked in suites/cast/array_casts.test (run on every core);
// this file covers only what that corpus cannot: (a) array → text is EXPLICIT-only (an assignment /
// implicit context stays 42804); (b) the jed-only element casts uuid⇄bytea (succeeding where PG
// errors); (c) the forbidden scalar element pair → 42804 and a composite-element array cast → 0A000;
// (d) runtime text → f32[]/f64[] (the float renderer is in the determinism-exception ledger).
// Mirrors impl/rust/tests/cast_array_runtime.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { execute } from "../src/tooling.ts";
import { dbWith, errCode, query } from "./util.ts";

const scalar = (db: ReturnType<typeof dbWith>, expr: string): string =>
  query(db, `SELECT ${expr}`)[0][0];

// --- (a) array → text is EXPLICIT-only -----------------------------------------------------------

test("array → text is explicit-only (assignment / implicit context stays 42804)", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, label text)"]);
  // Assignment context: an array value into a text column is a datatype mismatch, NOT a silent
  // array_out (PG would assignment-cast it).
  assert.equal(
    errCode(() => execute(db, "INSERT INTO t VALUES (1, ARRAY[1,2,3])")),
    "42804",
  );
  execute(db, "INSERT INTO t VALUES (1, '{1,2,3}')");
  // Implicit context: comparing a text column to an array value is a mismatch.
  assert.equal(
    errCode(() => execute(db, "SELECT id FROM t WHERE label = ARRAY[1,2,3]")),
    "42804",
  );
  // The explicit cast, by contrast, succeeds.
  assert.equal(scalar(db, "(ARRAY[1,2,3])::text"), "{1,2,3}");
});

// --- (b) the jed-only element casts uuid ⇄ bytea (succeed where PG errors) ------------------------

test("uuid[] → bytea[] → uuid[] round-trips; wrong-width bytea[] → uuid[] traps 22P02", () => {
  const db = dbWith([]);
  assert.equal(
    scalar(
      db,
      "((ARRAY['00000000-0000-0000-0000-000000000001']::uuid[])::bytea[])::uuid[] = " +
        "ARRAY['00000000-0000-0000-0000-000000000001']::uuid[]",
    ),
    "true",
  );
  assert.equal(
    errCode(() => execute(db, "SELECT (ARRAY['\\x00'::bytea])::uuid[]")),
    "22P02",
  );
});

// --- (c) forbidden scalar element pair (42804) + composite-element array cast (0A000) -------------

test("forbidden array element pairs: 42804 (no scalar cast) / 0A000 (composite element / param)", () => {
  const db = dbWith(["CREATE TYPE addr AS (street text, zip i32)"]);
  // A scalar element pair with no cast → 42804 (PG reports 42846). i32 → timestamp has no cast.
  assert.equal(
    errCode(() => execute(db, "SELECT (ARRAY[1,2,3]::i32[])::timestamp[]")),
    "42804",
  );
  // A composite-element array cast is the deferred composite cast surface → 0A000.
  assert.equal(
    errCode(() => execute(db, "SELECT (ARRAY[ROW('Main',90210)::addr]::addr[])::text[]")),
    "0A000",
  );
  // A bind parameter into an array type stays the container-param narrowing (0A000).
  assert.equal(
    errCode(() => execute(db, "SELECT $1::i32[]")),
    "0A000",
  );
});

// --- (d) runtime text → f32[] / f64[] element casts (float renderer is determinism-exempt) -------

test("runtime text → f32[] / f64[] element casts", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, s text)",
    "INSERT INTO t VALUES (1, '{0.5,0.25,-1.5}')",
  ]);
  assert.equal(
    query(db, "SELECT (s::float8[])::text FROM t WHERE id = 1")[0][0],
    "{0.5,0.25,-1.5}",
  );
  // text → f32[] then widen to f64[] (0.5/0.25 are exact in binary32).
  assert.equal(
    scalar(db, "(((ARRAY['0.5','0.25']::text[])::float4[])::float8[])::text"),
    "{0.5,0.25}",
  );
  // i32[] → f64[] element-wise (numeric → float).
  assert.equal(scalar(db, "((ARRAY[1,2,3]::i32[])::float8[])::text"), "{1,2,3}");
});
