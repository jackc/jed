// Join base-table primary-key pushdown over a MULTI-LEAF table (spec/design/cost.md §3 "bounded scan
// / JOIN"). The conformance corpus (spec/conformance/suites/joins/pushdown.test) pins the cost
// contract on single-leaf tables; this exercises what it cannot — a join base table wide enough that
// a full materialization would be expensive, so bounding it by its own primary key is the difference
// between sublinear and a full double scan. The win is shown by contrast: `WHERE a.id = c` (a's PK,
// bounded) vs `WHERE a.k = c` (not the PK, full scan), which return the SAME row because k == id.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Engine, execute } from "../src/tooling.ts";
import type { Value } from "../src/value.ts";

// `a` is n rows (id i32 PRIMARY KEY, k i32; k == id) wide enough to span several leaves; `b` is
// three small rows whose k-values exist as a's k-values, so the join matches.
function joinTables(n: number): Engine {
  const db = new Engine();
  execute(db, "CREATE TABLE a (id i32 PRIMARY KEY, k i32)");
  execute(db, "CREATE TABLE b (id i32 PRIMARY KEY, k i32)");
  const parts: string[] = [];
  for (let i = 1; i <= n; i++) parts.push(`(${i},${i})`);
  execute(db, "INSERT INTO a VALUES " + parts.join(","));
  execute(db, "INSERT INTO b VALUES (1, 500), (2, 600), (3, 700)");
  return db;
}

function cost(db: Engine, sql: string): bigint {
  return execute(db, sql).cost;
}

function intOf(v: Value): number {
  if (v.kind !== "int") throw new Error("expected int, got " + v.kind);
  return Number(v.int);
}

function ids(db: Engine, sql: string): number[] {
  const o = execute(db, sql);
  if (o.kind !== "query") throw new Error("expected a query result");
  return o.rows.map((r) => intOf(r[0]!));
}

test("join pushdown bounds one side sublinear", () => {
  const db = joinTables(1000);
  // Both pick the single a row with id/k == 500 and join it to b(k=500); `a.id` is the PK (seeks a),
  // `a.k` is not (full scan of a). k == id, so they return the SAME row.
  const bounded = "SELECT a.id FROM a JOIN b ON a.k = b.k WHERE a.id = 500";
  const unbounded = "SELECT a.id FROM a JOIN b ON a.k = b.k WHERE a.k = 500";
  assert.deepStrictEqual(ids(db, bounded), [500]);
  assert.deepStrictEqual(ids(db, unbounded), [500]);

  const seek = cost(db, bounded);
  const scan = cost(db, unbounded);
  // The non-PK predicate full-scans all ~1000 a rows; the PK pushdown materializes one a row.
  assert.ok(seek * 10n < scan, `bounded join ${seek} should be far below full-scan join ${scan}`);
  assert.ok(
    seek <= 60n,
    `bounded join ${seek} should be sublinear (seek a + scan small b), not ~1000`,
  );
});

test("join pushdown miss collapses to empty", () => {
  const db = joinTables(1000);
  // A point-lookup miss on the bounded side materializes ZERO a rows, so the loop has nothing to
  // drive: empty result at the cost of (a's miss page) + (b's full scan), not a 1000-row scan.
  const q = "SELECT a.id FROM a JOIN b ON a.k = b.k WHERE a.id = 999999";
  assert.deepStrictEqual(ids(db, q), []);
  assert.ok(cost(db, q) <= 60n, "a miss-bounded join should collapse to b's small scan, not ~1000");
});

test("join pushdown both sides bounded", () => {
  const db = joinTables(1000);
  // Bounding BOTH tables by their own PK: a.id = 500 (one a row, k=500) and b.id = 1 (one b row,
  // k=500). They join on k. Sublinear in a's size.
  const q = "SELECT a.id, b.id FROM a JOIN b ON a.k = b.k WHERE a.id = 500 AND b.id = 1";
  assert.deepStrictEqual(ids(db, q), [500]);
  assert.ok(cost(db, q) <= 30n, "both-sides-bounded join should be tiny, not ~1000");
});
