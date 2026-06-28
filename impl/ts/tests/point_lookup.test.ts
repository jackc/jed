// Primary-key predicate pushdown over a MULTI-LEAF B-tree (spec/design/cost.md §3 "bounded scan").
// The conformance corpus (spec/conformance/suites/query/point_lookup.test) pins the cost contract on
// single-leaf tables; this exercises what it cannot — the bounded-scan primitive where page_read drops
// below node_count, and a range scan that spans leaf boundaries.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Engine, execute, intValue } from "../src/tooling.ts";
import { PMap, unboundedBound } from "../src/pmap.ts";
import type { KeyBound } from "../src/pmap.ts";
import type { Row } from "../src/storage.ts";
import type { Value } from "../src/value.ts";

// --- direct storage-primitive check (the page_read drop) ---

const CAP = 240;
const W = 15;

function key(n: number): Uint8Array {
  const b = new Uint8Array(8);
  new DataView(b.buffer).setBigUint64(0, BigInt(n));
  return b;
}

function pnodeRow(n: number): Row {
  return [intValue(BigInt(n))];
}

test("bounded range + overlap over a multi-leaf tree", () => {
  const m = new PMap();
  for (let n = 0; n < 200; n++) m.insert(key(n), pnodeRow(n), W, CAP, null);
  assert.ok(m.nodeCount() > 1, "test needs a multi-leaf tree");

  // A point bound visits strictly fewer nodes than the whole tree (the page_read win).
  const pb: KeyBound = { lo: key(100), loInc: true, hi: key(100), hiInc: true };
  assert.ok(m.overlapNodeCount(pb) < m.nodeCount());
  assert.strictEqual(m.rangeEntries(pb, null).vals.length, 1);

  // An inclusive range spanning many leaves returns exactly those entries (50..=150 = 101).
  const rb: KeyBound = { lo: key(50), loInc: true, hi: key(150), hiInc: true };
  assert.strictEqual(m.rangeEntries(rb, null).vals.length, 101);

  // Exclusive endpoints drop both boundary keys (51..=149 = 99).
  const ex: KeyBound = { lo: key(50), loInc: false, hi: key(150), hiInc: false };
  assert.strictEqual(m.rangeEntries(ex, null).vals.length, 99);

  // Half-open (>= 195) reaches the end of the key space (195..=199 = 5).
  const hiOpen: KeyBound = { lo: key(195), loInc: true, hi: null, hiInc: false };
  assert.strictEqual(m.rangeEntries(hiOpen, null).vals.length, 5);

  // The unbounded bound reproduces the full scan exactly.
  const unb = unboundedBound();
  assert.strictEqual(m.overlapNodeCount(unb), m.nodeCount());
  assert.strictEqual(m.rangeEntries(unb, null).vals.length, 200);
});

// --- end-to-end (public API): correctness across leaves + sublinear cost ---

function bigTable(n: number): Engine {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  const parts: string[] = [];
  for (let i = 1; i <= n; i++) parts.push(`(${i},${i})`);
  execute(db, "INSERT INTO t VALUES " + parts.join(","));
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
  return o.rows.map((r) => intOf(r[0]));
}

test("point lookup is sublinear", () => {
  const db = bigTable(1000);
  assert.deepStrictEqual(ids(db, "SELECT v FROM t WHERE id = 500"), [500]);
  const point = cost(db, "SELECT v FROM t WHERE id = 500");
  const full = cost(db, "SELECT v FROM t");
  assert.ok(point < full, `point cost ${point} should be far below full-scan ${full}`);
  assert.ok(point <= 50n, `point cost ${point} should be small (≈ height + a few), not ~1000`);
  assert.deepStrictEqual(ids(db, "SELECT v FROM t WHERE id = 99999"), []);
  const miss = cost(db, "SELECT v FROM t WHERE id = 99999");
  assert.ok(miss > 0n && miss <= 50n, `miss cost ${miss} should be small and non-zero`);
});

test("range scan crosses leaf boundaries", () => {
  const db = bigTable(1000);
  const got = ids(db, "SELECT id FROM t WHERE id >= 300 AND id <= 700 ORDER BY id");
  assert.strictEqual(got.length, 401);
  for (let i = 0; i < got.length; i++) assert.strictEqual(got[i], 300 + i);
  assert.deepStrictEqual(
    ids(db, "SELECT id FROM t WHERE id > 996 ORDER BY id"),
    [997, 998, 999, 1000],
  );
  // Contradictory bound: empty, cost 0.
  assert.deepStrictEqual(ids(db, "SELECT id FROM t WHERE id > 700 AND id < 300"), []);
  assert.strictEqual(cost(db, "SELECT id FROM t WHERE id > 700 AND id < 300"), 0n);
});

test("LIMIT short-circuit is sublinear", () => {
  const db = bigTable(1000); // id 1..1000, v == id
  // LIMIT without ORDER BY stops the scan early: `limit` rows at sublinear cost, the PK-order prefix.
  assert.deepStrictEqual(ids(db, "SELECT v FROM t LIMIT 5"), [1, 2, 3, 4, 5]);
  const point = cost(db, "SELECT v FROM t LIMIT 5");
  const full = cost(db, "SELECT v FROM t");
  assert.ok(point < full, `LIMIT cost ${point} should be far below full-scan ${full}`);
  assert.ok(
    point <= 20n,
    `LIMIT 5 cost ${point} should be sublinear (≈ limit + node count), not ~1000`,
  );
  assert.deepStrictEqual(ids(db, "SELECT v FROM t LIMIT 3 OFFSET 10"), [11, 12, 13]);

  // Trap windowing: streaming projects ONLY the windowed rows, so a later trapping row is never
  // reached under a LIMIT that excludes it.
  const dz = new Engine();
  execute(dz, "CREATE TABLE z (id i32 PRIMARY KEY, c i32)");
  execute(dz, "INSERT INTO z VALUES (1, 5), (2, 0), (3, 5)");
  assert.deepStrictEqual(ids(dz, "SELECT 100 / c FROM z LIMIT 1"), [20]);
  assert.throws(() => execute(dz, "SELECT 100 / c FROM z LIMIT 2"));
});

test("mutation pushdown is sublinear", () => {
  const db = bigTable(1000);
  const d = cost(db, "DELETE FROM t WHERE id = 500");
  assert.ok(d <= 50n, `DELETE point-lookup cost ${d} should be sublinear`);
  assert.deepStrictEqual(ids(db, "SELECT id FROM t WHERE id = 500"), []);
  assert.deepStrictEqual(ids(db, "SELECT id FROM t WHERE id = 501"), [501]);

  const u = cost(db, "UPDATE t SET v = -1 WHERE id = 700");
  assert.ok(u <= 50n, `UPDATE point-lookup cost ${u} should be sublinear`);
  assert.deepStrictEqual(ids(db, "SELECT v FROM t WHERE id = 700"), [-1]);
});
