// S1 session surface (spec/design/session.md §2): the Engine-owned STATEFUL default session,
// ADDITIONAL sessions minted by db.newSession (shared committed storage, independent settings +
// transaction state, run sequentially via the swap), the relocated settings, and the explicit
// Idle/Open/Failed transaction state machine. Per-core API behaviors the shared corpus cannot
// express (it is single-handle SQL-in/rows-out — CLAUDE.md §10). Mirrors impl/rust/tests/session.rs
// (TS drives transactions via SQL BEGIN/COMMIT through Session.execute — the view/update closure
// sugar is a TS follow-on, since it would import the api.ts Transaction the executor avoids).

import assert from "node:assert/strict";
import { test } from "node:test";
import { Engine, EngineError, execute, intValue } from "../src/lib.ts";

function code(fn: () => unknown): string {
  try {
    fn();
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  throw new Error("expected an error, got none");
}

function queryRows(o: ReturnType<typeof execute>) {
  if (o.kind !== "query") throw new Error("expected a query result");
  return o.rows;
}

test("default session is stateful across calls", () => {
  // The Engine-owned default session holds an open BEGIN block across separate calls (the
  // PG/SQLite connection model, §2.1); db.status() exposes the explicit state machine.
  const db = new Engine();
  assert.strictEqual(db.status(), "Idle");
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  execute(db, "BEGIN");
  assert.strictEqual(db.status(), "Open");
  execute(db, "INSERT INTO t VALUES (1)");
  assert.strictEqual(db.status(), "Open"); // still open across the separate call
  execute(db, "COMMIT");
  assert.strictEqual(db.status(), "Idle");
});

test("failed block is the Failed state", () => {
  // A statement error inside a block poisons it: status is Failed, every later statement but
  // ROLLBACK/COMMIT is 25P02 (§2.2 / transactions.md §6), and ROLLBACK returns to Idle.
  const db = new Engine();
  execute(db, "BEGIN");
  assert.strictEqual(
    code(() => execute(db, "SELECT * FROM missing")),
    "42P01",
  );
  assert.strictEqual(db.status(), "Failed");
  assert.strictEqual(
    code(() => execute(db, "SELECT 1")),
    "25P02",
  );
  execute(db, "ROLLBACK");
  assert.strictEqual(db.status(), "Idle");
});

test("additional session shares storage with independent settings", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  execute(db, "INSERT INTO t VALUES (1, 10)");

  // Mint a second session with its own cost ceiling — the default is untouched.
  const s = db.newSession({ maxCost: 5n });
  assert.strictEqual(s.maxCost, 5n);
  assert.strictEqual(db.session.maxCost, 0n);

  // It sees the default session's committed data (committed storage is shared).
  assert.deepStrictEqual(queryRows(s.execute(db, "SELECT id, v FROM t")), [
    [intValue(1n), intValue(10n)],
  ]);

  // A write through the second session is visible to the default session.
  s.execute(db, "INSERT INTO t VALUES (2, 20)");
  assert.deepStrictEqual(queryRows(execute(db, "SELECT id FROM t ORDER BY id")), [
    [intValue(1n)],
    [intValue(2n)],
  ]);

  // The swap restored the default session: still Idle, still unlimited.
  assert.strictEqual(db.status(), "Idle");
  assert.strictEqual(db.session.maxCost, 0n);
});

test("additional session cost ceiling is enforced via swap", () => {
  // Proves the swap installs the additional session's settings into the execution path: a tiny
  // ceiling aborts the scan with 54P01, while the unlimited default runs it fine.
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  execute(db, "INSERT INTO t VALUES (1), (2), (3)");
  execute(db, "SELECT * FROM t"); // default: unlimited

  const s = db.newSession({ maxCost: 1n });
  assert.strictEqual(
    code(() => s.execute(db, "SELECT * FROM t")),
    "54P01",
  );

  execute(db, "SELECT * FROM t"); // default unaffected
  assert.strictEqual(db.session.maxCost, 0n);
});
