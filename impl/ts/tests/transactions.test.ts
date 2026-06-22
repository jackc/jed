// Phase 5 (P5.2): explicit transactions — the host Transaction API (spec/design/api.md §2.2 / §6,
// transactions.md §4.4). The SQL BEGIN/COMMIT/ROLLBACK surface and its visibility / rollback /
// read-only / failed-block semantics are pinned by the shared conformance corpus
// (suites/transactions/); these per-core tests cover the programmatic surface the corpus does not
// exercise: begin(db, writable), the view/update closure wrappers, and commit/rollback as the same
// mechanism.

import assert from "node:assert/strict";
import { test } from "node:test";
import { begin, Database, EngineError, execute, update, view } from "../src/lib.ts";

// rowCount returns the number of rows of `SELECT * FROM t` against the committed/visible state.
function rowCount(db: Database, table: string): number {
  const o = execute(db, `SELECT * FROM ${table}`);
  if (o.kind !== "query") throw new Error("expected a query result");
  return o.rows.length;
}

function codeOf(fn: () => void): string {
  try {
    fn();
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  throw new Error("expected an error");
}

test("begin → execute → commit is visible", () => {
  const db = new Database();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  const tx = begin(db, true);
  tx.execute("INSERT INTO t VALUES (1)");
  tx.execute("INSERT INTO t VALUES (2)");
  // read-your-writes within the transaction
  assert.equal(tx.query("SELECT id FROM t").columnNames.length >= 1, true);
  let inside = 0;
  for (const _ of tx.query("SELECT id FROM t")) inside++;
  assert.equal(inside, 2);
  tx.commit();
  assert.equal(rowCount(db, "t"), 2);
});

test("begin → execute → rollback discards", () => {
  const db = new Database();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  execute(db, "INSERT INTO t VALUES (1)");
  const tx = begin(db, true);
  tx.execute("INSERT INTO t VALUES (2)");
  tx.rollback();
  assert.equal(db.inTransaction(), false);
  assert.equal(rowCount(db, "t"), 1);
});

test("update closure commits on success", () => {
  const db = new Database();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  const n = update(db, (tx) => {
    tx.execute("INSERT INTO t VALUES (1)");
    tx.execute("INSERT INTO t VALUES (2)");
    return 42;
  });
  assert.equal(n, 42);
  assert.equal(db.inTransaction(), false);
  assert.equal(rowCount(db, "t"), 2);
});

test("update closure rolls back on a thrown error", () => {
  const db = new Database();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  execute(db, "INSERT INTO t VALUES (1)");
  const code = codeOf(() =>
    update(db, (tx) => {
      tx.execute("INSERT INTO t VALUES (2)");
      // a duplicate key fails the closure -> the whole update auto-rolls-back
      tx.execute("INSERT INTO t VALUES (1)");
    }),
  );
  assert.equal(code, "23505");
  assert.equal(db.inTransaction(), false);
  // both the failing insert AND the earlier successful one are discarded
  assert.equal(rowCount(db, "t"), 1);
});

test("view is read-only", () => {
  const db = new Database();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  execute(db, "INSERT INTO t VALUES (1), (2)");
  // a read inside a view works and returns its value
  const got = view(db, (tx) => {
    let c = 0;
    for (const _ of tx.query("SELECT id FROM t")) c++;
    return c;
  });
  assert.equal(got, 2);
  // a write inside a view is 25006, and the view auto-rolls-back
  const code = codeOf(() => view(db, (tx) => tx.execute("INSERT INTO t VALUES (3)")));
  assert.equal(code, "25006");
  assert.equal(rowCount(db, "t"), 2);
});

test("nested begin is 25001", () => {
  const db = new Database();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  const tx = begin(db, true);
  tx.execute("INSERT INTO t VALUES (1)");
  // a SQL BEGIN inside an already-open transaction is 25001
  assert.equal(
    codeOf(() => tx.execute("BEGIN")),
    "25001",
  );
  tx.commit();
  assert.equal(rowCount(db, "t"), 1);
});

test("commit and rollback are no-ops in autocommit", () => {
  const db = new Database();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  // no open transaction: both are lenient no-op successes (transactions.md §4.2)
  db.commitTx();
  db.rollbackTx();
  execute(db, "INSERT INTO t VALUES (1)");
  db.rollbackTx(); // does not undo the autocommitted insert
  assert.equal(rowCount(db, "t"), 1);
});
