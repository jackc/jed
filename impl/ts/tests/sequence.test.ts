// Sequences (spec/design/sequences.md) — the per-core unit tests for behavior the PG-clean oracle
// corpus cannot express: the transactional-rollback divergence (nextval rolls back — a deliberate
// PG divergence, §5), the read-only 25006 gate, session-local currval, and NULL propagation. The
// PG-agreeing behavior (nextval values, currval, 42P01/42P07/22023/2200H, CYCLE) lives in
// suites/ddl/sequence.test + suites/expr/sequence_value.test (CLAUDE.md §10). Mirrors
// impl/rust/tests/sequence.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, EngineError, execute } from "../src/lib.ts";

// oneInt runs a single-column SELECT and returns its one int value, or null for a NULL value.
function oneInt(db: Database, sql: string): bigint | null {
  const o = execute(db, sql);
  if (o.kind !== "query") throw new Error(`expected a query, got ${o.kind}`);
  const v = o.rows[0]![0]!;
  if (v.kind === "int") return v.int;
  if (v.kind === "null") return null;
  throw new Error(`expected int/null, got ${v.kind}`);
}

// errCode runs sql and returns the SQLSTATE of the EngineError it throws.
function errCode(db: Database, sql: string): string {
  try {
    execute(db, sql);
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  throw new Error(`expected an EngineError from ${sql}`);
}

// THE headline divergence (§5): a nextval advance inside a transaction is discarded by ROLLBACK
// (PostgreSQL keeps it — its sequences are non-transactional). jed is deterministic instead.
test("nextval rolls back with its transaction", () => {
  const db = new Database();
  execute(db, "CREATE SEQUENCE s");
  assert.equal(oneInt(db, "SELECT nextval('s')"), 1n); // committed: last_value 1

  execute(db, "BEGIN");
  assert.equal(oneInt(db, "SELECT nextval('s')"), 2n); // working: last_value 2
  assert.equal(oneInt(db, "SELECT nextval('s')"), 3n); // working: last_value 3
  execute(db, "ROLLBACK");

  // jed: the in-transaction advances vanished — the committed counter is still 1, so the next value
  // is 2 (PostgreSQL would return 4 here: its advance to 3 survived the rollback).
  assert.equal(oneInt(db, "SELECT nextval('s')"), 2n);

  // A COMMITted advance, by contrast, persists (identical to PG).
  execute(db, "BEGIN");
  assert.equal(oneInt(db, "SELECT nextval('s')"), 3n);
  execute(db, "COMMIT");
  assert.equal(oneInt(db, "SELECT nextval('s')"), 4n);
});

// A failed autocommit statement does not advance the sequence either (the per-statement rollback).
test("failed statement does not advance", () => {
  const db = new Database();
  execute(db, "CREATE SEQUENCE s MAXVALUE 1");
  assert.equal(oneInt(db, "SELECT nextval('s')"), 1n);
  // The next nextval traps 2200H — and because it failed, the counter did not move.
  assert.equal(errCode(db, "SELECT nextval('s')"), "2200H");
  assert.equal(errCode(db, "SELECT nextval('s')"), "2200H");
});

// nextval is a write, so a READ ONLY transaction rejects it with 25006; currval (a pure read) is
// allowed there (spec/design/sequences.md §4/§6).
test("nextval in read-only transaction is 25006", () => {
  const db = new Database();
  execute(db, "CREATE SEQUENCE s");
  oneInt(db, "SELECT nextval('s')"); // 1, defines the session value

  execute(db, "BEGIN READ ONLY");
  assert.equal(errCode(db, "SELECT nextval('s')"), "25006");
  execute(db, "ROLLBACK");

  // currval is allowed in a read-only transaction (it mutates nothing) — a fresh block, since the
  // 25006 above poisoned the previous one (any in-block error aborts it).
  execute(db, "BEGIN READ ONLY");
  assert.equal(oneInt(db, "SELECT currval('s')"), 1n);
  execute(db, "ROLLBACK");
});

// currval is session-local and 55000 before the first nextval.
test("currval session state and 55000", () => {
  const db = new Database();
  execute(db, "CREATE SEQUENCE s");
  assert.equal(errCode(db, "SELECT currval('s')"), "55000");
  oneInt(db, "SELECT nextval('s')");
  assert.equal(oneInt(db, "SELECT currval('s')"), 1n);
  // currval does not advance: repeated reads return the same value.
  assert.equal(oneInt(db, "SELECT currval('s')"), 1n);
});
