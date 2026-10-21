import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { Engine, execute, prepare, queryPrepared, intValue } from "../src/tooling.ts";
import { nullValue, type Value } from "../src/value.ts";

const plan = JSON.parse(
  readFileSync(new URL("../../../bench/prepared-join/plan.json", import.meta.url), "utf8"),
);

test("prepared joins reuse value-blind plans beyond the custom memo capacity", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE items (id i32 PRIMARY KEY, bucket i32, score i32, label text)");
  execute(
    db,
    "INSERT INTO items SELECT CAST(g AS i32), CAST(g%10 AS i32), CAST((g*37)%10000 AS i32), 'row'||CAST(g AS text) FROM generate_series(0,999) AS g",
  );
  const run = (stmt: ReturnType<typeof prepare>, value: Value) => {
    const cursor = queryPrepared(db, stmt, [value]);
    const rows = [...cursor];
    const result = { rows, cost: cursor.cost, types: cursor.columnTypes };
    cursor.close();
    return result;
  };
  for (const w of plan.workloads) {
    const stmt = prepare(db, w.sql);
    const holder = (
      stmt as unknown as { scHolder: { cache: { sensitive: boolean; memo: unknown[] } | null } }
    ).scHolder;
    let entry: typeof holder.cache = null;
    for (let i = 0; i < 24; i++) {
      const value = intValue(BigInt(i * 17));
      assert.deepEqual(run(stmt, value), run(prepare(db, w.sql), value), w.name);
      assert.ok(holder.cache);
      if (i === 0) entry = holder.cache;
      assert.equal(holder.cache, entry);
      assert.equal(holder.cache.sensitive, false, w.name);
      assert.equal(holder.cache.memo.length, 0, w.name);
    }
    assert.deepEqual(run(stmt, nullValue()), run(prepare(db, w.sql), nullValue()), w.name);
    assert.equal(holder.cache, entry);
  }
  execute(db, "ANALYZE items");
  for (const w of plan.workloads.filter(
    (w: { name: string }) =>
      w.name === "indexed_join" ||
      w.name.startsWith("join_text") ||
      w.name === "join_integer_outputs",
  )) {
    const stmt = prepare(db, w.sql);
    for (const n of [17, 51, 999, 1000]) {
      const value = intValue(BigInt(n));
      assert.deepEqual(run(stmt, value), run(prepare(db, w.sql), value));
      const holder = (stmt as unknown as { scHolder: { cache: { sensitive: boolean } } }).scHolder;
      assert.equal(holder.cache.sensitive, false, w.name);
    }
  }
});

test("prepared join contradictions still depend on both literal bindings", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  execute(db, "INSERT INTO t VALUES (1),(2),(3)");
  const sql = "SELECT a.id,b.id FROM t a JOIN t b ON b.id=a.id WHERE a.id=$1 AND a.id=$2";
  const stmt = prepare(db, sql);
  const run = (query: ReturnType<typeof prepare>, values: Value[]) => {
    const rows = queryPrepared(db, query, values);
    const out = { rows: [...rows], cost: rows.cost };
    rows.close();
    return out;
  };
  for (const values of [
    [intValue(1n), intValue(1n)],
    [intValue(1n), intValue(2n)],
    [nullValue(), intValue(1n)],
  ]) {
    assert.deepEqual(run(stmt, values), run(prepare(db, sql), values));
  }
  const holder = (stmt as unknown as { scHolder: { cache: { sensitive: boolean } } }).scHolder;
  assert.equal(holder.cache.sensitive, true);
});
