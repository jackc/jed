// Array as a KEY — the parts the PG-clean oracle corpus cannot express (encoding.md §2.14, the
// array-elements-terminated rule). The 1-D PRIMARY KEY surface agrees with PostgreSQL and is
// oracle-checked in types/array_key.test; this file covers only what that corpus cannot:
//   (a) the MULTIDIM / CUSTOM-LOWER-BOUND key tiebreak, where jed's consistent array_cmp order
//       deliberately differs from PostgreSQL's single-column ORDER BY (an abbreviated-key artifact);
//   (b) the keyable-element gate — a float-element array PRIMARY KEY IS keyable (the §2.8 lift —
//       f64[]/f32[]); a composite-element array key is still rejected 0A000 (composite not yet keyable).
// Mirrors impl/rust/tests/array_key.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database } from "../src/tooling.ts";
import { dbWith, errCode, query } from "./util.ts";

// (a) jed's array_cmp PK order for multidim / custom-lower-bound values (diverges from PG's ORDER BY).
test("multidim / custom-lower-bound array key order (jed's array_cmp, not PG's ORDER BY)", () => {
  const db = dbWith(["CREATE TABLE m (k i32[] PRIMARY KEY)"]);
  for (const v of ["{1,2,3,4}", "{{1,2},{3,4}}", "{1,2,3}", "[2:4]={1,2,3}"]) {
    db.execute(`INSERT INTO m VALUES ('${v}')`);
  }
  const got = query(db, "SELECT k FROM m ORDER BY k").map((r) => r[0]);
  assert.deepEqual(got, ["{1,2,3}", "[2:4]={1,2,3}", "{1,2,3,4}", "{{1,2},{3,4}}"]);
});

// (b) float-element arrays ARE keyable; the order recurses into the float-order-preserving element
// key (float total order: -0=+0, NaN largest, shorter-prefix first). The '{…}' literal coerces the
// specials (NaN/Infinity) without an INSERT ... SELECT (0A000 into an array column this slice).
test("float-element array key is keyable", () => {
  const db = dbWith(["CREATE TABLE m (k f64[] PRIMARY KEY)"]);
  for (const v of ["{1.5,2.5}", "{1.5}", "{-Infinity}", "{NaN}", "{1.5,2.0}"]) {
    db.execute(`INSERT INTO m VALUES ('${v}')`);
  }
  const got = query(db, "SELECT k FROM m ORDER BY k").map((r) => r[0]);
  assert.deepEqual(got, ["{-Infinity}", "{1.5}", "{1.5,2}", "{1.5,2.5}", "{NaN}"]);
});

// the multidim / lower-bound float-element key tiebreak (jed's array_cmp, NOT PG's ORDER BY).
test("float-element array multidim key order (jed's array_cmp)", () => {
  const db = dbWith(["CREATE TABLE m (k f64[] PRIMARY KEY)"]);
  for (const v of [
    "{1.5,2.5,3.5,4.5}",
    "{{1.5,2.5},{3.5,4.5}}",
    "{1.5,2.5,3.5}",
    "[2:4]={1.5,2.5,3.5}",
  ]) {
    db.execute(`INSERT INTO m VALUES ('${v}')`);
  }
  const got = query(db, "SELECT k FROM m ORDER BY k").map((r) => r[0]);
  assert.deepEqual(got, [
    "{1.5,2.5,3.5}",
    "[2:4]={1.5,2.5,3.5}",
    "{1.5,2.5,3.5,4.5}",
    "{{1.5,2.5},{3.5,4.5}}",
  ]);
});

// (c) a composite-element array key is still 0A000 (composite not yet keyable); float-element arrays
// are accepted everywhere a key is taken.
test("composite-element array keys are rejected (0A000); float-element arrays accepted", () => {
  const db = dbWith([]);
  db.execute("CREATE TYPE addr AS (street text, zip i32)");
  assert.equal(
    errCode(() => db.execute("CREATE TABLE bad (k addr[] PRIMARY KEY)")),
    "0A000",
  );
  db.execute("CREATE TABLE ok (id i32 PRIMARY KEY, k f32[] UNIQUE)");
  db.execute("CREATE TABLE ok2 (id i32 PRIMARY KEY, k f64[])");
  db.execute("CREATE INDEX ix ON ok2 (k)");
});
