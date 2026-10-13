// Parameter-sensitive ("custom") plans (spec/design/estimator.md §6.1). The shared corpus binds no
// parameters, and its literal EXPLAIN records already pin every custom plan, so these per-core tests
// pin the binding path: a statement planned with bound values estimates, selects, and costs exactly
// like the equivalent literal query; a value-blind statement keeps one cached plan; a sensitive one
// re-plans as its value moves between statistics classes; and a memo hit equals fresh planning.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Engine, execute, intValue, prepare, query, queryPrepared } from "../src/tooling.ts";
import type { Value } from "../src/value.ts";
import { boolValue, nullValue, render, textValue } from "../src/value.ts";

type PreparedLike = ReturnType<typeof prepare>;

type CacheEntry = { sp: unknown; sensitive: boolean; memo: unknown[] };

// The private scan-plan cache slot (white-box: TS `private` is compile-time only).
function cacheOf(stmt: PreparedLike): CacheEntry | null {
  return (stmt as unknown as { scHolder: { cache: CacheEntry | null } }).scHolder.cache;
}

function run(db: Engine, sql: string, params: Value[] = []): { rows: Value[][]; cost: bigint } {
  const cursor = query(db, sql, params);
  const rows: Value[][] = [];
  for (const r of cursor) rows.push(r);
  const cost = cursor.cost;
  cursor.close();
  return { rows, cost };
}

function drain(db: Engine, stmt: PreparedLike, params: Value[]): { rows: Value[][]; cost: bigint } {
  const cursor = queryPrepared(db, stmt, params);
  const rows: Value[][] = [];
  for (const r of cursor) rows.push(r);
  const cost = cursor.cost;
  cursor.close();
  return { rows, cost };
}

// 1000 items: cat is 1 for nine rows in ten (an MCV) and unique otherwise; tags holds the common tag
// 'a' on every row and the rare tag 'r<i>' on one. Both are indexed and analyzed.
function seed(): Engine {
  const db = new Engine();
  execute(db, "CREATE TABLE items (id i32 PRIMARY KEY, cat i32, tags text[])");
  execute(
    db,
    "INSERT INTO items SELECT CAST(g AS i32), " +
      "CAST(CASE WHEN g % 10 = 0 THEN g ELSE 1 END AS i32), " +
      "ARRAY['a', 'r' || CAST(g AS text)] FROM generate_series(1, 1000) AS g",
  );
  execute(db, "CREATE INDEX items_cat ON items (cat)");
  execute(db, "CREATE INDEX items_tags ON items USING gin (tags)");
  execute(db, "ANALYZE items");
  return db;
}

// A $N still renders symbolically; the comparison with the literal query is over everything else.
function stripDetail(plan: Value[][]): Value[][] {
  return plan.map((row) => row.map((cell, i) => (i === 2 ? textValue("") : cell)));
}

function assertCustomMatchesLiteral(
  db: Engine,
  paramSql: string,
  params: Value[],
  literalSql: string,
): void {
  const custom = run(db, `EXPLAIN ${paramSql}`, params).rows;
  const literal = run(db, `EXPLAIN ${literalSql}`).rows;
  assert.deepEqual(stripDetail(custom), stripDetail(literal), `${paramSql} vs ${literalSql}`);
  const stmt = prepare(db, paramSql);
  const prepared = drain(db, stmt, params);
  const want = run(db, literalSql);
  assert.deepEqual(prepared.rows, want.rows);
  assert.equal(prepared.cost, want.cost, `actual cost of ${paramSql}`);
}

function accessDetails(plan: Value[][]): string {
  return plan
    .map((row) => render(row[2]!))
    .filter((d) => d.includes("scan") || d.includes("bound"))
    .join(" | ");
}

test("custom plans: MCV and rare values take their literal plans", () => {
  const db = seed();
  const common = run(db, "EXPLAIN SELECT id FROM items WHERE cat = 1").rows;
  const rare = run(db, "EXPLAIN SELECT id FROM items WHERE cat = 70").rows;
  assert.notEqual(accessDetails(common), accessDetails(rare), "the data must flip the plan");
  const sql = "SELECT id FROM items WHERE cat = $1";
  assertCustomMatchesLiteral(db, sql, [intValue(1n)], "SELECT id FROM items WHERE cat = 1");
  assertCustomMatchesLiteral(db, sql, [intValue(70n)], "SELECT id FROM items WHERE cat = 70");
  assertCustomMatchesLiteral(
    db,
    "SELECT id FROM items WHERE cat > $1",
    [intValue(500n)],
    "SELECT id FROM items WHERE cat > 500",
  );
});

test("custom plans: GIN term facts apply to a bound operand", () => {
  const db = seed();
  const common = run(db, "EXPLAIN SELECT id FROM items WHERE tags @> ARRAY['a']").rows;
  const rare = run(db, "EXPLAIN SELECT id FROM items WHERE tags @> ARRAY['r7']").rows;
  assert.notDeepEqual(common, rare);
  for (const sql of [
    "SELECT id FROM items WHERE tags @> ARRAY[$1]",
    "SELECT id FROM items WHERE $1 = ANY(tags)",
  ]) {
    for (const tag of ["a", "r7"]) {
      assertCustomMatchesLiteral(db, sql, [textValue(tag)], sql.replaceAll("$1", `'${tag}'`));
    }
  }
});

test("custom plans: a key lookup is planned once", () => {
  const db = seed();
  const stmt = prepare(db, "SELECT cat FROM items WHERE id = $1");
  const first = drain(db, stmt, [intValue(5n)]);
  assert.deepEqual(first.rows, [[intValue(1n)]]);
  const entry = cacheOf(stmt);
  assert.ok(entry !== null && !entry.sensitive, "a full-key equality is value-blind");
  const second = drain(db, stmt, [intValue(900n)]);
  assert.deepEqual(second.rows, [[intValue(900n)]]);
  assert.equal(second.cost, first.cost);
  assert.equal(cacheOf(stmt), entry, "the value-blind plan is reused");
  // A NULL binding takes its own custom plan (a known NULL proves no row) without disturbing the
  // cached one.
  assert.deepEqual(drain(db, stmt, [nullValue()]).rows, []);
  assert.equal(cacheOf(stmt), entry);
  assertCustomMatchesLiteral(
    db,
    "SELECT cat FROM items WHERE id = $1",
    [nullValue()],
    "SELECT cat FROM items WHERE id = NULL",
  );
});

test("custom plans: a sensitive statement re-plans and memoizes", () => {
  const db = seed();
  const stmt = prepare(db, "SELECT id FROM items WHERE cat = $1");
  const common = drain(db, stmt, [intValue(1n)]);
  assert.equal(common.rows.length, 900);
  const entry = cacheOf(stmt)!;
  assert.ok(entry.sensitive);
  assert.equal(entry.memo.length, 1);
  const rare = drain(db, stmt, [intValue(70n)]);
  assert.deepEqual(rare.rows, [[intValue(70n)]]);
  assert.equal(entry.memo.length, 2);
  assert.equal(common.cost, run(db, "SELECT id FROM items WHERE cat = 1").cost);
  assert.equal(rare.cost, run(db, "SELECT id FROM items WHERE cat = 70").cost);
  // A memo hit equals fresh planning.
  const again = drain(db, stmt, [intValue(70n)]);
  assert.deepEqual(again.rows, rare.rows);
  assert.equal(again.cost, rare.cost);
  assert.equal(entry.memo.length, 2, "a memo hit adds nothing");
  for (let v = 0n; v < 20n; v++) drain(db, stmt, [intValue(1000n + v)]);
  assert.ok(entry.memo.length <= 8, "the memo is bounded");
  // A relevant estimator-input change replaces the entry and its memo.
  execute(db, "INSERT INTO items VALUES (5000, 70, ARRAY['a'])");
  assert.equal(drain(db, stmt, [intValue(70n)]).rows.length, 2);
  assert.notEqual(cacheOf(stmt), entry);
  assert.equal(cacheOf(stmt)!.memo.length, 1);
});

test("custom plans: DML target scans use the custom plan", () => {
  const db = seed();
  const restore =
    "INSERT INTO items VALUES (70, 70, ARRAY['a', 'r70']) ON CONFLICT (id) DO UPDATE SET cat = 70, tags = ARRAY['a', 'r70']";
  for (const [paramSql, literalSql] of [
    ["UPDATE items SET tags = NULL WHERE cat = $1", "UPDATE items SET tags = NULL WHERE cat = 70"],
    ["DELETE FROM items WHERE cat = $1", "DELETE FROM items WHERE cat = 70"],
    [
      "DELETE FROM items WHERE id IN (SELECT id FROM items WHERE cat = $1)",
      "DELETE FROM items WHERE id IN (SELECT id FROM items WHERE cat = 70)",
    ],
  ] as const) {
    let custom = run(db, `EXPLAIN ${paramSql}`, [intValue(70n)]).rows;
    let literal = run(db, `EXPLAIN ${literalSql}`).rows;
    assert.deepEqual(stripDetail(custom), stripDetail(literal), paramSql);
    custom = run(db, `EXPLAIN ANALYZE ${paramSql}`, [intValue(70n)]).rows;
    execute(db, restore);
    literal = run(db, `EXPLAIN ANALYZE ${literalSql}`).rows;
    assert.deepEqual(stripDetail(custom), stripDetail(literal), paramSql);
    execute(db, restore);
  }
});

test("custom plans: an INSERT ... SELECT source uses the custom plan", () => {
  const db = seed();
  execute(db, "CREATE TABLE picked (id i32 PRIMARY KEY)");
  const custom = run(db, "EXPLAIN ANALYZE INSERT INTO picked SELECT id FROM items WHERE cat = $1", [
    intValue(70n),
  ]).rows;
  execute(db, "DELETE FROM picked");
  const literal = run(
    db,
    "EXPLAIN ANALYZE INSERT INTO picked SELECT id FROM items WHERE cat = 70",
  ).rows;
  assert.deepEqual(stripDetail(custom), stripDetail(literal));
});

test("custom plans: generate_series bounds are plan-time literals", () => {
  const db = new Engine();
  assertCustomMatchesLiteral(
    db,
    "SELECT count(*) FROM generate_series(1, $1) AS g",
    [intValue(50n)],
    "SELECT count(*) FROM generate_series(1, 50) AS g",
  );
});

// The classification's soundness (estimator.md §6.1, "never the reverse"): a statement classified
// value-blind has one plan for every non-NULL binding, so its unbound EXPLAIN equals its EXPLAIN with
// values, estimates included.
test("custom plans: the value-blind classification is sound", () => {
  const db = seed();
  execute(db, "CREATE TABLE t2 (id i32 PRIMARY KEY, v i32, w text)");
  execute(
    db,
    "INSERT INTO t2 SELECT CAST(g AS i32), CAST(g % 7 AS i32), CAST(g AS text) FROM generate_series(1, 50) AS g",
  );
  execute(db, "CREATE INDEX t2_v ON t2 (v)");
  const i = (n: number) => intValue(BigInt(n));
  const cases: [string, Value[], boolean | null][] = [
    ["SELECT cat FROM items WHERE id = $1", [i(5)], false],
    ["SELECT id FROM items WHERE cat = $1", [i(70)], true],
    ["SELECT id FROM items WHERE id > $1", [i(990)], true],
    ["SELECT count(*) FROM items i JOIN items j ON i.id = j.cat WHERE i.id = $1", [i(70)], null],
    ["SELECT * FROM t2 WHERE v = $1", [i(3)], false],
    ["SELECT * FROM t2 WHERE v > $1", [i(3)], false],
    ["SELECT * FROM t2 WHERE v > $1 AND v < $2", [i(5), i(1)], true],
    ["SELECT * FROM t2 WHERE w = $1 OR v = $2", [textValue("4"), i(2)], null],
    ["SELECT * FROM t2 WHERE id = $1 AND v = $2", [i(4), i(4)], null],
    ["SELECT * FROM t2 WHERE v = $1 OR v = $2", [i(4), i(4)], null],
    ["SELECT CAST($1 AS i32) + id FROM t2 WHERE id = 3", [i(9)], false],
    ["SELECT * FROM t2 WHERE v = 2 AND $1 = TRUE", [boolValue(false)], true],
    ["SELECT * FROM t2 WHERE v BETWEEN $1 AND $2", [i(2), i(3)], null],
  ];
  for (const [sql, params, expect] of cases) {
    const stmt = prepare(db, sql);
    drain(db, stmt, params);
    const entry = cacheOf(stmt);
    assert.ok(entry !== null, `${sql}: no cache entry`);
    if (expect !== null) assert.equal(entry.sensitive, expect, `classification of ${sql}`);
    if (!entry.sensitive) {
      assert.deepEqual(
        run(db, `EXPLAIN ${sql}`).rows,
        run(db, `EXPLAIN ${sql}`, params).rows,
        `${sql} is classified value-blind`,
      );
    }
  }
});
