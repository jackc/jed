// Shared helpers of the aggregate-lane tests (not a `*.test.ts` file, so the runner does not execute
// it): the lane-versus-row-path comparisons and the seeded databases they run over.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  createDatabase,
  EngineError,
  laneCeiling,
  openDatabase,
  type Session,
} from "../src/tooling.ts";
import { render } from "../src/value.ts";
import { queryOutcome } from "./util.ts";

export const NEVER = 1n << 60n;

export type Result = { rows: string; cost: bigint } | { error: string };

// outcome is one query's rows (rendered; sorted unless the query has an ORDER BY) and cost, or its
// error code.
export function outcome(s: Session, sql: string): Result {
  try {
    const o = queryOutcome(s, sql, []);
    if (o.kind !== "query") throw new Error(`expected a query: ${sql}`);
    const rows = o.rows.map((r) => r.map((v) => render(v)).join("\x1f"));
    if (!sql.includes("ORDER BY")) rows.sort();
    return { rows: rows.join("\n"), cost: o.cost };
  } catch (e) {
    if (e instanceof EngineError) return { error: e.code() };
    throw e;
  }
}

// show renders a result for a failure message (a cost is a bigint).
export function show(r: Result): string {
  return "error" in r ? r.error : `cost ${r.cost}: ${r.rows}`;
}

// rowPath runs `fn` with a ceiling the statement never reaches and the lane's attempt off.
export function rowPath<T>(s: Session, fn: () => T): T {
  s.setMaxCost(NEVER);
  laneCeiling.off = true;
  try {
    return fn();
  } finally {
    laneCeiling.off = false;
    s.setMaxCost(0n);
  }
}

// check asserts the lane (unmetered) and the row path agree on rows and cost.
export function check(s: Session, sql: string): void {
  s.setMaxCost(0n);
  const lane = outcome(s, sql);
  const row = rowPath(s, () => outcome(s, sql));
  assert.ok(!("error" in row), `row path failed: ${sql}: ${show(row)}`);
  assert.deepEqual(lane, row, `lane vs row path: ${sql}`);
}

// checkUnderBudgets: under a query-memory budget the lane and the row path hold different buffers, so
// they may trip 54P05 at different budgets (memory.md §5.4; the thresholds themselves are pinned across
// cores by `mise run conformance:query_memory`). What must hold on every path and at every budget: a
// statement either fails 54P05 or returns exactly its unbudgeted rows and cost. Run unmetered (the
// lane), under a ceiling it never reaches (the lane as an attempt, cost.md §6.1), and with the attempt
// off (the row path).
export function checkUnderBudgets(
  s: Session,
  sql: string,
  budgets: bigint[] = [64n, 4096n, 64n * 1024n, 1024n * 1024n, 64n * 1024n * 1024n],
): void {
  s.setMaxCost(0n);
  const want = outcome(s, sql);
  assert.ok(!("error" in want), `unbudgeted: ${sql}: ${show(want)}`);
  for (const budget of budgets) {
    s.setMaxQueryMemoryBytes(budget);
    try {
      for (const [maxCost, off] of [
        [0n, false],
        [NEVER, false],
        [NEVER, true],
      ] as const) {
        s.setMaxCost(maxCost);
        laneCeiling.off = off;
        let got: Result;
        try {
          got = outcome(s, sql);
        } finally {
          laneCeiling.off = false;
        }
        if ("error" in got) {
          assert.equal(
            got.error,
            "54P05",
            `budget ${budget}, max_cost ${maxCost}, off ${off}: ${sql}`,
          );
        } else {
          assert.deepEqual(got, want, `budget ${budget}, max_cost ${maxCost}, off ${off}: ${sql}`);
        }
      }
    } finally {
      s.setMaxCost(0n);
      s.setMaxQueryMemoryBytes(0n);
    }
  }
}

// withDb seeds a database — file-backed (reopened, so its leaves are clean Packed pages) or in memory
// — and runs `fn` over a session on it.
export function withDb(file: boolean, seed: string[], fn: (s: Session) => void): void {
  if (!file) {
    const db = createDatabase({});
    try {
      const s = db.session();
      for (const sql of seed) s.execute(sql);
      fn(s);
    } finally {
      db.close();
    }
    return;
  }
  const dir = mkdtempSync(join(tmpdir(), "jed-aggregate-lanes-"));
  try {
    const path = join(dir, "t.jed");
    const created = createDatabase({ path, skipFsync: true });
    const seeder = created.session();
    for (const sql of seed) seeder.execute(sql);
    created.close();
    const db = openDatabase(path, { skipFsync: true });
    try {
      fn(db.session());
    } finally {
      db.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

// The `gt` table — narrow and wide decimals, text, floats, NULLs in every column.
export const GT_SEED = [
  "CREATE TABLE gt (id i32 PRIMARY KEY, n i32, k i64, big i64, d numeric(12,2), " +
    "wide numeric, s text, f f64, m i16)",
  "INSERT INTO gt VALUES " +
    "(-3, 1, NULL, 9223372036854775807, 0.01, 1, 'a', 1.5, 1), " +
    "(-2, 1, 1, -9223372036854775808, -0.01, 0.000000000000000000000000000001, '', -2.25, -1), " +
    "(-1, NULL, 2, NULL, NULL, NULL, NULL, NULL, NULL), " +
    "(0, 2, 2, 0, 0.00, 0.0, 'é', 0, 0)",
  "INSERT INTO gt SELECT g, g % 5, g % 3, " +
    "CASE WHEN g % 2 = 0 THEN -1 ELSE 1 END * g * 2305843009213693, " +
    "((g * 7919) % 100000000)::numeric / 100 - 250000, " +
    "CASE WHEN g % 11 = 0 THEN NULL " +
    "WHEN g % 7 = 0 THEN g * 123456789012345678901234567890.5 * 10000000000000000000000.0 " +
    "WHEN g % 4 = 0 THEN -(g::numeric / 1000000007) " +
    "ELSE g::numeric * 1.000001 END, " +
    "'v' || repeat('x', (g % 40)::i32) || (g % 97)::text, " +
    "(g % 1000)::f64 / 8, (g % 300)::i16 " +
    "FROM generate_series(1, 3000) g",
];
