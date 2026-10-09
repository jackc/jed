// Stage-2 WHERE pushdown with BOUND PARAMETERS (spec/design/planner.md §3.2). The conformance corpus
// pins pushdown rows/costs/EXPLAIN with literals (joins/where_pushdown.test,
// query/where_rewrite_explain.test) but cannot bind `$N`; this checks the host-API surface it cannot:
// a parameter operand is pushdown-safe, the pushed filter reads the bound value inside the scan, the
// cost equals the same query spelled with the literal, and a prepared statement's cached plan reuses
// the pushdown across executions with different values.

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  Engine,
  execute,
  executeParams,
  intValue,
  type Outcome,
  prepare,
  queryPrepared,
} from "../src/tooling.ts";
import type { Value } from "../src/value.ts";

const PARAM =
  "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE a.v >= $1 AND b.w > $2 ORDER BY a.id, b.id";

function literal(v: bigint, w: bigint): string {
  return `SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE a.v >= ${v} AND b.w > ${w} ORDER BY a.id, b.id`;
}

function queryResult(out: Outcome): { rows: Value[][]; cost: bigint } {
  assert.equal(out.kind, "query");
  if (out.kind !== "query") throw new Error("expected a query result");
  return { rows: out.rows, cost: out.cost };
}

test("where pushdown: bound parameters filter inside the scan and charge like literals", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE a (id i32 PRIMARY KEY, k i32, v i32)");
  execute(db, "CREATE TABLE b (id i32 PRIMARY KEY, k i32, w i32)");
  execute(db, "INSERT INTO a VALUES (1, 1, 10), (2, 2, 20), (3, 3, NULL), (4, 9, 40)");
  execute(db, "INSERT INTO b VALUES (11, 1, 5), (12, 2, NULL), (13, 2, 7), (14, 7, 0), (15, 8, 1)");
  const stmt = prepare(db, PARAM);
  const cases: [bigint, bigint, [bigint, bigint][]][] = [
    [
      10n,
      4n,
      [
        [1n, 11n],
        [2n, 13n],
      ],
    ],
    [20n, 0n, [[2n, 13n]]],
    [50n, 0n, []],
  ];
  for (const [v, w, want] of cases) {
    const params = [intValue(v), intValue(w)];
    const expected = want.map(([x, y]) => [intValue(x), intValue(y)]);
    const lit = queryResult(execute(db, literal(v, w)));

    const direct = queryResult(executeParams(db, PARAM, params));
    assert.deepEqual(direct.rows, expected, `v=${v} w=${w}`);
    assert.equal(direct.cost, lit.cost, `v=${v} w=${w}: a pushed $N must charge like a literal`);

    const cursor = queryPrepared(db, stmt, params);
    const rows: Value[][] = [];
    for (const r of cursor) rows.push(r);
    const cost = cursor.cost;
    cursor.close();
    assert.deepEqual(rows, expected, `prepared v=${v} w=${w}`);
    assert.equal(cost, lit.cost, `prepared v=${v} w=${w}`);
  }
});
