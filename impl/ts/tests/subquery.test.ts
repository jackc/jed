// Uncorrelated subqueries — scalar `(SELECT …)`, `x [NOT] IN (SELECT …)`, `[NOT] EXISTS
// (SELECT …)`. These complement the conformance corpus (spec/conformance/suites/subquery) with
// finer-grained per-feature assertions: plan-time folding (execute once → constant), the typed
// NULL of an empty scalar, three-valued IN, EXISTS ignoring the select list, the cost contract
// (subquery cost added once, the fold is a leaf), and the error / narrowing codes (21000 / 42601 /
// 0A000). See spec/design/grammar.md §26.

import assert from "node:assert/strict";
import { test } from "node:test";
import { execute } from "../src/lib.ts";
import { dbWith, errCode, query } from "./util.ts";

function ab() {
  return dbWith([
    "CREATE TABLE a (id int32 PRIMARY KEY, k int32)",
    "CREATE TABLE b (id int32 PRIMARY KEY, k int32)",
    "CREATE TABLE one (id int32 PRIMARY KEY)",
    "INSERT INTO a VALUES (1, 10), (2, 20), (3, 30)",
    "INSERT INTO b VALUES (1, 20), (2, 30), (3, 40)",
    "INSERT INTO one VALUES (1)",
  ]);
}

test("scalar subquery in WHERE and in the select list", () => {
  assert.deepStrictEqual(query(ab(), "SELECT id FROM a WHERE k = (SELECT max(k) FROM a) ORDER BY id"), [["3"]]);
  assert.deepStrictEqual(query(ab(), "SELECT (SELECT count(*) FROM b) FROM a ORDER BY id"), [["3"], ["3"], ["3"]]);
});

test("scalar subquery nested and inside a larger expression", () => {
  assert.deepStrictEqual(
    query(ab(), "SELECT (SELECT (SELECT max(k) FROM b) FROM one) FROM one"),
    [["40"]],
  );
  assert.deepStrictEqual(query(ab(), "SELECT k + (SELECT max(k) FROM b) FROM a ORDER BY id"), [["50"], ["60"], ["70"]]);
});

test("empty scalar subquery is NULL", () => {
  assert.deepStrictEqual(query(ab(), "SELECT (SELECT k FROM b WHERE id = 99) FROM one"), [["NULL"]]);
  assert.deepStrictEqual(query(ab(), "SELECT id FROM a WHERE k = (SELECT k FROM b WHERE id = 99) ORDER BY id"), []);
});

test("IN / NOT IN subquery", () => {
  assert.deepStrictEqual(query(ab(), "SELECT id FROM a WHERE k IN (SELECT k FROM b) ORDER BY id"), [["2"], ["3"]]);
  assert.deepStrictEqual(query(ab(), "SELECT id FROM a WHERE k NOT IN (SELECT k FROM b) ORDER BY id"), [["1"]]);
});

test("IN over an empty subquery is FALSE, NOT IN is TRUE", () => {
  assert.deepStrictEqual(query(ab(), "SELECT id FROM a WHERE k IN (SELECT k FROM b WHERE id = 99) ORDER BY id"), []);
  assert.deepStrictEqual(query(ab(), "SELECT id FROM a WHERE k NOT IN (SELECT k FROM b WHERE id = 99) ORDER BY id"), [
    ["1"],
    ["2"],
    ["3"],
  ]);
});

test("IN with a NULL in the result is three-valued", () => {
  const db = dbWith([
    "CREATE TABLE s (id int32 PRIMARY KEY, k int32)",
    "CREATE TABLE vals (id int32 PRIMARY KEY, v int32)",
    "INSERT INTO s VALUES (1, 5), (2, 10)",
    "INSERT INTO vals VALUES (1, 10), (2, NULL)",
  ]);
  // 10 matches -> TRUE (id 2). 5 matches nothing but the NULL makes it UNKNOWN -> dropped.
  assert.deepStrictEqual(query(db, "SELECT id FROM s WHERE k IN (SELECT v FROM vals) ORDER BY id"), [["2"]]);
  assert.deepStrictEqual(query(db, "SELECT id FROM s WHERE k NOT IN (SELECT v FROM vals) ORDER BY id"), []);
});

test("EXISTS / NOT EXISTS, and EXISTS ignores the select list", () => {
  assert.deepStrictEqual(query(ab(), "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b) ORDER BY id"), [["1"], ["2"], ["3"]]);
  assert.deepStrictEqual(query(ab(), "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE id = 99) ORDER BY id"), []);
  assert.deepStrictEqual(query(ab(), "SELECT id FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE id = 99) ORDER BY id"), [
    ["1"],
    ["2"],
    ["3"],
  ]);
  assert.deepStrictEqual(query(ab(), "SELECT id FROM a WHERE EXISTS (SELECT 1, 2, 3 FROM b) ORDER BY id"), [
    ["1"],
    ["2"],
    ["3"],
  ]);
  assert.deepStrictEqual(query(ab(), "SELECT id FROM a WHERE EXISTS (SELECT * FROM b) ORDER BY id"), [["1"], ["2"], ["3"]]);
});

test("a subquery's cost is added once, the folded constant a leaf", () => {
  const db = ab();
  const base = execute(db, "SELECT id FROM a WHERE k = 999").cost;
  const withSub = execute(db, "SELECT id FROM a WHERE k = (SELECT max(k) FROM b)").cost;
  // The folded constant is a leaf, so the only delta is the subquery's own cost (3 scan + 3
  // accumulate + 1 produced = 7), added exactly once.
  assert.strictEqual(withSub - base, 7n);
});

test("subquery error codes and narrowings", () => {
  const cases: [string, string][] = [
    ["SELECT (SELECT k FROM b) FROM one", "21000"],
    ["SELECT (SELECT id, k FROM b WHERE id = 1) FROM one", "42601"],
    ["SELECT id FROM a WHERE k IN (SELECT id, k FROM b)", "42601"],
    ["SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE k = a.k)", "0A000"],
    ["SELECT (SELECT max(k) FROM b WHERE b.id = a.id) FROM a", "0A000"],
    ["SELECT id FROM a WHERE k = (SELECT $1 FROM b LIMIT 1)", "0A000"],
    ["DELETE FROM a WHERE k IN (SELECT k FROM b)", "0A000"],
  ];
  for (const [sql, code] of cases) {
    assert.strictEqual(
      errCode(() => execute(ab(), sql)),
      code,
      sql,
    );
  }
});
