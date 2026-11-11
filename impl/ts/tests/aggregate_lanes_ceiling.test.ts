// The columnar lanes under a cost ceiling (spec/design/cost.md §6.1): with a max_cost or a
// lifetime budget set a lane runs as an attempt, accepted only if it finishes cleanly with every
// ceiling unreached and otherwise discarded for the row path. So with the attempt on and off a
// statement must have the same outcome — rows and cost, or error code and text — and add the same
// amount to the session's cumulative cost, at every ceiling. And with no ceiling (§6.2) a statement
// that fails on a lane leaves the cumulative cost the row path leaves. Mirrors Rust
// (tests/varwidth_lanes.rs) and Go (varwidth_lanes_test.go).

import assert from "node:assert/strict";
import { test } from "node:test";
import { VOLATILE_SCALAR_FUNCS } from "../src/executor.ts";
import { OPERATORS } from "../src/operators.ts";
import { DEFAULT_WORK_MEM } from "../src/spill.ts";
import { EngineError, laneCeiling, type Session } from "../src/tooling.ts";
import { render } from "../src/value.ts";
import { GT_SEED, rowPath, withDb } from "./aggregate_lanes_util.ts";
import { queryOutcome } from "./util.ts";

// meteredOutcome is one statement's whole observable outcome under the session's current ceilings:
// its rows and cost, or its error code and text, and what it added to the session's cumulative cost.
function meteredOutcome(s: Session, sql: string): [string, bigint] {
  const before = s.lifetimeCost();
  let out: string;
  try {
    const o = queryOutcome(s, sql, []);
    if (o.kind !== "query") throw new Error(`expected a query: ${sql}`);
    const rows = o.rows.map((r) => r.map((v) => render(v)).join("\x1f"));
    if (!sql.includes("ORDER BY")) rows.sort();
    out = `cost ${o.cost}: ${rows.join("\n")}`;
  } catch (e) {
    if (!(e instanceof EngineError)) throw e;
    out = e.message;
  }
  return [out, s.lifetimeCost() - before];
}

// laneMatchesRowPath runs the same statement with the lane's attempt on and off: every observable
// must match.
function laneMatchesRowPath(s: Session, sql: string, what: string): void {
  laneCeiling.off = false;
  const lane = meteredOutcome(s, sql);
  laneCeiling.off = true;
  let row: [string, bigint];
  try {
    row = meteredOutcome(s, sql);
  } finally {
    laneCeiling.off = false;
  }
  assert.deepEqual(lane, row, `${what}: ${sql}`);
}

// rowPathSpend is what the row path alone spends on `sql` with a ceiling it never reaches (to an
// error, if it raises one): the top of the range of ceilings worth sweeping.
function rowPathSpend(s: Session, sql: string): bigint {
  return rowPath(s, () => meteredOutcome(s, sql)[1]);
}

// ceilings is every `step`-th ceiling from 1 to past `top`, and the boundary itself.
function ceilings(top: bigint, step: bigint): bigint[] {
  const out: bigint[] = [];
  for (let limit = 1n; limit <= top + 2n; limit += step) out.push(limit);
  for (let limit = top - 1n; limit <= top + 2n; limit++) if (limit > 0n) out.push(limit);
  return out;
}

// sweepMaxCost runs `sql` at every `step`-th max_cost from 1 to past its cost, and at the boundary.
// A negative step asks for about that many evenly spaced ceilings instead.
function sweepMaxCost(s: Session, sql: string, step: bigint): void {
  const top = rowPathSpend(s, sql);
  if (step < 0n) step = top / -step + 1n;
  try {
    for (const limit of ceilings(top, step)) {
      s.setMaxCost(limit);
      laneMatchesRowPath(s, sql, `max_cost ${limit}`);
    }
  } finally {
    s.setMaxCost(0n);
  }
}

// sweepLifetime runs `sql` under a lifetime budget with `left` units remaining, alone and beside a
// max_cost one either side of it (which ceiling's error wins is part of the outcome).
function sweepLifetime(s: Session, sql: string, step: bigint): void {
  const top = rowPathSpend(s, sql);
  for (const left of ceilings(top, step)) {
    for (const maxCost of [0n, left - 1n, left, left + 1n]) {
      if (maxCost < 0n) continue;
      // Each run draws on the budget, so it is re-armed `left` above the running total.
      const run = (off: boolean): [string, bigint] => {
        laneCeiling.off = off;
        s.setLifetimeMaxCost(s.lifetimeCost() + left);
        s.setMaxCost(maxCost);
        try {
          const [out, spent] = meteredOutcome(s, sql);
          // The lifetime error's text carries the session's absolute totals, which differ between
          // the two runs; its code and the cost it left accrued are what must match.
          return [out.startsWith("54P02") ? "54P02" : out, spent];
        } finally {
          s.setLifetimeMaxCost(0n);
          s.setMaxCost(0n);
          laneCeiling.off = false;
        }
      };
      const lane = run(false);
      const row = run(true);
      assert.deepEqual(lane, row, `lifetime +${left}, max_cost ${maxCost}: ${sql}`);
    }
  }
}

// A 300-row table small enough to try every ceiling: a NULL row, decimals of both signs, text.
const CT_SEED = [
  "CREATE TABLE ct (id i32 PRIMARY KEY, n i32, big i64, d numeric(12,2), s text)",
  "INSERT INTO ct VALUES (0, NULL, NULL, NULL, NULL)",
  "INSERT INTO ct SELECT g, g % 5, " +
    "CASE WHEN g % 2 = 0 THEN -1 ELSE 1 END * g * 2305843009213693, " +
    "((g * 7919) % 100000000)::numeric / 100 - 250000, " +
    "'v' || repeat('x', (g % 40)::i32) || (g % 97)::text " +
    "FROM generate_series(1, 300) g",
];

// Lane-shaped statements whose filters are safe to discard: whole-table and grouped folds, ordered
// groups, a window, no survivors, and filters that raise part-way through the scan. The first three
// are swept at every ceiling on a file; the rest at every twenty-ninth, always with the boundary
// ceilings (cost − 1 … cost + 2).
const CEILING_QUERIES = [
  "SELECT count(*), sum(d), min(s) FROM ct",
  "SELECT n, count(*), sum(d) FROM ct GROUP BY n",
  "SELECT count(*), sum(d) FROM ct WHERE 1000 / (id - 200) > 0",
  "SELECT sum(big), avg(d) FROM ct WHERE n > 1 AND d <> 0",
  "SELECT n, sum(d), max(s) FROM ct WHERE big % 3 <> 1 GROUP BY n ORDER BY sum(d) DESC, n",
  "SELECT n, count(*) FROM ct GROUP BY n ORDER BY n LIMIT 2 OFFSET 1",
  "SELECT big, count(*), max(d) FROM ct WHERE n IN (1, 3) GROUP BY big",
  "SELECT n, sum(d) FROM ct WHERE id < -5 GROUP BY n",
  "SELECT n, sum(d) FROM ct WHERE CASE WHEN id > 250 THEN 1000 / (id - 260) ELSE 1 END <> 7 GROUP BY n",
  "SELECT * FROM (SELECT n, sum(d) AS t FROM ct GROUP BY n) g WHERE t IS NOT NULL",
  // An uncorrelated scalar subquery is folded to a constant before the lane runs.
  "SELECT count(*), sum(d) FROM ct WHERE n > (SELECT 1)",
  // Built-in function calls the catalog does not mark volatile.
  "SELECT n, sum(d) FROM ct WHERE abs(n) > 1 GROUP BY n",
  "SELECT count(*), sum(d) FROM ct WHERE abs(n) > 1 AND lower(s) <> 'v' AND length(s) < 40",
  // Expression operands: a run that folds one is always an attempt.
  "SELECT sum(d * 2), count(big + 1), max(big % 7) FROM ct",
  "SELECT n, sum(d + id), min(abs(d)) FROM ct WHERE n > 0 GROUP BY n ORDER BY n",
  // An operand that raises at row 100 and a filter that raises at row 250: the row path filters
  // every row before it folds any, so the filter's error is the one it reports.
  "SELECT sum(1000 / (id - 100)) FROM ct WHERE 1000 / (id - 250) <> 7",
  "SELECT n, sum(1000 / (id - 100)) FROM ct GROUP BY n",
];

// Bare-column projections: the columnar projection lane, an attempt under a ceiling.
const PROJECTION_QUERIES = [
  "SELECT id, d, s FROM ct",
  "SELECT id, s FROM ct WHERE n > 1 AND d <> 0",
  "SELECT d, big FROM ct WHERE id BETWEEN 40 AND 200 AND abs(n) < 3",
  "SELECT id, d FROM ct WHERE 1000 / (id - 200) > 0",
];

// Statements whose filter is outside the allowlist, so they run on the plain lane with no ceiling and
// keep the row path under one — each failing part-way through its scan.
const PLAIN_LANE_FAILURES = [
  "SELECT count(*), sum(d) FROM ct WHERE s ~ '^v' AND 1000 / (id - 200) > 0",
  "SELECT n, max(s), sum(d) FROM ct WHERE uuidv4() IS NOT NULL AND 1000 / (id - 77) > 0 GROUP BY n",
  "SELECT id, s FROM ct WHERE s ~ '^v' AND 1000 / (id - 200) > 0",
];

// unmeteredMatchesRowPath: with no ceiling, the lane (plain, or an attempt for an expression operand)
// against the row path — the same statement under a ceiling it never reaches with the attempt off,
// which accrues exactly what the unmetered row path does. Rows and cost, or the error and what it
// left accrued.
function unmeteredMatchesRowPath(s: Session, sql: string): void {
  s.setMaxCost(0n);
  const lane = meteredOutcome(s, sql);
  const row = rowPath(s, () => meteredOutcome(s, sql));
  assert.deepEqual(lane, row, `unmetered lane vs row path: ${sql}`);
}

// cost.md §6.2: a statement that fails on a lane leaves the cumulative cost the row path leaves, and
// an expression-operand lane reports the error the row path reports.
function unmeteredLane(file: boolean): void {
  withDb(file, CT_SEED, (s) => {
    for (const workMem of file ? [1, 400, DEFAULT_WORK_MEM] : [DEFAULT_WORK_MEM]) {
      s.setWorkMem(workMem);
      for (const q of [...CEILING_QUERIES, ...PROJECTION_QUERIES, ...PLAIN_LANE_FAILURES])
        unmeteredMatchesRowPath(s, q);
    }
  });
}

test("the unmetered lane matches the row path on a file", () => {
  unmeteredLane(true);
});

test("the unmetered lane matches the row path in memory", () => {
  unmeteredLane(false);
});

function laneUnderACeiling(file: boolean): void {
  withDb(file, CT_SEED, (s) => {
    CEILING_QUERIES.forEach((q, qi) => {
      const before = { ...laneCeiling };
      sweepMaxCost(s, q, file ? (qi < 3 ? 1n : 29n) : 11n);
      const tried = laneCeiling.attempts - before.attempts;
      const fellBack = laneCeiling.fallbacks - before.fallbacks;
      assert.ok(tried > 0, `the lane was never attempted: ${q}`);
      // A statement that completes was accepted at its high ceilings and rejected at its low ones.
      assert.ok(fellBack > 0, `the lane never fell back: ${q}`);
      if (!q.includes("1000 /")) assert.ok(tried > fellBack, `the lane was never accepted: ${q}`);
      sweepLifetime(s, q, 397n);
    });
    // The projection lane under a ceiling.
    for (const q of PROJECTION_QUERIES) {
      const before = { ...laneCeiling };
      sweepMaxCost(s, q, file ? 29n : 11n);
      assert.ok(
        laneCeiling.attempts > before.attempts,
        `the projection lane was never attempted: ${q}`,
      );
      assert.ok(
        laneCeiling.fallbacks > before.fallbacks,
        `the projection lane never fell back: ${q}`,
      );
      sweepLifetime(s, q, 397n);
    }
    // The grouped fold handing over to the spill structures part-way (file only).
    if (file) {
      for (const workMem of [1, 400, 3_000]) {
        s.setWorkMem(workMem);
        for (const q of CEILING_QUERIES.filter((q) => q.includes("GROUP BY")))
          sweepMaxCost(s, q, 97n);
      }
      s.setWorkMem(DEFAULT_WORK_MEM);
    }
    // Not safe to discard — entropy, the per-call clock, a correlated subquery, a regex — so never
    // attempted.
    for (const q of [
      "SELECT count(*), sum(d) FROM ct WHERE uuidv4() IS NOT NULL",
      "SELECT n, sum(d) FROM ct WHERE clock_timestamp() IS NOT NULL GROUP BY n",
      "SELECT sum(d * 2) FROM ct WHERE s ~ '^v'",
      "SELECT id, d FROM ct WHERE s ~ '^v'",
      "SELECT count(*) FROM ct WHERE n > (SELECT count(*) FROM ct c WHERE c.id = ct.id)",
      "SELECT count(*) FROM ct WHERE s ~ '^vx+1'",
    ]) {
      const before = laneCeiling.attempts;
      sweepMaxCost(s, q, 97n);
      assert.equal(laneCeiling.attempts, before, `attempted though not safe to discard: ${q}`);
    }
    // With a query-memory budget too (one nothing here reaches), the attempt is still made.
    s.setMaxQueryMemoryBytes(64n * 1024n * 1024n);
    {
      const before = laneCeiling.attempts;
      for (const q of CEILING_QUERIES) sweepMaxCost(s, q, 97n);
      assert.ok(laneCeiling.attempts > before);
    }
    s.setMaxQueryMemoryBytes(0n);
    // A writer's uncommitted leaves, at a ceiling the statement fits under (an abort would end the
    // transaction).
    s.execute("BEGIN");
    s.execute("INSERT INTO ct VALUES (900, 9, 42, 123.45, 'zz')");
    s.execute("UPDATE ct SET d = d + 1, s = s || '!' WHERE n = 3");
    for (const q of CEILING_QUERIES.filter((q) => !q.includes("1000 /"))) {
      const top = rowPathSpend(s, q);
      s.setMaxCost(top + 1n);
      laneMatchesRowPath(s, q, "uncommitted writer");
      s.setMaxCost(0n);
    }
    s.execute("ROLLBACK");
  });
}

test("the lane under a ceiling matches the row path on a file", () => {
  laneUnderACeiling(true);
});

test("the lane under a ceiling matches the row path in memory", () => {
  laneUnderACeiling(false);
});

// A sampled sweep over the 3,000-row table: many groups, a mid-scan hand-over, wide decimals.
test("the lane under a ceiling matches the row path on a larger table", () => {
  withDb(true, GT_SEED, (s) => {
    for (const workMem of [3_000, DEFAULT_WORK_MEM]) {
      s.setWorkMem(workMem);
      for (const q of [
        "SELECT count(*), sum(wide), avg(big), max(s) FROM gt WHERE d <> 0",
        "SELECT big, sum(d), max(s), count(*) FROM gt GROUP BY big",
        "SELECT n, sum(d), max(s) FROM gt GROUP BY n ORDER BY sum(d) DESC NULLS LAST, n",
        "SELECT big, count(*) FROM gt WHERE n > 1 GROUP BY big ORDER BY big % 7, big LIMIT 9",
      ])
        sweepMaxCost(s, q, -8n);
    }
  });
});

// The lane's "volatile" list is the function catalog's (spec/functions/catalog.toml): every function
// it marks volatile, and nothing else. `gen_random_uuid` is uuidv4's other name.
test("the volatile scalar functions are the catalog's", () => {
  const listed = [...VOLATILE_SCALAR_FUNCS].sort();
  const catalog = [
    ...new Set(
      OPERATORS.filter((d) => d.volatility === "volatile" && d.name !== "gen_random_uuid").map(
        (d) => d.name,
      ),
    ),
  ].sort();
  assert.deepEqual(listed, catalog);
});
