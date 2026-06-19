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

// --- S2 (setval / lastval / ALTER SEQUENCE RESTART, spec/design/sequences.md §4/§6) -----------

// A setval is transactional too (the §5 divergence): an advance inside a rolled-back transaction is
// discarded — PostgreSQL would keep it.
test("setval rolls back with its transaction", () => {
  const db = new Database();
  execute(db, "CREATE SEQUENCE s START 1");
  assert.equal(oneInt(db, "SELECT nextval('s')"), 1n); // committed last_value 1

  execute(db, "BEGIN");
  assert.equal(oneInt(db, "SELECT setval('s', 99)"), 99n); // working last_value 99
  execute(db, "ROLLBACK");

  // jed: the setval vanished — the committed counter is still 1, so the next value is 2.
  assert.equal(oneInt(db, "SELECT nextval('s')"), 2n);
});

// An ALTER SEQUENCE … RESTART is transactional as well (the same §5 divergence).
test("ALTER SEQUENCE RESTART rolls back", () => {
  const db = new Database();
  execute(db, "CREATE SEQUENCE s START 10");
  assert.equal(oneInt(db, "SELECT nextval('s')"), 10n);

  execute(db, "BEGIN");
  execute(db, "ALTER SEQUENCE s RESTART WITH 100");
  assert.equal(oneInt(db, "SELECT nextval('s')"), 100n); // working
  execute(db, "ROLLBACK");

  // The RESTART (and its advance) rolled back — the committed counter is still 10, next is 11.
  assert.equal(oneInt(db, "SELECT nextval('s')"), 11n);
});

// A nextval's lastval/currval session updates roll back with the transaction too (§5/§6): after a
// rolled-back nextval, lastval reverts to its pre-transaction state. (The PG-agreeing lastval values
// — tracking the most recent nextval, reflecting a setval on that same sequence — live in the oracle
// corpus; this asserts only the rollback, which the corpus cannot.)
test("lastval rolls back with its transaction", () => {
  const db = new Database();
  execute(db, "CREATE SEQUENCE a START 100");
  execute(db, "CREATE SEQUENCE b START 200");
  oneInt(db, "SELECT nextval('a')"); // committed: lastval → a's 100
  assert.equal(oneInt(db, "SELECT lastval()"), 100n);

  execute(db, "BEGIN");
  oneInt(db, "SELECT nextval('b')"); // working: lastval → b's 200
  assert.equal(oneInt(db, "SELECT lastval()"), 200n);
  execute(db, "ROLLBACK");

  // The in-transaction nextval('b') vanished, so lastval reverts to a's committed 100.
  assert.equal(oneInt(db, "SELECT lastval()"), 100n);
});

// A non-RESTART ALTER SEQUENCE action is 0A000 in jed (only RESTART is supported this slice) — a
// divergence from PostgreSQL, where ALTER SEQUENCE … INCREMENT BY is valid, so it cannot live in the
// PG-clean oracle corpus.
test("non-RESTART ALTER SEQUENCE is 0A000", () => {
  const db = new Database();
  execute(db, "CREATE SEQUENCE s");
  assert.equal(errCode(db, "ALTER SEQUENCE s INCREMENT BY 2"), "0A000");
  assert.equal(errCode(db, "ALTER SEQUENCE s OWNED BY t.c"), "0A000");
  // ALTER of a non-sequence object is not a known statement at all → 42601 (no escape hatch).
  assert.equal(errCode(db, "ALTER TABLE t ADD COLUMN c i32"), "42601");
});

// setval/ALTER … RESTART are writes — a READ ONLY transaction rejects each with 25006 (each in its
// own block, since the error poisons the block). lastval/currval (pure reads) are allowed.
test("setval/ALTER in read-only transaction is 25006", () => {
  const db = new Database();
  execute(db, "CREATE SEQUENCE s");
  oneInt(db, "SELECT nextval('s')"); // 1, defines session state

  execute(db, "BEGIN READ ONLY");
  assert.equal(errCode(db, "SELECT setval('s', 5)"), "25006");
  execute(db, "ROLLBACK");

  execute(db, "BEGIN READ ONLY");
  assert.equal(errCode(db, "ALTER SEQUENCE s RESTART"), "25006");
  execute(db, "ROLLBACK");

  // lastval is allowed in a read-only block (it mutates nothing).
  execute(db, "BEGIN READ ONLY");
  assert.equal(oneInt(db, "SELECT lastval()"), 1n);
  execute(db, "ROLLBACK");
});
