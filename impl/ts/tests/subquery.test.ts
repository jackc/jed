// Subqueries — scalar `(SELECT …)`, `x [NOT] IN (SELECT …)`, `[NOT] EXISTS (SELECT …)`, both
// uncorrelated and CORRELATED. These complement the conformance corpus
// (spec/conformance/suites/subquery) with finer-grained per-feature assertions: the uncorrelated
// fold (execute once → constant, cost added once), the typed NULL of an empty scalar, three-valued
// IN, EXISTS ignoring the select list; and for correlated subqueries the scope-chain resolution,
// per-outer-row execution + cost, correlation in a JOIN ON and inside an aggregate argument,
// multi-level + skip-level (grandparent) correlation, and the error / narrowing codes
// (21000 / 42601 / 0A000). See spec/design/grammar.md §26.

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
    // the >1-column check is plan-time, so it fires even over an empty subquery result
    ["SELECT (SELECT id, k FROM b WHERE id = 99) FROM one", "42601"],
    // $N inside a subquery, and a subquery in a non-SELECT, remain 0A000 narrowings (§26)
    ["SELECT id FROM a WHERE k = (SELECT $1 FROM b LIMIT 1)", "0A000"],
    ["DELETE FROM a WHERE k IN (SELECT k FROM b)", "0A000"],
    // grouping / ordering a subquery BY an enclosing-query column -> 0A000 (degenerate)
    ["SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b GROUP BY a.k)", "0A000"],
    ["SELECT id FROM a WHERE EXISTS (SELECT k FROM b ORDER BY a.k)", "0A000"],
  ];
  for (const [sql, code] of cases) {
    assert.strictEqual(
      errCode(() => execute(ab(), sql)),
      code,
      sql,
    );
  }
});

// t123 is the 3-table fixture for the correlated-subquery tests (matches correlated.test).
function t123() {
  return dbWith([
    "CREATE TABLE t1 (id int32 PRIMARY KEY, v int32)",
    "CREATE TABLE t2 (id int32 PRIMARY KEY, v int32)",
    "CREATE TABLE t3 (id int32 PRIMARY KEY, v int32)",
    "INSERT INTO t1 VALUES (1, 10), (2, 20)",
    "INSERT INTO t2 VALUES (1, 10), (2, 30)",
    "INSERT INTO t3 VALUES (1, 10), (2, 20)",
  ]);
}

test("correlated EXISTS / NOT EXISTS", () => {
  assert.deepStrictEqual(
    query(t123(), "SELECT t1.id FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.v = t1.v) ORDER BY t1.id"),
    [["1"]],
  );
  assert.deepStrictEqual(
    query(t123(), "SELECT t1.id FROM t1 WHERE NOT EXISTS (SELECT 1 FROM t2 WHERE t2.v = t1.v) ORDER BY t1.id"),
    [["2"]],
  );
});

test("correlated scalar (count over a correlated WHERE) and empty -> NULL", () => {
  assert.deepStrictEqual(
    query(t123(), "SELECT t1.id, (SELECT count(*) FROM t2 WHERE t2.v > t1.v) FROM t1 ORDER BY t1.id"),
    [["1", "1"], ["2", "1"]],
  );
  assert.deepStrictEqual(
    query(t123(), "SELECT t1.id, (SELECT t2.v FROM t2 WHERE t2.v = t1.v * 100) FROM t1 ORDER BY t1.id"),
    [["1", "NULL"], ["2", "NULL"]],
  );
});

test("correlated IN / NOT IN", () => {
  assert.deepStrictEqual(
    query(t123(), "SELECT t1.id FROM t1 WHERE t1.v IN (SELECT t2.v FROM t2 WHERE t2.id = t1.id) ORDER BY t1.id"),
    [["1"]],
  );
  assert.deepStrictEqual(
    query(t123(), "SELECT t1.id FROM t1 WHERE t1.v NOT IN (SELECT t2.v FROM t2 WHERE t2.id = t1.id) ORDER BY t1.id"),
    [["2"]],
  );
});

test("correlation in a nested JOIN ON", () => {
  // the inner self-join's ON predicate references the OUTER t1.
  assert.deepStrictEqual(
    query(t123(), "SELECT t1.id FROM t1 WHERE EXISTS (SELECT 1 FROM t2 JOIN t2 AS t2b ON t2b.v = t1.v WHERE t2.id = t1.id) ORDER BY t1.id"),
    [["1"]],
  );
});

test("multi-level and skip-level (grandparent) correlation", () => {
  // two-level, each to its immediate parent.
  assert.deepStrictEqual(
    query(t123(), "SELECT t1.id FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.v = t1.v AND EXISTS (SELECT 1 FROM t3 WHERE t3.v = t2.v)) ORDER BY t1.id"),
    [["1"]],
  );
  // skip-level: the innermost references the GRANDPARENT t1, skipping t2.
  assert.deepStrictEqual(
    query(t123(), "SELECT t1.id FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE EXISTS (SELECT 1 FROM t3 WHERE t3.v = t1.v)) ORDER BY t1.id"),
    [["1"], ["2"]],
  );
});

test("outer reference inside an aggregate argument", () => {
  // sum(t2.v + t1.v) over t2 for each t1 row -> (10+10)+(30+10)=60 ; (10+20)+(30+20)=80.
  assert.deepStrictEqual(
    query(t123(), "SELECT t1.id, (SELECT sum(t2.v + t1.v) FROM t2) FROM t1 ORDER BY t1.id"),
    [["1", "60"], ["2", "80"]],
  );
});

test("a correlated subquery's cost is per outer row", () => {
  // The derivation is in spec/conformance/suites/subquery/correlated.test (cost = 14).
  assert.strictEqual(
    execute(t123(), "SELECT t1.id FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.v = t1.v)").cost,
    14n,
  );
});

test("a subquery's inner error is raised over an empty outer (plan-once)", () => {
  // The subquery is planned once, so a >1-column error fires even when the outer is empty.
  const db = dbWith([
    "CREATE TABLE e (id int32 PRIMARY KEY, v int32)",
    "CREATE TABLE f (id int32 PRIMARY KEY, v int32)",
    "INSERT INTO f VALUES (1, 1)",
  ]);
  assert.strictEqual(
    errCode(() => execute(db, "SELECT (SELECT id, v FROM f WHERE v = e.v) FROM e")),
    "42601",
  );
});
