// Cost ceiling + deterministic abort (CLAUDE.md §13; spec/design/cost.md §6). A caller sets
// maxCost on the handle; the instant a statement's accrued execution cost reaches it, execution
// aborts with 54P01. The conformance corpus (spec/conformance/suites/resource/cost_limit.test) pins
// the cross-core abort points on small tables; this exercises what it cannot — that the bound is on
// ACTUAL accrued cost (a cheap point lookup survives a ceiling a full scan blows) and that the abort
// threads through SELECT / DELETE / UPDATE and a pathological expression.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, EngineError, Session } from "../src/tooling.ts";
import type { Handle } from "./util.ts";

function rowTable(n: number): Session {
  const db = Database.newInMemory().session();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  const parts: string[] = [];
  for (let i = 1; i <= n; i++) parts.push(`(${i},${i})`);
  db.execute("INSERT INTO t VALUES " + parts.join(","));
  return db;
}

function cost(db: Handle, sql: string): bigint {
  return db.execute(sql).cost;
}

function rowCount(db: Handle, sql: string): number {
  const o = db.execute(sql);
  if (o.kind !== "query") throw new Error("expected a query result");
  return o.rows.length;
}

function assertAborts(db: Handle, sql: string): void {
  assert.throws(
    () => db.execute(sql),
    (e: unknown) => e instanceof EngineError && e.code() === "54P01",
    `expected a 54P01 cost-limit abort from: ${sql}`,
  );
}

test("cost limit is unlimited by default", () => {
  const db = rowTable(100);
  assert.strictEqual(db.maxCost, 0n);
  db.execute("SELECT * FROM t"); // runs to completion, no ceiling
});

test("a ceiling above the cost succeeds, below aborts", () => {
  const db = rowTable(50);
  const full = cost(db, "SELECT v FROM t");
  assert.ok(full > 10n, `expected a non-trivial full-scan cost, got ${full}`);

  db.setMaxCost(full + 100n);
  assert.strictEqual(cost(db, "SELECT v FROM t"), full);

  db.setMaxCost(full / 2n);
  assertAborts(db, "SELECT v FROM t");

  db.setMaxCost(0n); // cleared → unlimited again
  assert.strictEqual(cost(db, "SELECT v FROM t"), full);
});

test("a ceiling equal to the true cost aborts; one above succeeds", () => {
  const db = rowTable(20);
  const full = cost(db, "SELECT v FROM t");
  // The ceiling is the first DISALLOWED value: accrued reaching it aborts (CLAUDE.md §13).
  db.setMaxCost(full);
  assertAborts(db, "SELECT v FROM t");
  db.setMaxCost(full + 1n);
  assert.strictEqual(cost(db, "SELECT v FROM t"), full);
});

test("a point lookup survives a ceiling the full scan blows", () => {
  const db = rowTable(200);
  const full = cost(db, "SELECT v FROM t");
  const lookup = cost(db, "SELECT v FROM t WHERE id = 100");
  assert.ok(lookup * 4n < full, `point lookup ${lookup} should be far below full scan ${full}`);

  // Between the two: the keyed lookup runs, the scan aborts. The bound is on real cost, not size.
  db.setMaxCost((lookup + full) / 2n);
  assert.strictEqual(cost(db, "SELECT v FROM t WHERE id = 100"), lookup);
  assertAborts(db, "SELECT v FROM t");
});

test("the abort threads through DELETE and UPDATE and rolls back", () => {
  const db = rowTable(50);
  const scanCost = cost(db, "SELECT v FROM t");
  db.setMaxCost(scanCost / 2n);

  assertAborts(db, "DELETE FROM t WHERE v > 0");
  assertAborts(db, "UPDATE t SET v = v + 1 WHERE v > 0");

  // The aborts rolled back (autocommit): the table is untouched.
  db.setMaxCost(0n);
  assert.strictEqual(rowCount(db, "SELECT v FROM t"), 50);
  assert.strictEqual(cost(db, "SELECT v FROM t"), scanCost);
});

test("a pathological expression aborts on one row (per-node eval guard)", () => {
  const db = rowTable(1);
  const expr = Array<string>(80).fill("1").join(" + ");
  const sql = `SELECT ${expr} FROM t`;
  const big = cost(db, sql);
  db.setMaxCost(big / 2n);
  assertAborts(db, sql);
});

test("a provably-empty bound accrues 0 and survives a ceiling of 1", () => {
  const db = rowTable(10);
  db.setMaxCost(1n);
  const o = db.execute("SELECT v FROM t WHERE id > 5 AND id < 5");
  if (o.kind !== "query") throw new Error("expected a query result");
  assert.strictEqual(o.rows.length, 0);
  assert.strictEqual(o.cost, 0n);
});
