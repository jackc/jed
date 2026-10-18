// Parameter-sensitive planner frames are continuation-local, even for internal drivers that
// interleave in one realm. The public worker also serializes engine operations.
import assert from "node:assert/strict";
import { test } from "node:test";
import { createDatabase } from "../src/file.ts";
import type { Rows } from "../src/api.ts";
import { EngineError } from "../src/errors.ts";
import { COOPERATE, runAsync, runSync, type Steps } from "../src/execution.ts";
import { currentPlanningFrames, paramValue, Planning } from "../src/plan_params.ts";
import type { Session } from "../src/shared.ts";
import { intValue, type Value } from "../src/value.ts";

function* collect(
  session: Session,
  sql: string,
  value: bigint,
): Steps<{ values: Value[][]; cost: bigint }> {
  let rows: Rows | undefined;
  try {
    rows = yield* session.queryPreparedSteps(session.prepareStatement(sql), [intValue(value)]);
    const values: Value[][] = [];
    for (;;) {
      const row = yield* rows.nextSteps();
      if (row.done) return { values, cost: rows.cost };
      values.push(row.value);
    }
  } finally {
    rows?.close();
  }
}

test("interleaved WITH and EXPLAIN continuations isolate bound planning values", async () => {
  const db = createDatabase();
  const first = db.session();
  const second = db.session();
  const withSql = "WITH x AS (SELECT g FROM generate_series(1, $1::i64) AS g) SELECT sum(g) FROM x";
  const explainSql =
    "EXPLAIN (ANALYZE, COSTS false) SELECT sum(g) FROM generate_series(1, $1::i64) AS g";
  const caller = Planning.begin();
  const callerValue = intValue(999n);
  caller.bind([callerValue]);
  const ambient = currentPlanningFrames();
  let turns = 0;
  const schedule = (resume: () => void) => {
    assert.equal(currentPlanningFrames(), ambient);
    assert.equal(paramValue(0), callerValue);
    turns++;
    setImmediate(resume);
  };
  try {
    const expectedFirst = runSync(collect(first, withSql, 40n));
    const expectedSecond = runSync(collect(second, explainSql, 85n));
    const pendingFirst = runAsync(collect(first, withSql, 40n), { quantum: 2, schedule });
    const pendingSecond = runAsync(collect(second, explainSql, 85n), { quantum: 2, schedule });
    assert.equal(currentPlanningFrames(), ambient);
    assert.equal(paramValue(0), callerValue);
    const [actualFirst, actualSecond] = await Promise.all([pendingFirst, pendingSecond]);
    assert.deepEqual(actualFirst, expectedFirst);
    assert.deepEqual(actualSecond, expectedSecond);
    assert.ok(turns > 2, "both executions must suspend through task turns");
    assert.equal(currentPlanningFrames(), ambient);
    assert.equal(paramValue(0), callerValue);
  } finally {
    caller.end();
    first.close();
    second.close();
    db.close();
  }
});

test("cancellation unwinds its planning frames while nested sync work shares the active scope", async () => {
  const db = createDatabase();
  const session = db.session();
  const caller = Planning.begin();
  const callerValue = intValue(37n);
  caller.bind([callerValue]);
  const ambient = currentPlanningFrames();
  let canceled = false;
  let turns = 0;
  let cleaned = false;
  function* work(): Steps<void> {
    const local = Planning.begin();
    const localValue = intValue(73n);
    local.bind([localValue]);
    try {
      const active = currentPlanningFrames();
      runSync(
        (function* (): Steps<void> {
          assert.equal(currentPlanningFrames(), active);
          assert.equal(paramValue(0), localValue);
          yield COOPERATE;
        })(),
      );
      yield* collect(
        session,
        "WITH x AS (SELECT g FROM generate_series(1, $1::i64) AS g) SELECT sum(g) FROM x",
        1000n,
      );
    } finally {
      assert.equal(paramValue(0), localValue);
      local.end();
      cleaned = true;
    }
  }
  try {
    await assert.rejects(
      runAsync(work(), {
        quantum: 2,
        check: () => canceled,
        schedule(resume) {
          assert.equal(currentPlanningFrames(), ambient);
          assert.equal(paramValue(0), callerValue);
          setImmediate(() => {
            if (++turns === 2) canceled = true;
            resume();
          });
        },
      }),
      (error) => error instanceof EngineError && error.code() === "57014",
    );
    assert.equal(cleaned, true);
    assert.equal(turns, 2);
    assert.equal(currentPlanningFrames(), ambient);
    assert.equal(paramValue(0), callerValue);
  } finally {
    caller.end();
    session.close();
    db.close();
  }
});
