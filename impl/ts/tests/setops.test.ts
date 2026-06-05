// Set operations — UNION/INTERSECT/EXCEPT (each [ALL]). These complement the conformance corpus
// (spec/conformance/suites/setops) with finer-grained per-feature assertions: PG precedence,
// multiset multiplicities, integer<->decimal unification, the lhs+rhs cost contract, and the
// error codes. See spec/design/grammar.md §25.

import assert from "node:assert/strict";
import { test } from "node:test";
import { execute } from "../src/lib.ts";
import { dbWith, errCode, query } from "./util.ts";

function ab() {
  return dbWith([
    "CREATE TABLE a (id int32 PRIMARY KEY, k int32)",
    "CREATE TABLE b (id int32 PRIMARY KEY, k int32)",
    "INSERT INTO a VALUES (1, 10), (2, 20), (3, 30)",
    "INSERT INTO b VALUES (1, 20), (2, 30), (3, 40)",
  ]);
}

test("UNION deduplicates, UNION ALL keeps multiplicities", () => {
  assert.deepStrictEqual(query(ab(), "SELECT k FROM a UNION SELECT k FROM b ORDER BY k"), [
    ["10"],
    ["20"],
    ["30"],
    ["40"],
  ]);
  assert.deepStrictEqual(query(ab(), "SELECT k FROM a UNION ALL SELECT k FROM b ORDER BY k"), [
    ["10"],
    ["20"],
    ["20"],
    ["30"],
    ["30"],
    ["40"],
  ]);
});

test("set-op cost is lhs+rhs; the window is unmetered", () => {
  // 2*3 + 2*3; dedup unmetered.
  assert.strictEqual(execute(ab(), "SELECT k FROM a UNION SELECT k FROM b").cost, 12n);
  // LIMIT does not lower the cost: operands fully produce, the window is unmetered.
  assert.strictEqual(
    execute(ab(), "SELECT k FROM a UNION SELECT k FROM b ORDER BY k LIMIT 1").cost,
    12n,
  );
});

test("INTERSECT/EXCEPT multiset multiplicities (ALL and distinct)", () => {
  const db = dbWith([
    "CREATE TABLE l (id int32 PRIMARY KEY, k int32)",
    "CREATE TABLE r (id int32 PRIMARY KEY, k int32)",
    "INSERT INTO l VALUES (1,1),(2,1),(3,1),(4,2),(5,3)", // 1->3, 2->1, 3->1
    "INSERT INTO r VALUES (1,1),(2,2)", // 1->1, 2->1
  ]);
  // min(m,n): 1->1, 2->1, 3->0
  assert.deepStrictEqual(query(db, "SELECT k FROM l INTERSECT ALL SELECT k FROM r ORDER BY k"), [
    ["1"],
    ["2"],
  ]);
  assert.deepStrictEqual(query(db, "SELECT k FROM l INTERSECT SELECT k FROM r ORDER BY k"), [
    ["1"],
    ["2"],
  ]);
  // max(0,m-n): 1->2, 2->0, 3->1
  assert.deepStrictEqual(query(db, "SELECT k FROM l EXCEPT ALL SELECT k FROM r ORDER BY k"), [
    ["1"],
    ["1"],
    ["3"],
  ]);
  assert.deepStrictEqual(query(db, "SELECT k FROM l EXCEPT SELECT k FROM r ORDER BY k"), [["3"]]);
});

test("INTERSECT binds tighter than UNION (PostgreSQL precedence)", () => {
  const db = dbWith([
    "CREATE TABLE p (id int32 PRIMARY KEY, k int32)",
    "CREATE TABLE q (id int32 PRIMARY KEY, k int32)",
    "CREATE TABLE s (id int32 PRIMARY KEY, k int32)",
    "INSERT INTO p VALUES (1, 1)",
    "INSERT INTO q VALUES (1, 2), (2, 3)",
    "INSERT INTO s VALUES (1, 3), (2, 4)",
  ]);
  // p UNION q INTERSECT s = p UNION (q INTERSECT s) = {1} UNION {3} = {1,3}.
  assert.deepStrictEqual(
    query(db, "SELECT k FROM p UNION SELECT k FROM q INTERSECT SELECT k FROM s ORDER BY k"),
    [["1"], ["3"]],
  );
});

test("integer<->decimal unify: 5 matches 5.00, output renders as decimal", () => {
  const db = dbWith([
    "CREATE TABLE ai (id int32 PRIMARY KEY, n int32)",
    "CREATE TABLE ad (id int32 PRIMARY KEY, n decimal(10,2))",
    "INSERT INTO ai VALUES (1, 5), (2, 7)",
    "INSERT INTO ad VALUES (1, 5.0), (2, 9.50)",
  ]);
  // 5 (converted) == 5.00 -> matched; distinct {5, 7, 9.50}, the int rendered at scale 0.
  assert.deepStrictEqual(query(db, "SELECT n FROM ai UNION SELECT n FROM ad ORDER BY n"), [
    ["5"],
    ["7"],
    ["9.50"],
  ]);
});

test("set-op error codes", () => {
  const db = dbWith([
    "CREATE TABLE x (id int32 PRIMARY KEY, a int32, b int32)",
    "CREATE TABLE y (id int32 PRIMARY KEY, a int32, t text)",
    "INSERT INTO x VALUES (1, 10, 20)",
    "INSERT INTO y VALUES (1, 30, 'hi')",
  ]);
  assert.strictEqual(errCode(() => execute(db, "SELECT a, b FROM x UNION SELECT a FROM y")), "42601");
  assert.strictEqual(errCode(() => execute(db, "SELECT a FROM x UNION SELECT t FROM y")), "42804");
  assert.strictEqual(
    errCode(() => execute(db, "SELECT a FROM x ORDER BY a UNION SELECT a FROM y")),
    "42601",
  );
  assert.strictEqual(
    errCode(() => execute(db, "SELECT a FROM x UNION SELECT a FROM y ORDER BY x.a")),
    "42P01",
  );
  assert.strictEqual(
    errCode(() => execute(db, "SELECT a FROM x UNION SELECT a FROM y ORDER BY nope")),
    "42703",
  );
});
