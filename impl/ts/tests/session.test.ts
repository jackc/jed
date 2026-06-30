// Session surface (spec/design/session.md §2): the Engine-owned STATEFUL default session (the bare
// single-handle path), and — after the §2.4 convergence — ADDITIONAL sessions minted by db.session
// over a shared Database core (each owns its private Engine, shares committed storage through the
// core, carries an independent envelope, autocommit with the lazy gate — no swap), plus the explicit
// Idle/Open/Failed transaction state machine. Per-core API behaviors the shared corpus cannot
// express (it is single-handle SQL-in/rows-out — CLAUDE.md §10). Mirrors impl/rust/tests/session.rs,
// including the view/update closure sugar (now landed on the TS Session, routing the api.ts
// Transaction's commit/rollback through the session so the working set is published).

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, Engine, EngineError, execute, intValue } from "../src/tooling.ts";

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
  // Two sessions over one shared Database core: each owns its private Engine, but committed storage
  // is shared through the core (§2.4) — no swap. Settings (the cost ceiling) are independent.
  const db = Database.newInMemory();
  const a = db.session({});
  a.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  a.execute("INSERT INTO t VALUES (1, 10)");

  // A second session with its own cost ceiling — a's is untouched.
  const s = db.session({ maxCost: 5n });
  assert.strictEqual(s.maxCost, 5n);
  assert.strictEqual(a.maxCost, 0n);

  // It sees a's committed data (committed storage is shared via the core).
  assert.deepStrictEqual(queryRows(s.execute("SELECT id, v FROM t")), [
    [intValue(1n), intValue(10n)],
  ]);

  // A write through the second session (autocommit, lazy gate) is visible to a's next read.
  s.execute("INSERT INTO t VALUES (2, 20)");
  assert.deepStrictEqual(queryRows(a.execute("SELECT id FROM t ORDER BY id")), [
    [intValue(1n)],
    [intValue(2n)],
  ]);

  // Each session keeps its own state/settings: a is still Idle and unlimited.
  assert.strictEqual(a.status(), "Idle");
  assert.strictEqual(a.maxCost, 0n);
});

test("additional session cost ceiling is enforced", () => {
  // The session's settings drive the execution path: a tiny ceiling aborts the scan with 54P01,
  // while an unlimited session runs it fine — both over the same shared core.
  const db = Database.newInMemory();
  const a = db.session({});
  a.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
  a.execute("INSERT INTO t VALUES (1), (2), (3)");
  a.execute("SELECT * FROM t"); // unlimited

  const s = db.session({ maxCost: 1n });
  assert.strictEqual(
    code(() => s.execute("SELECT * FROM t")),
    "54P01",
  );

  a.execute("SELECT * FROM t"); // a unaffected
  assert.strictEqual(a.maxCost, 0n);
});

test("additional session update closure commits to shared storage", () => {
  // s.update opens a write block, runs the closure, and publishes on success — another session over
  // the shared core sees the committed rows. Mirrors impl/rust/tests/session.rs.
  const db = Database.newInMemory();
  const a = db.session({});
  a.execute("CREATE TABLE t (id i32 PRIMARY KEY)");

  const s = db.session({});
  s.update((tx) => {
    tx.execute("INSERT INTO t VALUES (1)");
    tx.execute("INSERT INTO t VALUES (2)");
  });

  assert.deepStrictEqual(queryRows(a.execute("SELECT count(*) FROM t")), [[intValue(2n)]]);
  assert.strictEqual(a.status(), "Idle");
});

test("session update rolls back on a thrown error and releases the gate", () => {
  const db = Database.newInMemory();
  const s = db.session({});
  s.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
  assert.throws(() =>
    s.update((tx) => {
      tx.execute("INSERT INTO t VALUES (1)");
      throw new Error("boom");
    }),
  );
  // The block rolled back (the row is gone), the session is back to Idle, and the writer gate is free
  // (a fresh write succeeds).
  assert.deepStrictEqual(queryRows(s.execute("SELECT count(*) FROM t")), [[intValue(0n)]]);
  assert.strictEqual(s.status(), "Idle");
  s.update((tx) => tx.execute("INSERT INTO t VALUES (9)"));
  assert.deepStrictEqual(queryRows(s.execute("SELECT count(*) FROM t")), [[intValue(1n)]]);
});

test("Database view and update mint a fresh session per call", () => {
  // The bare Database.view/update convenience each mint a fresh autocommit session, run the closure as
  // one transaction, and discard the session — committed data persists through the shared core.
  const db = Database.newInMemory();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
  db.update((tx) => {
    tx.execute("INSERT INTO t VALUES (1)");
    tx.execute("INSERT INTO t VALUES (2)");
  });
  const n = db.view((tx) => [...tx.query("SELECT count(*) FROM t")][0][0]);
  assert.deepStrictEqual(n, intValue(2n));
});
