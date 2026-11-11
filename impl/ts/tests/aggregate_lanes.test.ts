// A5 — the columnar aggregate lane widened (spec/design/packed-leaf.md §11 Track A5; memory.md §5.4,
// §6.6.1; cost.md §6.1; spill.md §4.1). The decimal folds (SUM over i64/decimal, AVG over an integer
// or decimal) take the lane; a grouped fold over a file-backed table takes it bounded, handing its
// group table over to the spill structures past work_mem and sorting its groups with the bounded-spill
// lane's own ORDER BY stage; under a cost ceiling the lane runs as an attempt the row path may replace;
// and a numeric(p,s) column fits the file-backed top-k heap (aggregate_lanes_topn.test.ts). The oracle
// is the row path: the same session under a ceiling it never reaches, with the lane's attempt switched
// off (aggregate_lanes_ceiling.test.ts sweeps the ceilings themselves). Mirrors Rust
// (tests/varwidth_lanes.rs) and Go (varwidth_lanes_test.go).

import assert from "node:assert/strict";
import { test } from "node:test";
import { DEFAULT_WORK_MEM } from "../src/spill.ts";
import { laneGroupHandovers } from "../src/tooling.ts";
import { check, checkUnderBudgets, GT_SEED, withDb } from "./aggregate_lanes_util.ts";

// ---- The decimal folds ----------------------------------------------------------------------

// The values span one to many base-10⁴ groups, mixed scales and signs, and sums that grow past 2^64
// and 2^128, so the decimal_work charge changes as each sum grows.
const DT_SEED = [
  "CREATE TABLE dt (id i32 PRIMARY KEY, n i32, k i64, big i64, d numeric(12,2), wide numeric)",
  // Small money-shaped values, a NULL in every column, and a NULL group key.
  "INSERT INTO dt VALUES " +
    "(-3, 1, NULL, 9223372036854775807, 0.01, 1), " +
    "(-2, 1, 1, -9223372036854775808, -0.01, 0.000000000000000000000000000001), " +
    "(-1, NULL, 2, NULL, NULL, NULL), " +
    "(0, 2, 2, 0, 0.00, 0.0)",
  "INSERT INTO dt SELECT g, g % 5, g % 3, " +
    "CASE WHEN g % 2 = 0 THEN -1 ELSE 1 END * g * 2305843009213693, " +
    "((g * 7919) % 100000000)::numeric / 100 - 250000, " +
    "CASE WHEN g % 11 = 0 THEN NULL " +
    "WHEN g % 7 = 0 THEN g * 123456789012345678901234567890.5 * 10000000000000000000000.0 " +
    "WHEN g % 4 = 0 THEN -(g::numeric / 1000000007) " +
    "ELSE g::numeric * 1.000001 END " +
    "FROM generate_series(1, 3000) g",
];

const DECIMAL_FOLD_QUERIES = [
  "SELECT sum(big) FROM dt",
  "SELECT sum(d) FROM dt",
  "SELECT sum(wide) FROM dt",
  "SELECT avg(big), avg(d), avg(wide), avg(n) FROM dt",
  "SELECT sum(big), sum(d), sum(wide), count(*), min(d), max(wide) FROM dt",
  "SELECT sum(d) FROM dt WHERE n > 2",
  "SELECT avg(d) FROM dt WHERE d > 0",
  "SELECT sum(big), avg(wide) FROM dt WHERE wide IS NOT NULL AND big < 0",
  "SELECT sum(d) FROM dt WHERE id < 0",
  "SELECT n, sum(big), sum(d), avg(d) FROM dt GROUP BY n",
  "SELECT n, sum(wide), avg(big), count(d) FROM dt WHERE d <> 0 GROUP BY n",
  "SELECT k, sum(d) FROM dt GROUP BY k",
];

test("decimal folds match the row path", () => {
  withDb(true, DT_SEED, (s) => {
    // Clean (Packed) leaves faulted from the file, then a writer's uncommitted (Decoded) leaves.
    for (const q of DECIMAL_FOLD_QUERIES) {
      check(s, q);
      checkUnderBudgets(s, q);
    }
    s.execute("BEGIN");
    s.execute("INSERT INTO dt VALUES (5000, 1, 1, 42, 123.45, 1e40)");
    s.execute("UPDATE dt SET d = d + 1, big = big / 2 WHERE n = 3");
    for (const q of DECIMAL_FOLD_QUERIES) check(s, q);
    s.execute("ROLLBACK");
  });
  // The same over an in-memory database.
  withDb(false, DT_SEED, (s) => {
    for (const q of DECIMAL_FOLD_QUERIES) {
      check(s, q);
      checkUnderBudgets(s, q);
    }
  });
});

// ---- The bounded grouped lane -----------------------------------------------------------------

// Each work_mem below places the hand-over somewhere else — after the first row, after a few groups,
// part-way through a many-group scan, never — and at each the lane must agree with the row path on
// rows and cost.
const GROUPED_HANDOVER_QUERIES = [
  // A few groups, every accumulator kind (fixed-size and size-changing), NULL keys.
  "SELECT n, count(*), count(d), sum(big), sum(d), avg(d), min(d), max(wide), min(s), max(s) FROM gt GROUP BY n",
  "SELECT k, sum(wide), avg(big), min(wide), max(big), count(s) FROM gt WHERE d <> 0 GROUP BY k",
  "SELECT n, sum(f), avg(f), sum(m) FROM gt GROUP BY n",
  // About one group per row.
  "SELECT big, sum(d), max(s), count(*) FROM gt GROUP BY big",
  "SELECT big, count(*), min(s) FROM gt WHERE n > 1 AND s <> 'v3' GROUP BY big",
  // Grouped by the primary key, which the lane does not take: the same answer all the same.
  "SELECT id, sum(big) FROM gt GROUP BY id",
  // No survivors, and a primary-key bound.
  "SELECT n, sum(d) FROM gt WHERE id < -100 GROUP BY n",
  "SELECT n, sum(d), max(s) FROM gt WHERE id BETWEEN 100 AND 900 GROUP BY n",
];

// Grouped folds with an ORDER BY over their groups — by the key, by an aggregate, by an expression of
// both — each a total order, so the row sequence is part of the comparison.
const ORDERED_GROUP_QUERIES = [
  "SELECT n, count(*), sum(d) FROM gt GROUP BY n ORDER BY n",
  "SELECT k, avg(big), max(wide) FROM gt GROUP BY k ORDER BY k DESC NULLS FIRST",
  "SELECT n, sum(d), max(s) FROM gt GROUP BY n ORDER BY sum(d) DESC NULLS LAST, n",
  "SELECT big, count(*), min(s) FROM gt GROUP BY big ORDER BY min(s) DESC, big LIMIT 9 OFFSET 3",
  "SELECT big, sum(d) FROM gt WHERE n > 1 GROUP BY big ORDER BY big % 7, big",
  "SELECT big, sum(d) FROM gt GROUP BY big ORDER BY sum(d) + big % 3 DESC, big LIMIT 40",
  "SELECT n, sum(d) FROM gt WHERE id < -100 GROUP BY n ORDER BY n",
  "SELECT n, max(s) FROM gt GROUP BY n ORDER BY n LIMIT 0",
];

test("the grouped lane hands over to the spill structures", () => {
  withDb(true, GT_SEED, (s) => {
    // work_mem of 1 hands over after the first survivor; 3,000 after a handful of groups and part-way
    // through the many-group scans; the default never.
    for (const workMem of [1, 3_000, DEFAULT_WORK_MEM]) {
      s.setWorkMem(workMem);
      for (const q of GROUPED_HANDOVER_QUERIES) {
        const before = laneGroupHandovers.count;
        check(s, q);
        // At the smallest budgets every query with a survivor hands over on its lane run.
        if (workMem <= 400 && !q.includes("id < -100") && !q.includes("GROUP BY id"))
          assert.ok(laneGroupHandovers.count > before, `work_mem ${workMem}: no hand-over: ${q}`);
      }
    }
    // An ORDER BY over the groups: sorted by the bounded-spill lane's own stage, whether or not the
    // fold handed over.
    for (const workMem of [1, 3_000, DEFAULT_WORK_MEM]) {
      s.setWorkMem(workMem);
      for (const q of ORDERED_GROUP_QUERIES) {
        const before = laneGroupHandovers.count;
        check(s, q);
        if (workMem <= 400 && !q.includes("id < -100"))
          assert.ok(laneGroupHandovers.count > before, `work_mem ${workMem}: no hand-over: ${q}`);
      }
    }
    // The default work_mem never hands over.
    {
      const before = laneGroupHandovers.count;
      for (const q of GROUPED_HANDOVER_QUERIES) check(s, q);
      assert.equal(laneGroupHandovers.count, before);
    }
    // LIMIT/OFFSET over a handed-over result: the same window of the same emission order.
    s.setWorkMem(3_000);
    for (const q of [
      "SELECT big, sum(d) FROM gt GROUP BY big LIMIT 7",
      "SELECT big, count(*) FROM gt GROUP BY big LIMIT 5 OFFSET 2990",
      "SELECT big, count(*) FROM gt GROUP BY big OFFSET 5000",
      "SELECT n, max(s) FROM gt GROUP BY n LIMIT 0",
    ])
      check(s, q);
    // A writer's uncommitted (Decoded) leaves, mid hand-over.
    s.execute("BEGIN");
    s.execute("INSERT INTO gt VALUES (5000, 9, 9, 42, 123.45, 1e40, 'zz', 3.5, 7)");
    s.execute("UPDATE gt SET d = d + 1, s = s || '!' WHERE n = 3");
    for (const workMem of [3_000, DEFAULT_WORK_MEM]) {
      s.setWorkMem(workMem);
      for (const q of GROUPED_HANDOVER_QUERIES) check(s, q);
    }
    s.execute("ROLLBACK");
    // Under a query-memory budget: the same rows and cost, or 54P05. (A sample of the statements and
    // budgets: a tiny budget makes every structure spill row by row.)
    s.setWorkMem(3_000);
    for (const q of [
      GROUPED_HANDOVER_QUERIES[0]!,
      GROUPED_HANDOVER_QUERIES[3]!,
      ORDERED_GROUP_QUERIES[2]!,
      ORDERED_GROUP_QUERIES[3]!,
    ])
      checkUnderBudgets(s, q, [64n, 64n * 1024n, 64n * 1024n * 1024n]);
  });
});
