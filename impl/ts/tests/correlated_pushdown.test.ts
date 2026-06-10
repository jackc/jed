// Correlated primary-key pushdown over a MULTI-LEAF inner table (spec/design/cost.md §3 "bounded
// scan / correlated"). The conformance corpus
// (spec/conformance/suites/subquery/correlated_pushdown.test) pins the cost contract on single-leaf
// tables; this exercises what it cannot — an inner table wide enough that re-scanning it per outer row
// would be visibly expensive, so the per-outer-row seek is the difference between sublinear and
// quadratic. The win is shown by contrast: `inner.pk = o.col` (bounded) vs `inner.v = o.col` (a full
// re-scan), which return the SAME rows because v == id.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, execute } from "../src/lib.ts";
import type { Value } from "../src/value.ts";

// `o` is five outer rows whose k-values all exist as inner ids; `inr` is n rows (id int32 PRIMARY KEY,
// v int32; v == id) wide enough to span several leaves.
function correlatedTables(n: number): Database {
  const db = new Database();
  execute(db, "CREATE TABLE o (id int32 PRIMARY KEY, k int32)");
  execute(db, "CREATE TABLE inr (id int32 PRIMARY KEY, v int32)");
  execute(db, "INSERT INTO o VALUES (1, 100), (2, 300), (3, 500), (4, 700), (5, 900)");
  const parts: string[] = [];
  for (let i = 1; i <= n; i++) parts.push(`(${i},${i})`);
  execute(db, "INSERT INTO inr VALUES " + parts.join(","));
  return db;
}

function cost(db: Database, sql: string): bigint {
  return execute(db, sql).cost;
}

function intOf(v: Value): number {
  if (v.kind !== "int") throw new Error("expected int, got " + v.kind);
  return Number(v.int);
}

function ids(db: Database, sql: string): number[] {
  const o = execute(db, sql);
  if (o.kind !== "query") throw new Error("expected a query result");
  return o.rows.map((r) => intOf(r[0]!));
}

test("correlated EXISTS seek is sublinear", () => {
  const db = correlatedTables(1000);
  // Both correlate the inner to each outer row; `inr.id` is the PK (seeks), `inr.v` is not (full
  // re-scan). v == id, so they select the SAME inner rows and the SAME outer rows survive.
  const bounded = "SELECT o.id FROM o WHERE EXISTS (SELECT 1 FROM inr WHERE inr.id = o.k)";
  const unbounded = "SELECT o.id FROM o WHERE EXISTS (SELECT 1 FROM inr WHERE inr.v = o.k)";
  assert.deepStrictEqual(ids(db, bounded), [1, 2, 3, 4, 5]);
  assert.deepStrictEqual(ids(db, unbounded), [1, 2, 3, 4, 5]);

  const seek = cost(db, bounded);
  const scan = cost(db, unbounded);
  // The non-PK correlation re-scans all ~1000 inner rows for each of the 5 outer rows; the PK
  // pushdown seeks instead, so it is an order of magnitude cheaper.
  assert.ok(seek * 10n < scan, `correlated seek ${seek} should be far below full re-scan ${scan}`);
  // Sublinear in the inner size: 5 outer rows, each ≈ a point lookup (height + a row), not ~1000.
  assert.ok(seek <= 400n, `correlated seek ${seek} should be sublinear (≈ outer × height), not ~5000`);
});

test("correlated scalar seek matches unbounded rows", () => {
  const db = correlatedTables(1000);
  // A correlated SCALAR subquery seeking the inner PK returns each outer row's inner value. Rows are
  // identical to what a full re-scan would produce; only the cost differs.
  const bounded = "SELECT (SELECT inr.v FROM inr WHERE inr.id = o.k) FROM o ORDER BY o.id";
  const unbounded = "SELECT (SELECT inr.v FROM inr WHERE inr.v = o.k) FROM o ORDER BY o.id";
  assert.deepStrictEqual(ids(db, bounded), [100, 300, 500, 700, 900]);

  const seek = cost(db, bounded);
  const scan = cost(db, unbounded);
  assert.ok(seek * 10n < scan, `correlated scalar seek ${seek} should be far below full re-scan ${scan}`);
});

test("correlated miss and NULL outer seek nothing", () => {
  const db = correlatedTables(1000);
  // An outer k with no matching inner id is a point-lookup miss (visits the leaf, reads no row); a
  // NULL outer k is a 3VL-empty bound (reads no page, no row). Neither re-scans the inner.
  execute(db, "INSERT INTO o VALUES (6, 999999), (7, NULL)");
  const q = "SELECT o.id FROM o WHERE EXISTS (SELECT 1 FROM inr WHERE inr.id = o.k)";
  assert.deepStrictEqual(ids(db, q), [1, 2, 3, 4, 5]);
  assert.ok(cost(db, q) <= 500n, "seek cost should stay sublinear with a miss and a NULL outer row");
});
