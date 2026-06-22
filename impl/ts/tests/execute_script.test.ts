// S2 executeScript host-API surface (spec/design/session.md §4.2): the multi-statement
// migration/import convenience — split, run each in order, discard rows, return the O(1)
// ScriptSummary. All-or-nothing when Idle, join-when-Open, in-script transaction control 0A000.
// Host-API behaviors the single-statement corpus cannot call (CLAUDE.md §10). Mirrors
// impl/rust/tests/execute_script.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, EngineError, execute, intValue } from "../src/lib.ts";

function code(fn: () => unknown): string {
  try {
    fn();
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  throw new Error("expected an error, got none");
}

function countRows(db: Database): unknown {
  const out = execute(db, "SELECT count(*) FROM t");
  if (out.kind !== "query") throw new Error("expected a query result");
  return out.rows;
}

test("script summary counts and commits atomically when Idle", () => {
  const db = new Database();
  const summary = db.executeScript(
    `CREATE TABLE t (id i32 PRIMARY KEY, v i32);
     INSERT INTO t VALUES (1, 10);
     INSERT INTO t VALUES (2, 20), (3, 30);
     UPDATE t SET v = v + 1 WHERE id >= 2;
     DELETE FROM t WHERE id = 1;`,
  );
  assert.strictEqual(summary.statementsRun, 5);
  assert.strictEqual(summary.rowsAffectedTotal, 1 + 2 + 2 + 1); // DDL contributes 0
  assert.ok(summary.cost > 0n);
  assert.strictEqual(db.status(), "Idle");
  assert.deepStrictEqual(countRows(db), [[intValue(2n)]]); // ids 2 and 3 remain
});

test("script is all-or-nothing on error", () => {
  const db = new Database();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  assert.strictEqual(
    code(() =>
      db.executeScript(
        "INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); INSERT INTO t VALUES (1)",
      ),
    ),
    "23505",
  );
  assert.strictEqual(db.status(), "Idle");
  assert.deepStrictEqual(countRows(db), [[intValue(0n)]]); // nothing committed
});

test("script SELECT rows are discarded but the statement is counted", () => {
  const db = new Database();
  const summary = db.executeScript(
    `CREATE TABLE t (id i32 PRIMARY KEY);
     INSERT INTO t VALUES (1), (2);
     SELECT * FROM t;`,
  );
  assert.strictEqual(summary.statementsRun, 3);
  assert.strictEqual(summary.rowsAffectedTotal, 2); // only the INSERT
  assert.deepStrictEqual(countRows(db), [[intValue(2n)]]);
});

test("empty script is a no-op success", () => {
  const db = new Database();
  const summary = db.executeScript("  -- just a comment\n /* and a block */ ;;; ");
  assert.deepStrictEqual(summary, { statementsRun: 0, rowsAffectedTotal: 0, cost: 0n });
  assert.strictEqual(db.status(), "Idle");
});

test("in-script transaction control is 0A000 and rolls the run back", () => {
  const db = new Database();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  for (const script of [
    "INSERT INTO t VALUES (1); COMMIT; INSERT INTO t VALUES (2)",
    "INSERT INTO t VALUES (1); BEGIN; INSERT INTO t VALUES (2)",
    "INSERT INTO t VALUES (1); ROLLBACK",
  ]) {
    assert.strictEqual(
      code(() => db.executeScript(script)),
      "0A000",
      script,
    );
    assert.strictEqual(db.status(), "Idle");
    assert.deepStrictEqual(countRows(db), [[intValue(0n)]], script); // wrapper rolled back
  }
});

test("script joins an open transaction without committing", () => {
  const db = new Database();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  execute(db, "BEGIN");
  const summary = db.executeScript("INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)");
  assert.strictEqual(summary.statementsRun, 2);
  assert.strictEqual(db.status(), "Open"); // NOT auto-committed — the caller's block stays open
  assert.deepStrictEqual(countRows(db), [[intValue(2n)]]); // visible inside the block
  execute(db, "ROLLBACK");
  assert.deepStrictEqual(countRows(db), [[intValue(0n)]]); // the caller rolled it back
});

test("script error inside an open transaction leaves it Failed for the caller", () => {
  const db = new Database();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  execute(db, "BEGIN");
  assert.strictEqual(
    code(() => db.executeScript("INSERT INTO t VALUES (1); INSERT INTO t VALUES (1)")),
    "23505",
  );
  assert.strictEqual(db.status(), "Failed"); // executeScript does NOT roll back a tx it doesn't own
  execute(db, "ROLLBACK");
  assert.strictEqual(db.status(), "Idle");
});

test("additional session runs a script via the swap", () => {
  const db = new Database();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  const s = db.newSession();
  const summary = s.executeScript(db, "INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)");
  assert.strictEqual(summary.statementsRun, 2);
  assert.deepStrictEqual(countRows(db), [[intValue(2n)]]);
  assert.strictEqual(db.status(), "Idle");
});
