// Prepared-statement plan cache (spec/design/api.md §2.4). A prepared statement caches its resolved
// scan plan and reuses it across executes, re-planning only when the catalog changes. The behavior is
// invisible to the conformance corpus (which drives the materialized execute path and never reuses a
// plan), so these per-core tests pin it directly: the cache engages (white-box, via the private
// holder) and reuse is result/cost-identical (the regex-cost-drift guard); a DDL between executes
// re-plans (no stale plan served); and a non-cacheable plan (subquery / precompiled regex / temp) is
// never cached.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Engine, execute, intValue, prepare } from "../src/tooling.ts";
import type { Value } from "../src/value.ts";

type PreparedLike = ReturnType<typeof prepare>;

// The private scan-plan cache slot (white-box: TS `private` is compile-time only).
function cacheOf(stmt: PreparedLike): { catGen: bigint; sp: unknown } | null {
  return (stmt as unknown as { scHolder: { cache: { catGen: bigint; sp: unknown } | null } })
    .scHolder.cache;
}

function drain(stmt: PreparedLike, params: Value[] = []): { rows: Value[][]; cost: bigint } {
  const cursor = stmt.query(params);
  const rows: Value[][] = [];
  for (const r of cursor) rows.push(r);
  const cost = cursor.cost;
  cursor.close();
  return { rows, cost };
}

function seedOrders(db: Engine, n: number): void {
  execute(db, "CREATE TABLE orders (id i32 PRIMARY KEY, amount i32)");
  for (let i = 1; i <= n; i++) execute(db, `INSERT INTO orders VALUES (${i}, ${i * 100})`);
}

// A point lookup fills the cache on the first execute and REUSES the exact plan (same object) on later
// executes, and reuse is cost-identical (the regex-cost-drift guard). Params still bind per execute.
test("plan cache: point lookup reuses the plan, cost-identical", () => {
  const db = new Engine();
  seedOrders(db, 5);
  const stmt = prepare(db, "SELECT id, amount FROM orders WHERE id = $1");

  const r1 = drain(stmt, [intValue(3n)]);
  assert.deepEqual(r1.rows, [[intValue(3n), intValue(300n)]]);
  const cached = cacheOf(stmt);
  assert.notEqual(cached, null, "cache should fill on the first cacheable execute");
  const sp = cached!.sp;

  const r2 = drain(stmt, [intValue(3n)]);
  assert.deepEqual(r2.rows, [[intValue(3n), intValue(300n)]]);
  assert.equal(cacheOf(stmt)!.sp, sp, "the cached plan object changed — statement re-planned");
  assert.equal(r2.cost, r1.cost, "reusing the cached plan must be cost-identical");

  // Different param binds against the same cached plan.
  const r3 = drain(stmt, [intValue(5n)]);
  assert.deepEqual(r3.rows, [[intValue(5n), intValue(500n)]]);
  assert.equal(cacheOf(stmt)!.sp, sp, "plan object changed on a param-only change");

  // A no-match param.
  assert.deepEqual(drain(stmt, [intValue(999n)]).rows, []);
});

// DROP + re-CREATE with a different shape bumps the catalog generation, so the next execute re-plans
// and reflects the new column set — a stale cached plan would return the old shape.
test("plan cache: DROP/CREATE invalidates", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, a i32)");
  execute(db, "INSERT INTO t VALUES (1, 10)");
  const stmt = prepare(db, "SELECT * FROM t WHERE id = $1");

  const r1 = drain(stmt, [intValue(1n)]);
  assert.deepEqual(r1.rows, [[intValue(1n), intValue(10n)]]);
  const gen1 = cacheOf(stmt)!.catGen;

  execute(db, "DROP TABLE t");
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, a i32, c i32)");
  execute(db, "INSERT INTO t VALUES (1, 10, 20)");

  const r2 = drain(stmt, [intValue(1n)]);
  assert.deepEqual(
    r2.rows,
    [[intValue(1n), intValue(10n), intValue(20n)]],
    "a stale 2-column plan was served after DROP/CREATE",
  );
  assert.notEqual(cacheOf(stmt)!.catGen, gen1, "catGen did not advance after DROP/CREATE");
});

// CREATE INDEX between executes invalidates the cached full-scan plan; the re-plan picks up the new
// secondary index (cheaper cost). DROP INDEX reverses it.
test("plan cache: index DDL invalidates", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, a i32)");
  for (let i = 1; i <= 50; i++) execute(db, `INSERT INTO t VALUES (${i}, ${i})`);
  const stmt = prepare(db, "SELECT id FROM t WHERE a = $1");

  const scan = drain(stmt, [intValue(25n)]);
  assert.deepEqual(scan.rows, [[intValue(25n)]]);

  execute(db, "CREATE INDEX t_a ON t (a)");
  const idx = drain(stmt, [intValue(25n)]);
  assert.deepEqual(idx.rows, [[intValue(25n)]]);
  assert.ok(
    idx.cost < scan.cost,
    `expected index lookup cheaper than full scan after CREATE INDEX: idx=${idx.cost} scan=${scan.cost} (cached full-scan plan served?)`,
  );

  execute(db, "DROP INDEX t_a");
  const scan2 = drain(stmt, [intValue(25n)]);
  assert.deepEqual(scan2.rows, [[intValue(25n)]]);
  assert.ok(
    scan2.cost > idx.cost,
    `expected full scan costlier than index after DROP INDEX: scan=${scan2.cost} idx=${idx.cost} (stale index plan served?)`,
  );
});

// A precompiled (constant-pattern) regex is never cached — reusing its plan would under-charge the
// 2nd+ execute (the one-shot compile flag). Re-planned each execute, so cost is identical.
test("plan cache: precompiled-regex plan is not cached", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, note text)");
  execute(db, "INSERT INTO t VALUES (1, 'abc'), (2, 'xyz'), (3, 'abd')");
  const stmt = prepare(db, "SELECT id FROM t WHERE note ~ 'ab'");

  const r1 = drain(stmt);
  assert.deepEqual(r1.rows, [[intValue(1n)], [intValue(3n)]]);
  assert.equal(cacheOf(stmt), null, "a precompiled-regex plan must not be cached");
  const r2 = drain(stmt);
  assert.equal(r2.cost, r1.cost, "regex cost drifted across executes (regex plan wrongly cached?)");
});

// A plan with an uncorrelated subquery is never cached; results stay correct across executes.
test("plan cache: subquery plan is not cached, stays correct", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, a i32)");
  execute(db, "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)");
  const stmt = prepare(db, "SELECT id FROM t WHERE id = (SELECT max(id) FROM t)");

  assert.deepEqual(drain(stmt).rows, [[intValue(3n)]]);
  assert.equal(cacheOf(stmt), null, "a subquery plan must not be cached");
  // Insert a larger id; the (uncached, re-planned + re-evaluated) subquery must reflect it.
  execute(db, "INSERT INTO t VALUES (4, 40)");
  assert.deepEqual(drain(stmt).rows, [[intValue(4n)]]);
});
