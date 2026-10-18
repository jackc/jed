import assert from "node:assert/strict";
import { test } from "node:test";
import { Worker } from "node:worker_threads";
import { cancellationCheckpoint, withCancellation, withoutCancellation } from "../src/cancel.ts";
import { Meter } from "../src/cost.ts";
import { Engine } from "../src/executor.ts";
import { EngineError, engineError } from "../src/errors.ts";
import { lex } from "../src/lexer.ts";

function canceled(error: unknown): boolean {
  return error instanceof EngineError && error.code() === "57014";
}

test("cancellation scopes preserve costs, cover scratch meters, and restore after exceptions", () => {
  const meter = new Meter();
  let aborted = false;
  withCancellation(
    () => aborted,
    () => {
      meter.charge(123n);
      meter.guard();
      aborted = true;
      assert.throws(() => meter.guard(), canceled);
      assert.throws(() => new Meter().guard(), canceled);
      withoutCancellation(() => meter.guard());
      assert.throws(() => cancellationCheckpoint(), canceled);
    },
  );
  assert.equal(meter.accrued, 123n);
  meter.guard();
  assert.throws(
    () =>
      withCancellation(
        () => true,
        () => {},
      ),
    canceled,
  );
  cancellationCheckpoint();
});

test("lexer checkpoints can cancel inside a long single literal", () => {
  let checkpoints = 0;
  assert.throws(
    () =>
      withCancellation(
        () => ++checkpoints > 100,
        () => lex("'" + "x".repeat(100_000) + "'"),
      ),
    canceled,
  );
  assert.ok(checkpoints <= 102);
});

test("an atomic flag interrupts already running resident SQL without changing completed costs", async () => {
  const state = new Int32Array(new SharedArrayBuffer(12));
  const worker = new Worker(
    `
    const { workerData } = require('node:worker_threads');
    const flag = new Int32Array(workerData);
    Atomics.store(flag, 1, 1);
    Atomics.notify(flag, 1);
    Atomics.wait(flag, 2, 0, 5000);
    Atomics.store(flag, 0, 1);
  `,
    { eval: true, workerData: state.buffer },
  );
  const exited = new Promise<void>((resolve, reject) => {
    worker.once("exit", (code) =>
      code === 0 ? resolve() : reject(new Error(`worker exit ${code}`)),
    );
    worker.once("error", reject);
  });
  while (Atomics.load(state, 1) === 0) Atomics.wait(state, 1, 0, 1000);
  const engine = new Engine();
  let checkpoints = 0;
  assert.throws(
    () =>
      withCancellation(
        () => {
          // Release the helper only after execution has crossed many checkpoints, so the
          // test proves interruption of running work rather than a pre-aborted entry check.
          if (++checkpoints === 1024) {
            Atomics.store(state, 2, 1);
            Atomics.notify(state, 2);
          }
          return Atomics.load(state, 0) !== 0;
        },
        () =>
          engine.execute(
            "WITH RECURSIVE t(n) AS (VALUES (1) UNION ALL SELECT n+1 FROM t WHERE n<10000000) SELECT sum(n) FROM t",
          ),
      ),
    canceled,
  );
  await exited;
  const normal = engine.execute("SELECT 1+2");
  const scoped = withCancellation(
    () => false,
    () => engine.execute("SELECT 1+2"),
  );
  assert.equal(normal.cost, scoped.cost);
});

test("commit ignores cancellation after its safe checkpoint and discards an earlier canceled commit", () => {
  const engine = new Engine();
  engine.execute("CREATE TABLE t (n i64)");
  let aborted = false;
  let persisted = 0;
  engine.persistHook = () => {
    persisted++;
    aborted = true;
    // Models a metered or parsed operation inside persistence after writes began.
    new Meter().guard();
    lex("SELECT 1");
  };
  withCancellation(
    () => aborted,
    () => engine.execute("INSERT INTO t VALUES (1)"),
  );
  assert.equal(persisted, 1);
  engine.persistHook = null;
  const result = engine.executeStmt(engine.parse("SELECT n FROM t"));
  assert.equal(result.kind, "query");
  if (result.kind !== "query") throw new Error("query expected");
  assert.deepEqual(result.rows, [[{ kind: "int", int: 1n }]]);

  engine.beginTx(true);
  engine.execute("INSERT INTO t VALUES (2)");
  aborted = false;
  withCancellation(
    () => aborted,
    () => {
      aborted = true;
      assert.throws(() => engine.commitTx(), canceled);
    },
  );
  assert.equal(engine.session.tx, null);
  const after = engine.executeStmt(engine.parse("SELECT n FROM t"));
  assert.equal(after.kind, "query");
  if (after.kind !== "query") throw new Error("query expected");
  assert.deepEqual(after.rows, [[{ kind: "int", int: 1n }]]);
});

test("commit storage failure wins over an abort after persistence began", () => {
  const engine = new Engine();
  engine.execute("CREATE TABLE t (n i64)");
  let aborted = false;
  engine.persistHook = () => {
    aborted = true;
    new Meter().guard();
    throw engineError("connection_failure", "injected durable write failure");
  };
  assert.throws(
    () =>
      withCancellation(
        () => aborted,
        () => engine.execute("INSERT INTO t VALUES (1)"),
      ),
    (error) => error instanceof EngineError && error.code() === "08006",
  );
  engine.persistHook = null;
  const result = engine.executeStmt(engine.parse("SELECT n FROM t"));
  assert.equal(result.kind, "query");
  if (result.kind !== "query") throw new Error("query expected");
  assert.deepEqual(result.rows, []);
});
