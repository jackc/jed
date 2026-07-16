// Prepared-statement plan cache (spec/design/api.md §2.4). A prepared statement caches its resolved
// scan plan and reuses it across executes while its exact estimator inputs remain unchanged. The behavior is
// invisible to the conformance corpus (which drives the materialized execute path and never reuses a
// plan), so these per-core tests pin it directly: the cache engages (white-box, via the private
// holder) and reuse is result/cost-identical (the regex-cost-drift guard); a DDL between executes
// re-plans (no stale plan served); and a non-cacheable plan (subquery / precompiled regex / temp) is
// never cached.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import {
  Engine,
  EngineError,
  execute,
  intValue,
  loadUnicodeData,
  prepare,
  PrivilegeSet,
  queryPrepared,
  toImage,
} from "../src/tooling.ts";
import { loadedCollation } from "../src/collation.ts";
import { attachMemory } from "../src/shared.ts";
import { ExplainRender } from "../src/scope.ts";
import type { Value } from "../src/value.ts";
import { textValue } from "../src/value.ts";
import { memDb } from "./mem_db.ts";
import { specPath } from "./tomlmini.ts";

type PreparedLike = ReturnType<typeof prepare>;

// The private scan-plan cache slot (white-box: TS `private` is compile-time only).
function cacheOf(
  stmt: PreparedLike,
): { inputs: { database: object; catGen: bigint; revision: object }[]; sp: unknown } | null {
  return (
    stmt as unknown as {
      scHolder: {
        cache: {
          inputs: { database: object; catGen: bigint; revision: object }[];
          sp: unknown;
        } | null;
      };
    }
  ).scHolder.cache;
}

function insertCacheOf(
  stmt: PreparedLike,
): { plan: unknown; signature: { catGen: bigint } } | null {
  return (
    stmt as unknown as {
      icHolder: { cache: { plan: unknown; signature: { catGen: bigint } } | null };
    }
  ).icHolder.cache;
}

function caught(fn: () => unknown): { code: string; message: string } {
  try {
    fn();
  } catch (e) {
    if (e instanceof EngineError) return { code: e.code(), message: e.message };
    throw e;
  }
  throw new Error("expected EngineError");
}

function drain(
  db: Engine,
  stmt: PreparedLike,
  params: Value[] = [],
): { rows: Value[][]; cost: bigint } {
  const cursor = queryPrepared(db, stmt, params);
  const rows: Value[][] = [];
  for (const r of cursor) rows.push(r);
  const cost = cursor.cost;
  cursor.close();
  return { rows, cost };
}

// Render the exact physical plan held in the private cache. Public EXPLAIN plans afresh, so this is
// the white-box comparison required to prove a refilled cache made the same choice as an
// independently fresh prepared statement.
function cachedExplain(db: Engine, stmt: PreparedLike): unknown[] {
  const render = new ExplainRender();
  const engine = db as unknown as {
    renderSelectPlan(r: ExplainRender, sp: unknown, depth: number): void;
  };
  engine.renderSelectPlan(render, cacheOf(stmt)!.sp, 0);
  return render.rows;
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

  const r1 = drain(db, stmt, [intValue(3n)]);
  assert.deepEqual(r1.rows, [[intValue(3n), intValue(300n)]]);
  const cached = cacheOf(stmt);
  assert.notEqual(cached, null, "cache should fill on the first cacheable execute");
  const sp = cached!.sp;

  const r2 = drain(db, stmt, [intValue(3n)]);
  assert.deepEqual(r2.rows, [[intValue(3n), intValue(300n)]]);
  assert.equal(cacheOf(stmt)!.sp, sp, "the cached plan object changed — statement re-planned");
  assert.equal(r2.cost, r1.cost, "reusing the cached plan must be cost-identical");

  // Different param binds against the same cached plan.
  const r3 = drain(db, stmt, [intValue(5n)]);
  assert.deepEqual(r3.rows, [[intValue(5n), intValue(500n)]]);
  assert.equal(cacheOf(stmt)!.sp, sp, "plan object changed on a param-only change");

  // A no-match param.
  assert.deepEqual(drain(db, stmt, [intValue(999n)]).rows, []);
});

test("insert cache: estimator revisions do not invalidate the immutable plan", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE ins (id i32 PRIMARY KEY, v i32)");
  const stmt = prepare(db, "INSERT INTO ins VALUES ($1, $2) RETURNING id, v");

  const first = drain(db, stmt, [intValue(1n), intValue(10n)]);
  assert.deepEqual(first.rows, [[intValue(1n), intValue(10n)]]);
  const cached = insertCacheOf(stmt);
  assert.notEqual(cached, null);
  const plan = cached!.plan;

  const second = drain(db, stmt, [intValue(2n), intValue(20n)]);
  assert.deepEqual(second.rows, [[intValue(2n), intValue(20n)]]);
  assert.equal(insertCacheOf(stmt)!.plan, plan, "successful INSERT revision caused a re-plan");
  assert.equal(second.cost, first.cost);
});

test("insert cache: row-only transaction fills then hits", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  const stmt = prepare(db, "INSERT INTO t VALUES ($1, $2)");
  execute(db, "BEGIN");
  drain(db, stmt, [intValue(1n), intValue(10n)]);
  const plan = insertCacheOf(stmt)!.plan;
  drain(db, stmt, [intValue(2n), intValue(20n)]);
  assert.equal(insertCacheOf(stmt)!.plan, plan);
  execute(db, "ROLLBACK");
});

test("insert cache: Transaction.executePrepared fills then hits", () => {
  const db = memDb();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  const stmt = db.prepareStatement("INSERT INTO t VALUES ($1, $2)");
  const session = db.session({});

  session.update((tx) => {
    tx.executePrepared(stmt, [intValue(1n), intValue(10n)]);
    const plan = insertCacheOf(stmt)!.plan;
    tx.executePrepared(stmt, [intValue(2n), intValue(20n)]);
    assert.equal(insertCacheOf(stmt)!.plan, plan);
  });

  session.close();
  db.close();
});

test("insert cache: mutable and complex shapes remain uncached", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, note text)");
  const regex = prepare(db, "INSERT INTO t VALUES ($1, $2) RETURNING note ~ 'a'");
  const first = drain(db, regex, [intValue(1n), textValue("abc")]);
  const second = drain(db, regex, [intValue(2n), textValue("abd")]);
  assert.equal(insertCacheOf(regex), null);
  assert.equal(first.cost, second.cost);

  const subquery = prepare(db, "INSERT INTO t VALUES ($1, $2) RETURNING (SELECT max(id) FROM t)");
  drain(db, subquery, [intValue(3n), textValue("x")]);
  assert.equal(insertCacheOf(subquery), null);

  for (const sql of [
    "INSERT INTO t SELECT 4, 'select'",
    "INSERT INTO t VALUES (4, 'conflict') ON CONFLICT DO NOTHING",
  ]) {
    const stmt = prepare(db, sql);
    drain(db, stmt);
    assert.equal(insertCacheOf(stmt), null, sql);
  }
});

test("insert cache: catalog, database, attachment, and temp identities invalidate", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  const stmt = prepare(db, "INSERT INTO t VALUES ($1, $2)");
  drain(db, stmt, [intValue(1n), intValue(10n)]);
  let plan = insertCacheOf(stmt)!.plan;

  // The documented database-wide catGen policy deliberately misses on unrelated DDL.
  execute(db, "CREATE TABLE unrelated (id i32 PRIMARY KEY)");
  drain(db, stmt, [intValue(2n), intValue(20n)]);
  assert.notEqual(insertCacheOf(stmt)!.plan, plan);
  plan = insertCacheOf(stmt)!.plan;

  execute(db, "CREATE INDEX t_v_idx ON t (v)");
  drain(db, stmt, [intValue(3n), intValue(30n)]);
  assert.notEqual(insertCacheOf(stmt)!.plan, plan, "index DDL did not invalidate");
  plan = insertCacheOf(stmt)!.plan;

  // A working catalog may use but never replace the committed slot. Rollback restores the old hit.
  execute(db, "BEGIN");
  execute(db, "DROP INDEX t_v_idx");
  drain(db, stmt, [intValue(4n), intValue(40n)]);
  assert.equal(insertCacheOf(stmt)!.plan, plan);
  execute(db, "ROLLBACK");
  drain(db, stmt, [intValue(5n), intValue(50n)]);
  assert.equal(insertCacheOf(stmt)!.plan, plan);

  execute(db, "DROP TABLE t");
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  drain(db, stmt, [intValue(9n), intValue(90n)]);
  assert.notEqual(insertCacheOf(stmt)!.plan, plan, "DROP/CREATE served a stale target plan");

  // Same generation and shape on another core must still miss and refill under that core identity.
  const other = new Engine();
  execute(other, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  const beforeOther = insertCacheOf(stmt)!.plan;
  drain(other, stmt, [intValue(1n), intValue(100n)]);
  assert.notEqual(insertCacheOf(stmt)!.plan, beforeOther, "cross-database false hit");

  const shared = memDb();
  shared.attach("aux", attachMemory(), false);
  const s = shared.session({});
  s.execute("CREATE TABLE aux.t (id i32 PRIMARY KEY, v i32)");
  const attached = shared.prepareStatement("INSERT INTO aux.t VALUES ($1, $2)");
  s.executePrepared(attached, [intValue(1n), intValue(10n)]);
  const attachedPlan = insertCacheOf(attached)!.plan;
  shared.detach("aux");
  shared.attach("aux", attachMemory(), false);
  s.execute("CREATE TABLE aux.t (id i32 PRIMARY KEY, v i32)");
  s.executePrepared(attached, [intValue(2n), intValue(20n)]);
  assert.notEqual(insertCacheOf(attached)!.plan, attachedPlan, "re-attached database false hit");
  s.close();
  shared.close();

  const shadowed = memDb();
  const b = shadowed.session({});
  b.execute("CREATE TEMP TABLE t (id i32 PRIMARY KEY, v i32)");
  const a = shadowed.session({});
  a.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  const shadowStmt = shadowed.prepareStatement("INSERT INTO t VALUES ($1, $2)");
  a.executePrepared(shadowStmt, [intValue(1n), intValue(10n)]);
  const persistentPlan = insertCacheOf(shadowStmt)!.plan;
  b.executePrepared(shadowStmt, [intValue(1n), intValue(111n)]);
  assert.equal(insertCacheOf(shadowStmt)!.plan, persistentPlan, "temp execution replaced cache");
  a.executePrepared(shadowStmt, [intValue(2n), intValue(20n)]);
  assert.equal(insertCacheOf(shadowStmt)!.plan, persistentPlan, "temp shadow poisoned old hit");
  a.close();
  b.close();
  shadowed.close();
});

test("insert cache: every execute rechecks privilege and read-only gates", () => {
  const db = memDb();
  const writer = db.session({});
  writer.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  const stmt = db.prepareStatement("INSERT INTO t VALUES ($1, $2)");
  writer.executePrepared(stmt, [intValue(1n), intValue(10n)]);
  const plan = insertCacheOf(stmt)!.plan;

  const restricted = db.session({ defaultPrivileges: PrivilegeSet.empty().with("select") });
  assert.deepEqual(
    caught(() => restricted.executePrepared(stmt, [intValue(2n), intValue(20n)])).code,
    "42501",
  );
  assert.equal(insertCacheOf(stmt)!.plan, plan);

  assert.equal(
    caught(() => writer.view((tx) => tx.executePrepared(stmt, [intValue(2n), intValue(20n)]))).code,
    "25006",
  );
  assert.equal(insertCacheOf(stmt)!.plan, plan);
  restricted.close();
  writer.close();
  db.close();
});

test("insert cache: collation upgrade invalidates resolved collation identities", () => {
  loadUnicodeData(readFileSync(specPath("collation/fixtures/unicode.jucd")));
  const db = new Engine();
  execute(db, 'CREATE TABLE t (id i32 PRIMARY KEY, x text COLLATE "unicode")');
  const stmt = prepare(db, "INSERT INTO t VALUES ($1, $2)");
  drain(db, stmt, [intValue(1n), textValue("a")]);
  const plan = insertCacheOf(stmt)!.plan;
  const loaded = loadedCollation("unicode")!;
  db.committed.collations.set("unicode", { ...loaded, unicodeVersion: "0.0.0" });
  assert.equal(db.upgradeCollations(), 1);
  drain(db, stmt, [intValue(2n), textValue("b")]);
  assert.notEqual(insertCacheOf(stmt)!.plan, plan);
});

test("insert cache: hit and fresh resolution are result, error, cost, and byte identical", () => {
  const cachedDb = new Engine();
  const freshDb = new Engine();
  const schema = "CREATE TABLE t (id i32 PRIMARY KEY, v i32 CHECK (v > 0), note text DEFAULT 'x')";
  execute(cachedDb, schema);
  execute(freshDb, schema);
  execute(cachedDb, "CREATE UNIQUE INDEX t_v_idx ON t (v)");
  execute(freshDb, "CREATE UNIQUE INDEX t_v_idx ON t (v)");
  const stmt = prepare(cachedDb, "INSERT INTO t (id, v) VALUES ($1, $2) RETURNING id, note");

  drain(cachedDb, stmt, [intValue(1n), intValue(10n)]);
  execute(freshDb, "INSERT INTO t (id, v) VALUES (1, 10) RETURNING id, note");
  const plan = insertCacheOf(stmt)!.plan;
  const hit = drain(cachedDb, stmt, [intValue(2n), intValue(20n)]);
  const fresh = execute(freshDb, "INSERT INTO t (id, v) VALUES (2, 20) RETURNING id, note");
  assert.equal(fresh.kind, "query");
  assert.equal(insertCacheOf(stmt)!.plan, plan);
  assert.deepEqual(hit.rows, fresh.kind === "query" ? fresh.rows : []);
  assert.equal(hit.cost, fresh.cost);
  assert.deepEqual(toImage(cachedDb, 8192, 1n), toImage(freshDb, 8192, 1n));

  const hitErr = caught(() => drain(cachedDb, stmt, [intValue(2n), intValue(20n)]));
  const freshErr = caught(() =>
    execute(freshDb, "INSERT INTO t (id, v) VALUES (2, 20) RETURNING id, note"),
  );
  assert.deepEqual(hitErr, freshErr);
  assert.deepEqual(toImage(cachedDb, 8192, 1n), toImage(freshDb, 8192, 1n));
});

test("plan cache: estimator revision tracks relevant relations only", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE a (id i32 PRIMARY KEY, v i32)");
  execute(db, "CREATE TABLE b (id i32 PRIMARY KEY, v i32)");
  execute(db, "INSERT INTO a VALUES (1, 10)");
  execute(db, "INSERT INTO b VALUES (1, 10)");
  const stmt = prepare(db, "SELECT id FROM a WHERE v = $1");
  drain(db, stmt, [intValue(10n)]);
  const first = cacheOf(stmt)!;
  const firstPlan = first.sp;
  const firstRevision = first.inputs[0]!.revision;

  execute(db, "INSERT INTO b VALUES (2, 20)");
  drain(db, stmt, [intValue(10n)]);
  assert.equal(cacheOf(stmt)!.sp, firstPlan, "unrelated row count invalidated the plan");

  execute(db, "INSERT INTO a VALUES (2, 20)");
  execute(db, "DELETE FROM a WHERE id = 2");
  const refilled = drain(db, stmt, [intValue(10n)]);
  assert.notEqual(cacheOf(stmt)!.sp, firstPlan, "referenced relation did not re-plan");
  assert.notEqual(
    cacheOf(stmt)!.inputs[0]!.revision,
    firstRevision,
    "equal row count falsely reused the old revision",
  );
  const fresh = prepare(db, "SELECT id FROM a WHERE v = $1");
  const freshRun = drain(db, fresh, [intValue(10n)]);
  assert.deepEqual(refilled.rows, freshRun.rows);
  assert.equal(refilled.cost, freshRun.cost, "refilled and fresh actual cost must match");
  assert.deepEqual(cachedExplain(db, stmt), cachedExplain(db, fresh));

  // P9 conservatively advances the target revision even for a successful zero-row disposition,
  // retaining its facts as stale.
  const beforeNoop = cacheOf(stmt)!.sp;
  execute(db, "INSERT INTO a VALUES (1, 99) ON CONFLICT DO NOTHING");
  drain(db, stmt, [intValue(10n)]);
  assert.notEqual(
    cacheOf(stmt)!.sp,
    beforeNoop,
    "ON CONFLICT DO NOTHING did not conservatively invalidate the target",
  );
  for (const [sql, param] of [
    ["UPDATE a SET v = 11 WHERE id = 1", 11n],
    ["INSERT INTO a SELECT 2, 20", 11n],
    ["INSERT INTO a VALUES (1, 12) ON CONFLICT (id) DO UPDATE SET v = excluded.v", 12n],
    ["DELETE FROM a WHERE id = 2", 12n],
  ] as const) {
    const before = cacheOf(stmt)!.sp;
    execute(db, sql);
    drain(db, stmt, [intValue(param)]);
    assert.notEqual(cacheOf(stmt)!.sp, before, `row mutation did not invalidate: ${sql}`);
  }
});

test("plan cache: ANALYZE invalidates only its target relation", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE a (id i32 PRIMARY KEY, v i32)");
  execute(db, "CREATE INDEX a_v_idx ON a (v)");
  execute(
    db,
    "INSERT INTO a VALUES (1,0),(2,0),(3,0),(4,0),(5,0),(6,0),(7,0),(8,0),(9,1),(10,NULL)",
  );
  execute(db, "CREATE TABLE b (id i32 PRIMARY KEY, v i32)");
  execute(db, "INSERT INTO b VALUES (1, 1)");
  const stmt = prepare(db, "SELECT id FROM a WHERE v = 0");
  drain(db, stmt);
  const initial = cacheOf(stmt)!.sp;

  execute(db, "ANALYZE b");
  drain(db, stmt);
  assert.equal(cacheOf(stmt)!.sp, initial, "unrelated ANALYZE invalidated the plan");

  execute(db, "ANALYZE a (v)");
  const refilled = drain(db, stmt);
  assert.notEqual(cacheOf(stmt)!.sp, initial, "target ANALYZE did not invalidate the plan");
  const fresh = prepare(db, "SELECT id FROM a WHERE v = 0");
  const freshRun = drain(db, fresh);
  assert.deepEqual(refilled.rows, freshRun.rows);
  assert.equal(refilled.cost, freshRun.cost);
  assert.deepEqual(cachedExplain(db, stmt), cachedExplain(db, fresh));
});

test("plan cache: rollback restores committed estimator signature", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  execute(db, "INSERT INTO t VALUES (1, 10)");
  const stmt = prepare(db, "SELECT id FROM t WHERE v = $1");
  drain(db, stmt, [intValue(10n)]);
  const committed = cacheOf(stmt)!;

  execute(db, "BEGIN");
  execute(db, "INSERT INTO t VALUES (2, 10)");
  assert.deepEqual(drain(db, stmt, [intValue(10n)]).rows, [[intValue(1n)], [intValue(2n)]]);
  assert.equal(cacheOf(stmt), committed, "working statistics replaced the committed cache entry");
  execute(db, "ROLLBACK");
  assert.deepEqual(drain(db, stmt, [intValue(10n)]).rows, [[intValue(1n)]]);
  assert.equal(cacheOf(stmt), committed, "rollback did not restore the committed cache hit");
});

test("plan cache: attachment has an independent estimator signature", () => {
  const db = memDb();
  db.attach("aux", attachMemory(), false);
  const s = db.session({});
  s.execute("CREATE TABLE aux.t (id i32 PRIMARY KEY, v i32)");
  s.execute("INSERT INTO aux.t VALUES (1, 10)");
  const stmt = db.prepareStatement("SELECT id FROM aux.t WHERE v = $1");
  const run = () => {
    const cursor = s.queryPrepared(stmt, [intValue(10n)]);
    const rows = [...cursor];
    const cost = cursor.cost;
    cursor.close();
    return { rows, cost };
  };
  run();
  const firstPlan = cacheOf(stmt)!.sp;
  s.execute("CREATE TABLE local_only (id i32 PRIMARY KEY)");
  run();
  assert.equal(cacheOf(stmt)!.sp, firstPlan, "main DDL invalidated attachment-only plan");

  s.execute("INSERT INTO aux.t VALUES (2, 10)");
  const refilled = run();
  assert.notEqual(cacheOf(stmt)!.sp, firstPlan, "attachment mutation did not re-plan");
  const fresh = db.prepareStatement("SELECT id FROM aux.t WHERE v = $1");
  const freshCursor = s.queryPrepared(fresh, [intValue(10n)]);
  const freshRows = [...freshCursor];
  const freshCost = freshCursor.cost;
  freshCursor.close();
  assert.deepEqual(refilled.rows, freshRows);
  assert.equal(refilled.cost, freshCost);

  // Replacing an attachment at the same name must reject the old cache entry even if the new
  // database reaches the same catalog generation and has the same table shape.
  const old = cacheOf(stmt)!;
  db.detach("aux");
  db.attach("aux", attachMemory(), false);
  s.execute("CREATE TABLE aux.t (id i32 PRIMARY KEY, v i32)");
  s.execute("INSERT INTO aux.t VALUES (9, 10), (10, 10)");
  const replacedRows = run().rows;
  const replaced = cacheOf(stmt)!;
  assert.notEqual(replaced.inputs[0]!.database, old.inputs[0]!.database);
  assert.notEqual(replaced.sp, old.sp);
  assert.deepEqual(replacedRows, [[intValue(9n)], [intValue(10n)]]);
  s.close();
  db.close();
});

// DROP + re-CREATE with a different shape bumps the catalog generation, so the next execute re-plans
// and reflects the new column set — a stale cached plan would return the old shape.
test("plan cache: DROP/CREATE invalidates", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, a i32)");
  execute(db, "INSERT INTO t VALUES (1, 10)");
  const stmt = prepare(db, "SELECT * FROM t WHERE id = $1");

  const r1 = drain(db, stmt, [intValue(1n)]);
  assert.deepEqual(r1.rows, [[intValue(1n), intValue(10n)]]);
  const gen1 = cacheOf(stmt)!.inputs[0]!.catGen;

  execute(db, "DROP TABLE t");
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, a i32, c i32)");
  execute(db, "INSERT INTO t VALUES (1, 10, 20)");

  const r2 = drain(db, stmt, [intValue(1n)]);
  assert.deepEqual(
    r2.rows,
    [[intValue(1n), intValue(10n), intValue(20n)]],
    "a stale 2-column plan was served after DROP/CREATE",
  );
  assert.notEqual(
    cacheOf(stmt)!.inputs[0]!.catGen,
    gen1,
    "catGen did not advance after DROP/CREATE",
  );
});

// CREATE INDEX between executes invalidates the cached full-scan plan; the re-plan picks up the new
// secondary index (cheaper cost). DROP INDEX reverses it.
test("plan cache: index DDL invalidates", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, a i32)");
  for (let i = 1; i <= 50; i++) execute(db, `INSERT INTO t VALUES (${i}, ${i})`);
  const stmt = prepare(db, "SELECT id FROM t WHERE a = $1");

  const scan = drain(db, stmt, [intValue(25n)]);
  assert.deepEqual(scan.rows, [[intValue(25n)]]);

  execute(db, "CREATE INDEX t_a ON t (a)");
  const idx = drain(db, stmt, [intValue(25n)]);
  assert.deepEqual(idx.rows, [[intValue(25n)]]);
  assert.ok(
    idx.cost < scan.cost,
    `expected index lookup cheaper than full scan after CREATE INDEX: idx=${idx.cost} scan=${scan.cost} (cached full-scan plan served?)`,
  );

  execute(db, "DROP INDEX t_a");
  const scan2 = drain(db, stmt, [intValue(25n)]);
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

  const r1 = drain(db, stmt);
  assert.deepEqual(r1.rows, [[intValue(1n)], [intValue(3n)]]);
  assert.equal(cacheOf(stmt), null, "a precompiled-regex plan must not be cached");
  const r2 = drain(db, stmt);
  assert.equal(r2.cost, r1.cost, "regex cost drifted across executes (regex plan wrongly cached?)");
});

// A plan with an uncorrelated subquery is never cached; results stay correct across executes.
test("plan cache: subquery plan is not cached, stays correct", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, a i32)");
  execute(db, "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)");
  const stmt = prepare(db, "SELECT id FROM t WHERE id = (SELECT max(id) FROM t)");

  assert.deepEqual(drain(db, stmt).rows, [[intValue(3n)]]);
  assert.equal(cacheOf(stmt), null, "a subquery plan must not be cached");
  // Insert a larger id; the (uncached, re-planned + re-evaluated) subquery must reflect it.
  execute(db, "INSERT INTO t VALUES (4, 40)");
  assert.deepEqual(drain(db, stmt).rows, [[intValue(4n)]]);
});

// --- The converged shared-core surface (spec/design/api.md §2.4): a statement is a standalone value
// run via the handles' prepareStatement / queryPrepared / executePrepared, shared across sessions. ---

// A statement is a standalone value: a plan filled on one session is reused (same plan object, same
// cost) by a different session over the same Database.
test("plan cache: statement shared across sessions", () => {
  const db = memDb();
  const a = db.session({});
  a.execute("CREATE TABLE orders (id i32 PRIMARY KEY, amount i32)");
  for (let i = 1; i <= 5; i++) a.execute(`INSERT INTO orders VALUES (${i}, ${i * 100})`);
  const stmt = db.prepareStatement("SELECT id, amount FROM orders WHERE id = $1");

  const drainOn = (s: typeof a, params: Value[]) => {
    const cursor = s.queryPrepared(stmt, params);
    const rows: Value[][] = [];
    for (const r of cursor) rows.push(r);
    const cost = cursor.cost;
    cursor.close();
    return { rows, cost };
  };

  const ra = drainOn(a, [intValue(3n)]);
  assert.deepEqual(ra.rows, [[intValue(3n), intValue(300n)]]);
  const cached = cacheOf(stmt);
  assert.notEqual(cached, null, "cache should fill on session A");

  const b = db.session({});
  const rb = drainOn(b, [intValue(3n)]);
  assert.deepEqual(rb.rows, [[intValue(3n), intValue(300n)]]);
  assert.equal(cacheOf(stmt)!.sp, cached!.sp, "session B re-planned — the plan must be shared");
  assert.equal(rb.cost, ra.cost, "cross-session reuse must be cost-identical");
  a.close();
  b.close();
});

// A statement executed against a DIFFERENT Database must not falsely hit: catGen is only monotonic
// within one core, so two databases can sit at the same generation with different schemas. The
// entry's core identity forces a re-plan against the other database.
test("plan cache: distinct databases never false-hit", () => {
  const db1 = memDb();
  const db2 = memDb();
  // One CREATE each → both cores sit at the SAME catalog generation with different table shapes.
  db1.execute("CREATE TABLE t (id i32 PRIMARY KEY, a i32)");
  db2.execute("CREATE TABLE t (id i32 PRIMARY KEY, a i32, b i32)");
  db1.execute("INSERT INTO t VALUES (1, 10)");
  db2.execute("INSERT INTO t VALUES (1, 10, 20)");

  const stmt = db1.prepareStatement("SELECT * FROM t WHERE id = $1");
  const r1 = db1.queryPrepared(stmt, [intValue(1n)]);
  assert.deepEqual([...r1], [[intValue(1n), intValue(10n)]]);
  r1.close();

  // Same catGen, different core: a false hit would serve db1's 2-column plan against db2.
  const r2 = db2.queryPrepared(stmt, [intValue(1n)]);
  assert.deepEqual(
    [...r2],
    [[intValue(1n), intValue(10n), intValue(20n)]],
    "stale cross-database plan served?",
  );
  r2.close();
});

// A plan cached where a relation name is persistent must not be served on a session whose
// session-local temp table shadows that name — the hit path re-checks the plan's relations against
// the executing session's temp domain and re-plans.
test("plan cache: temp shadow re-plans", () => {
  const db = memDb();
  // Session B creates its temp table FIRST (a temp name may not shadow an existing persistent table,
  // but a later persistent CREATE in another session cannot see B's temp domain).
  const b = db.session({});
  b.execute("CREATE TEMP TABLE t (id i32 PRIMARY KEY, v i32)");
  b.execute("INSERT INTO t VALUES (1, 111)");

  const a = db.session({});
  a.execute("CREATE TABLE t (id i32 PRIMARY KEY, a i32, b i32)");
  a.execute("INSERT INTO t VALUES (1, 10, 20)");

  const stmt = db.prepareStatement("SELECT * FROM t WHERE id = $1");
  const ra = a.queryPrepared(stmt, [intValue(1n)]);
  assert.deepEqual([...ra], [[intValue(1n), intValue(10n), intValue(20n)]]);
  ra.close();
  assert.notEqual(cacheOf(stmt), null, "cache should fill on the persistent session");

  // Session B: same core, same catGen — but t resolves temp-first there. The cached persistent plan
  // must not be served (and B's temp plan is never cached).
  const rb = b.queryPrepared(stmt, [intValue(1n)]);
  assert.deepEqual(
    [...rb],
    [[intValue(1n), intValue(111n)]],
    "stale persistent plan served on a temp-shadowed session?",
  );
  rb.close();

  // Back on A the persistent plan still serves (B's run did not poison the cache).
  const ra2 = a.queryPrepared(stmt, [intValue(1n)]);
  assert.deepEqual([...ra2], [[intValue(1n), intValue(10n), intValue(20n)]]);
  ra2.close();
  a.close();
  b.close();
});

// The Transaction handle runs prepared statements too (the converged trio): read-your-writes inside
// the block, and the same statement value works before, during, and after.
test("plan cache: transaction runs prepared statements", () => {
  const db = memDb();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  const insert = db.prepareStatement("INSERT INTO t VALUES ($1, $2)");
  const select = db.prepareStatement("SELECT v FROM t WHERE id = $1");

  assert.equal(db.executePrepared(insert, [intValue(1n), intValue(100n)]).changes, 1);
  const s = db.session({});
  s.update((tx) => {
    assert.equal(tx.executePrepared(insert, [intValue(2n), intValue(200n)]).changes, 1);
    const rows = tx.queryPrepared(select, [intValue(2n)]);
    assert.deepEqual([...rows], [[intValue(200n)]], "in-tx prepared read-your-writes");
    rows.close();
  });
  const after = s.queryPrepared(select, [intValue(2n)]);
  assert.deepEqual([...after], [[intValue(200n)]], "the block committed");
  after.close();
  s.close();
});
