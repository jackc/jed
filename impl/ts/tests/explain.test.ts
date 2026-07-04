// EXPLAIN behaviours the shared corpus cannot express (spec/design/explain.md): privilege delegation
// to the inner statement, the read/write classification via a READ ONLY transaction, that ANALYZE of a
// write executes+persists while plain EXPLAIN does not, and the EXPLAIN-owns-its-render-cost invariant.
// The plan RENDERING itself is asserted in the corpus (query/explain*.test, dml/explain_dml.test),
// which runs on every core. Mirrors impl/go/explain_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  EngineError,
  intValue,
  type Outcome,
  PrivilegeSet,
  queryOutcome,
  render,
  type Session,
} from "../src/tooling.ts";
import { memDb } from "./mem_db.ts";

// code runs sql and returns the EngineError SQLSTATE, or "" when it succeeds.
function code(db: Session, sql: string): string {
  try {
    db.execute(sql);
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  return "";
}

function queryRows(o: Outcome) {
  if (o.kind !== "query") throw new Error("expected a query result");
  return o.rows;
}

// EXPLAIN requires the INNER statement's privileges (EXPLAIN INSERT needs INSERT), matching PG —
// even though plain EXPLAIN never executes.
test("EXPLAIN delegates the inner statement's privileges", () => {
  const db = memDb().session();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  db.execute("INSERT INTO t VALUES (1, 10)");
  db.setDefaultPrivileges(PrivilegeSet.empty().with("select"));
  db.execute("EXPLAIN SELECT v FROM t"); // SELECT privilege is held
  for (const sql of [
    "EXPLAIN INSERT INTO t VALUES (2, 20)",
    "EXPLAIN UPDATE t SET v = 0",
    "EXPLAIN DELETE FROM t",
  ]) {
    assert.equal(code(db, sql), "42501", sql);
  }
});

// Plain EXPLAIN of a write is a READ (it never mutates), so it is allowed in a READ ONLY transaction;
// EXPLAIN ANALYZE of a write IS a write and is rejected 25006.
test("EXPLAIN write classification via a READ ONLY transaction", () => {
  const db = memDb().session();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  db.execute("INSERT INTO t VALUES (1, 10)");
  db.execute("BEGIN READ ONLY");
  db.execute("EXPLAIN DELETE FROM t"); // a read — allowed in a read-only transaction
  assert.equal(code(db, "EXPLAIN ANALYZE DELETE FROM t"), "25006");
  db.execute("ROLLBACK");
});

// Plain EXPLAIN of a DELETE does not mutate; EXPLAIN ANALYZE of an INSERT does (and persists).
test("plain EXPLAIN does not execute; EXPLAIN ANALYZE does", () => {
  const db = memDb().session();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  db.execute("INSERT INTO t VALUES (1, 10)");
  db.execute("INSERT INTO t VALUES (2, 20)");
  db.execute("EXPLAIN DELETE FROM t"); // plan-only — deletes nothing
  assert.deepStrictEqual(queryRows(queryOutcome(db, "SELECT count(*) FROM t")), [[intValue(2n)]]);
  db.execute("EXPLAIN ANALYZE INSERT INTO t VALUES (3, 30)"); // executes
  assert.deepStrictEqual(queryRows(queryOutcome(db, "SELECT count(*) FROM t")), [[intValue(3n)]]);
});

// The EXPLAIN statement's OWN cost is one rowProduced per emitted plan row — independent of the
// (larger) inner cost reported inside the Analyze root.
test("EXPLAIN owns its render cost (one rowProduced per plan row)", () => {
  const db = memDb().session();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  db.execute("INSERT INTO t VALUES (1, 10)");
  db.execute("INSERT INTO t VALUES (2, 20)");
  const out = queryOutcome(db, "EXPLAIN ANALYZE SELECT * FROM t");
  if (out.kind !== "query") throw new Error("expected a query result");
  assert.equal(out.cost, BigInt(out.rows.length));
  // The Analyze root (row 0) reports the inner cost, which exceeds the render cost here.
  assert.equal(render(out.rows[0]![1]!), "Analyze");
});
