// boolean ⇄ i32 casts — the deliberate PostgreSQL divergences (spec/types/casts.toml,
// spec/design/types.md §9). The agreeing behavior (bool→i32, i32→bool, NULL, chains, the
// literal-adapts-to-i32 rule) is oracle-checked in suites/cast/bool_int.test and runs on every
// core; these per-core tests cover only what the oracle corpus CANNOT express (CLAUDE.md §10):
//
//   - the FORBIDDEN width pairs — PG ties the boolean↔integer cast to int4 ONLY, so bool⇄i16 and
//     bool⇄i64 are not casts. jed reports 42804 (datatype_mismatch — its standing convention for a
//     forbidden cast pair) where PG reports 42846 (cannot_coerce).
//   - the literal-beyond-i32 corner — CAST(5000000000 AS boolean) traps 22003 in jed (the literal
//     adapts to the i32 the bool cast needs and overflows it) where PG says 42846.
//
// Mirrors impl/rust/tests/cast_bool_int.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database } from "../src/tooling.ts";
import { dbWith, errCode, query } from "./util.ts";

// bool → i16 and bool → i64 are forbidden (PG has only bool → int4): jed 42804, PG 42846.
test("bool → non-i32 integer is forbidden (42804)", () => {
  const db = dbWith([]);
  for (const sql of [
    "SELECT CAST(TRUE AS i16)",
    "SELECT CAST(TRUE AS i64)",
    "SELECT CAST(FALSE AS smallint)",
    "SELECT TRUE::bigint",
  ]) {
    assert.equal(
      errCode(() => db.execute(sql)),
      "42804",
      sql,
    );
  }
});

// i16 → boolean and i64 → boolean are forbidden (PG has only int4 → bool): jed 42804, PG 42846.
// A column carries the width unambiguously (a bare literal would adapt to i32).
test("non-i32 integer → bool is forbidden (42804)", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, s i16, b i64)",
    "INSERT INTO t VALUES (1, 5, 9)",
  ]);
  for (const sql of [
    "SELECT CAST(s AS boolean) FROM t WHERE id = 1",
    "SELECT b::boolean FROM t WHERE id = 1",
  ]) {
    assert.equal(
      errCode(() => db.execute(sql)),
      "42804",
      sql,
    );
  }
});

// An integer literal operand of a boolean target adapts to i32, so a magnitude beyond i32 range
// traps 22003 (PG reports 42846 — it types the literal as int8 first). A documented divergence.
test("integer literal beyond i32 range → bool overflows (22003)", () => {
  const db = dbWith([]);
  for (const sql of ["SELECT CAST(5000000000 AS boolean)", "SELECT 5000000000::boolean"]) {
    assert.equal(
      errCode(() => db.execute(sql)),
      "22003",
      sql,
    );
  }
});

// The headline directions still work here (a quick per-core smoke check alongside the divergences;
// the exhaustive behavior is in the corpus). true→1, false→0, 0→false, nonzero→true, NULL→NULL.
test("bool ⇄ i32 round-trip smoke", () => {
  const db = dbWith([]);
  assert.deepEqual(query(db, "SELECT CAST(TRUE AS i32)"), [["1"]]);
  assert.deepEqual(query(db, "SELECT FALSE::int"), [["0"]]);
  assert.deepEqual(query(db, "SELECT CAST(0 AS boolean)"), [["false"]]);
  assert.deepEqual(query(db, "SELECT (-7)::boolean"), [["true"]]);
  assert.deepEqual(query(db, "SELECT CAST(NULL AS boolean)"), [["NULL"]]);
  assert.deepEqual(query(db, "SELECT 7::boolean::int"), [["1"]]);
});
