// The columnar lanes with a cancellation check installed (spec/design/memory.md §5.4): a check does
// not change which lane a statement takes — the lanes poll it themselves, at most every 256 records —
// so a statement reserves the same query memory with and without one, and a cancellation that lands
// inside a lane raises 57014, charges the work it consumed, and leaves the session usable. Both
// drivers: the synchronous drain under an installed check, and the async driver's atomic poll.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { withCancellation } from "../src/cancel.ts";
import { runAsync, type Steps } from "../src/execution.ts";
import {
  createDatabase,
  EngineError,
  laneGroupHandovers,
  openDatabase,
  queryMemoryPeak,
  type Session,
} from "../src/tooling.ts";
import { render } from "../src/value.ts";
import { GT_SEED, outcome, type Result, rowPath, show, withDb } from "./aggregate_lanes_util.ts";

// A whole-table fold, a grouped fold, and a bare-column projection — the three lane shapes.
const WHOLE = "SELECT count(*), sum(d), max(s) FROM gt WHERE d <> 0";
const GROUPED = "SELECT n, count(*), sum(d) FROM gt GROUP BY n";
const PROJECTION = "SELECT id, d, s FROM gt WHERE n > 1";
// No filter and no decimal work: on a file this lane charges nothing until its scan ends, and polls
// only at its own 256-record cadence.
const COUNTS = "SELECT n, count(*) FROM gt GROUP BY n";
const QUERIES = [WHOLE, GROUPED, PROJECTION, COUNTS];

const BUDGET = 1n << 40n; // huge but active, so every reservation is measured

// withDbAsync is withDb for a body that awaits: the database stays open until the body settles.
async function withDbAsync(file: boolean, fn: (s: Session) => Promise<void>): Promise<void> {
  if (!file) {
    const db = createDatabase({});
    try {
      const s = db.session();
      for (const sql of GT_SEED) s.execute(sql);
      await fn(s);
    } finally {
      db.close();
    }
    return;
  }
  const dir = mkdtempSync(join(tmpdir(), "jed-aggregate-lanes-cancel-"));
  try {
    const path = join(dir, "t.jed");
    const created = createDatabase({ path, skipFsync: true });
    const seeder = created.session();
    for (const sql of GT_SEED) seeder.execute(sql);
    created.close();
    const db = openDatabase(path, { skipFsync: true });
    try {
      await fn(db.session());
    } finally {
      db.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

function* drainSteps(s: Session, sql: string): Steps<string> {
  const rows = yield* s.querySteps(sql);
  const out: string[] = [];
  try {
    for (;;) {
      const step = yield* rows.nextSteps();
      if (step.done) break;
      out.push(step.value.map((v) => render(v)).join("\x1f"));
    }
  } finally {
    rows.close();
  }
  if (!sql.includes("ORDER BY")) out.sort();
  return out.join("\n");
}

// The statement's rows through the async driver, or its error code.
async function asyncRows(s: Session, sql: string, poll?: () => boolean): Promise<string> {
  try {
    return await runAsync(drainSteps(s, sql), {
      poll,
      schedule: (resume) => setImmediate(resume),
    });
  } catch (e) {
    if (e instanceof EngineError) return `error ${e.code()}`;
    throw e;
  }
}

// The statement's outcome on the synchronous drain under `check` (which is also polled once when it
// is installed, before the statement starts).
function checkedOutcome(s: Session, sql: string, check: () => boolean): Result {
  try {
    return withCancellation(check, () => outcome(s, sql));
  } catch (e) {
    if (e instanceof EngineError) return { error: e.code() };
    throw e;
  }
}

// peak runs `fn` and returns the highest query-memory balance it reached.
function peak(fn: () => void): number {
  queryMemoryPeak.value = 0;
  fn();
  return queryMemoryPeak.value;
}

async function asyncPeak(fn: () => Promise<unknown>): Promise<number> {
  queryMemoryPeak.value = 0;
  await fn();
  return queryMemoryPeak.value;
}

for (const file of [true, false]) {
  const where = file ? "on a file" : "in memory";

  test(`a cancellation check does not change the lane (${where})`, async () => {
    await withDbAsync(file, async (s) => {
      s.setMaxQueryMemoryBytes(BUDGET);
      for (const sql of QUERIES) {
        const want = outcome(s, sql);
        assert.ok(!("error" in want), `${sql}: ${show(want)}`);
        const bare = peak(() => outcome(s, sql));
        // The synchronous drain under an installed check that never fires.
        let got: Result | undefined;
        const checked = peak(() => {
          got = withCancellation(
            () => false,
            () => outcome(s, sql),
          );
        });
        assert.deepEqual(got, want, `rows and cost under a check: ${sql}`);
        assert.equal(checked, bare, `sync driver, peak under a check: ${sql}`);
        // The async driver with an atomic poll, against the same driver with none.
        const asyncBare = await asyncPeak(() => asyncRows(s, sql));
        const asyncChecked = await asyncPeak(() => asyncRows(s, sql, () => false));
        assert.equal(asyncChecked, asyncBare, `async driver, peak under a poll: ${sql}`);
        // The comparison discriminates: on a file the row path holds different buffers.
        if (file) {
          const row = peak(() => rowPath(s, () => outcome(s, sql)));
          assert.notEqual(row, bare, `the row path holds different buffers: ${sql}`);
        }
      }
      s.setMaxQueryMemoryBytes(0n);
    });
  });
}

test("a grouped fold hands over to spill under a check", () => {
  withDb(true, GT_SEED, (s) => {
    s.setWorkMem(1);
    const before = laneGroupHandovers.count;
    const want = outcome(s, GROUPED);
    const got = withCancellation(
      () => false,
      () => outcome(s, GROUPED),
    );
    assert.deepEqual(got, want);
    assert.equal(laneGroupHandovers.count, before + 2, "both runs took the lane and handed over");
  });
});

// Fire the check at its k-th poll, for every k a full run makes: each run is 57014, several of them
// land inside the lane (the session's cumulative cost then grows by part of the statement's cost —
// the consumed work), and the session answers the same statement afterwards.
for (const file of [true, false]) {
  const where = file ? "on a file" : "in memory";

  test(`cancelling inside a lane is 57014 and leaves the session usable, sync (${where})`, () => {
    withDb(file, GT_SEED, (s) => {
      for (const sql of QUERIES) {
        const want = outcome(s, sql);
        assert.ok(!("error" in want));
        let total = 0;
        withCancellation(
          () => {
            total++;
            return false;
          },
          () => outcome(s, sql),
        );
        let partial = 0;
        // About fifty polls spread over the run (every one when it makes fewer), and the last.
        const step = Math.max(1, Math.floor(total / 48));
        for (let k = 1; k <= total; k = k < total && k + step > total ? total : k + step) {
          let polls = 0;
          const before = s.lifetimeCost();
          const got = checkedOutcome(s, sql, () => ++polls >= k);
          assert.deepEqual(got, { error: "57014" }, `poll ${k}: ${sql}`);
          const spent = s.lifetimeCost() - before;
          assert.ok(spent <= want.cost, `poll ${k} spent ${spent} of ${want.cost}: ${sql}`);
          // Under half the statement's cost: the scan was still running (emission follows it).
          if (spent > 0n && spent * 2n < want.cost) partial++;
        }
        // 3,004 records at one poll per 256: several polls land while the scan has consumed some
        // work and less than half of it.
        assert.ok(partial >= 3, `${partial} mid-scan cancellations charged consumed work: ${sql}`);
        assert.deepEqual(outcome(s, sql), want, `usable afterwards: ${sql}`);
      }
    });
  });

  test(`cancelling inside a lane is 57014 and leaves the session usable, async (${where})`, async () => {
    await withDbAsync(file, async (s) => {
      for (const sql of QUERIES) {
        const want = await asyncRows(s, sql);
        assert.ok(!want.startsWith("error"), want);
        let total = 0;
        await asyncRows(s, sql, () => {
          total++;
          return false;
        });
        // A dozen polls spread over the run, and the last.
        const ks: number[] = [];
        for (let k = 1; k < total; k += Math.max(1, Math.floor(total / 12))) ks.push(k);
        ks.push(total);
        for (const k of ks) {
          let polls = 0;
          const got = await asyncRows(s, sql, () => ++polls >= k);
          assert.equal(got, "error 57014", `poll ${k} of ${total}: ${sql}`);
        }
        assert.equal(await asyncRows(s, sql), want, `usable afterwards: ${sql}`);
      }
    });
  });
}
