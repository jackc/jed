// Phase 5 (P5.2): explicit transactions — the host Session transaction API (spec/design/api.md §2.2 /
// §6, transactions.md §4.4). The SQL BEGIN/COMMIT/ROLLBACK surface and its visibility / rollback /
// read-only / failed-block semantics are pinned by the shared conformance corpus
// (suites/transactions/); these per-core tests cover the programmatic surface the corpus does not
// exercise: db.begin(writable), the view/update closure wrappers, and commit/rollback as the same
// mechanism. Mirrors impl/rust/tests/transactions.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { EngineError } from "../src/tooling.ts";
import { type Handle, queryOutcome } from "./util.ts";
import { memDb } from "./mem_db.ts";

// rowCount returns the number of rows of `SELECT * FROM t` against the committed/visible state.
function rowCount(db: Handle, table: string): number {
  const o = queryOutcome(db, `SELECT * FROM ${table}`);
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
  const db = memDb().session();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
  db.begin(true);
  db.execute("INSERT INTO t VALUES (1)");
  db.execute("INSERT INTO t VALUES (2)");
  // read-your-writes within the transaction
  assert.equal(db.query("SELECT id FROM t").columnNames.length >= 1, true);
  let inside = 0;
  for (const _ of db.query("SELECT id FROM t")) inside++;
  assert.equal(inside, 2);
  db.commit();
  assert.equal(rowCount(db, "t"), 2);
});

test("begin → execute → rollback discards", () => {
  const db = memDb().session();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
  db.execute("INSERT INTO t VALUES (1)");
  db.begin(true);
  db.execute("INSERT INTO t VALUES (2)");
  db.rollback();
  assert.equal(db.inTransaction(), false);
  assert.equal(rowCount(db, "t"), 1);
});

test("update closure commits on success", () => {
  const db = memDb().session();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
  const n = db.update((tx) => {
    tx.execute("INSERT INTO t VALUES (1)");
    tx.execute("INSERT INTO t VALUES (2)");
    return 42;
  });
  assert.equal(n, 42);
  assert.equal(db.inTransaction(), false);
  assert.equal(rowCount(db, "t"), 2);
});

test("update closure rolls back on a thrown error", () => {
  const db = memDb().session();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
  db.execute("INSERT INTO t VALUES (1)");
  const code = codeOf(() =>
    db.update((tx) => {
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
  const db = memDb().session();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
  db.execute("INSERT INTO t VALUES (1), (2)");
  // a read inside a view works and returns its value
  const got = db.view((tx) => {
    let c = 0;
    for (const _ of tx.query("SELECT id FROM t")) c++;
    return c;
  });
  assert.equal(got, 2);
  // a write inside a view is 25006, and the view auto-rolls-back
  const code = codeOf(() => db.view((tx) => tx.execute("INSERT INTO t VALUES (3)")));
  assert.equal(code, "25006");
  assert.equal(rowCount(db, "t"), 2);
});

test("nested begin is 25001", () => {
  const db = memDb().session();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
  db.begin(true);
  db.execute("INSERT INTO t VALUES (1)");
  // a SQL BEGIN inside an already-open transaction is 25001
  assert.equal(
    codeOf(() => db.execute("BEGIN")),
    "25001",
  );
  db.commit();
  assert.equal(rowCount(db, "t"), 1);
});

test("commit and rollback are no-ops in autocommit", () => {
  const db = memDb().session();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
  // no open transaction: both are lenient no-op successes (transactions.md §4.2)
  db.commit();
  db.rollback();
  db.execute("INSERT INTO t VALUES (1)");
  db.rollback(); // does not undo the autocommitted insert
  assert.equal(rowCount(db, "t"), 1);
});
