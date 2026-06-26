// Array as a KEY — the parts the PG-clean oracle corpus cannot express (encoding.md §2.14, the
// array-elements-terminated rule). The 1-D PRIMARY KEY surface agrees with PostgreSQL and is
// oracle-checked in types/array_key.test; this file covers only what that corpus cannot:
//   (a) the MULTIDIM / CUSTOM-LOWER-BOUND key tiebreak, where jed's consistent array_cmp order
//       deliberately differs from PostgreSQL's single-column ORDER BY (an abbreviated-key artifact);
//   (b) the keyable-element gate — a float-element or composite-element array PRIMARY KEY is rejected
//       0A000, where PostgreSQL allows it.
// Mirrors impl/rust/tests/array_key.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { execute } from "../src/lib.ts";
import { dbWith, errCode, query } from "./util.ts";

// (a) jed's array_cmp PK order for multidim / custom-lower-bound values (diverges from PG's ORDER BY).
test("multidim / custom-lower-bound array key order (jed's array_cmp, not PG's ORDER BY)", () => {
  const db = dbWith(["CREATE TABLE m (k i32[] PRIMARY KEY)"]);
  for (const v of ["{1,2,3,4}", "{{1,2},{3,4}}", "{1,2,3}", "[2:4]={1,2,3}"]) {
    execute(db, `INSERT INTO m VALUES ('${v}')`);
  }
  const got = query(db, "SELECT k FROM m ORDER BY k").map((r) => r[0]);
  assert.deepEqual(got, ["{1,2,3}", "[2:4]={1,2,3}", "{1,2,3,4}", "{{1,2},{3,4}}"]);
});

// (b) the keyable-element gate: float / composite element arrays are NOT keyable (0A000).
test("float / composite element array keys are rejected (0A000)", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => execute(db, "CREATE TABLE bad (k f64[] PRIMARY KEY)")),
    "0A000",
  );
  execute(db, "CREATE TYPE addr AS (street text, zip i32)");
  assert.equal(
    errCode(() => execute(db, "CREATE TABLE bad2 (k addr[] PRIMARY KEY)")),
    "0A000",
  );
  assert.equal(
    errCode(() => execute(db, "CREATE TABLE bad3 (id i32 PRIMARY KEY, k f64[] UNIQUE)")),
    "0A000",
  );
  execute(db, "CREATE TABLE ok (id i32 PRIMARY KEY, k f64[])");
  assert.equal(
    errCode(() => execute(db, "CREATE INDEX ix ON ok (k)")),
    "0A000",
  );
});
