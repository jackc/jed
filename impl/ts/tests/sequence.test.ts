// Sequences (spec/design/sequences.md) — the per-core unit tests for behavior the PG-clean oracle
// corpus cannot express: the transactional-rollback divergence (nextval rolls back — a deliberate
// PG divergence, §5), the read-only 25006 gate, session-local currval, and NULL propagation. The
// PG-agreeing behavior (nextval values, currval, 42P01/42P07/22023/2200H, CYCLE) lives in
// suites/ddl/sequence.test + suites/expr/sequence_value.test (CLAUDE.md §10). Mirrors
// impl/rust/tests/sequence.rs.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { close, commit, create, Engine, EngineError, execute, open } from "../src/lib.ts";

// oneInt runs a single-column SELECT and returns its one int value, or null for a NULL value.
function oneInt(db: Engine, sql: string): bigint | null {
  const o = execute(db, sql);
  if (o.kind !== "query") throw new Error(`expected a query, got ${o.kind}`);
  const v = o.rows[0]![0]!;
  if (v.kind === "int") return v.int;
  if (v.kind === "null") return null;
  throw new Error(`expected int/null, got ${v.kind}`);
}

// errCode runs sql and returns the SQLSTATE of the EngineError it throws.
function errCode(db: Engine, sql: string): string {
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
  const db = new Engine();
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
  const db = new Engine();
  // A two-value [1, 2] sequence (MINVALUE == MAXVALUE is rejected, matching PG — §15.2).
  execute(db, "CREATE SEQUENCE s MAXVALUE 2");
  assert.equal(oneInt(db, "SELECT nextval('s')"), 1n);
  assert.equal(oneInt(db, "SELECT nextval('s')"), 2n);
  // The next nextval traps 2200H — and because it failed, the counter did not move, so a second
  // attempt traps identically.
  assert.equal(errCode(db, "SELECT nextval('s')"), "2200H");
  assert.equal(errCode(db, "SELECT nextval('s')"), "2200H");
});

// nextval is a write, so a READ ONLY transaction rejects it with 25006; currval (a pure read) is
// allowed there (spec/design/sequences.md §4/§6).
test("nextval in read-only transaction is 25006", () => {
  const db = new Engine();
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
  const db = new Engine();
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
  const db = new Engine();
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
  const db = new Engine();
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
  const db = new Engine();
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

// The ALTER SEQUENCE actions jed still does not support are 0A000 — each VALID in PostgreSQL, so they
// cannot live in the PG-clean oracle corpus (sequences.md §15). AS type is foreclosed because the
// value type is not persisted (§14.4); OWNED BY / OWNER TO / SET … have no jed concept. (The option
// set INCREMENT/MINVALUE/… and RENAME TO are now supported — see ddl/alter_sequence.test.)
test("unsupported ALTER SEQUENCE actions are 0A000", () => {
  const db = new Engine();
  execute(db, "CREATE SEQUENCE s");
  assert.equal(errCode(db, "ALTER SEQUENCE s AS bigint"), "0A000");
  assert.equal(errCode(db, "ALTER SEQUENCE s OWNED BY t.c"), "0A000");
  assert.equal(errCode(db, "ALTER SEQUENCE s OWNER TO bob"), "0A000");
  assert.equal(errCode(db, "ALTER SEQUENCE s SET SCHEMA other"), "0A000");
  // ALTER of a non-sequence object is not a known statement at all → 42601 (no escape hatch).
  assert.equal(errCode(db, "ALTER TABLE t ADD COLUMN c i32"), "42601");
});

// An ALTER SEQUENCE … <options> edit is a transactional catalog write — it rolls back with its block
// (the §5 divergence applies to every ALTER action, not just RESTART). A jed-vs-PG divergence, so a
// per-core unit test, not corpus.
test("ALTER SEQUENCE options roll back", () => {
  const db = new Engine();
  execute(db, "CREATE SEQUENCE s INCREMENT 1");
  execute(db, "BEGIN");
  execute(db, "ALTER SEQUENCE s INCREMENT BY 100");
  execute(db, "ROLLBACK");
  // The INCREMENT edit rolled back, so the step is still 1: setval to 5, next is 6 (not 105).
  execute(db, "SELECT setval('s', 5)");
  assert.equal(oneInt(db, "SELECT nextval('s')"), 6n);
});

// setval/ALTER … RESTART are writes — a READ ONLY transaction rejects each with 25006 (each in its
// own block, since the error poisons the block). lastval/currval (pure reads) are allowed.
test("setval/ALTER in read-only transaction is 25006", () => {
  const db = new Engine();
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

// ---------------------------------------------------------------------------
// S3 — serial / bigserial / smallserial (spec/design/sequences.md §12). These per-core tests cover
// what the PG-clean corpus cannot: the auto-named OWNED sequence, the DROP TABLE auto-drop surviving
// a reopen (file persistence of the owner link, v13), and the DROP SEQUENCE 2BP01. The PG-agreeing
// surface lives in suites/ddl/serial.test. Mirrors impl/rust/tests/sequence.rs.

// queryRows runs sql and returns its rows' int cells (throwing on a NULL/non-int cell).
function queryRows(db: Engine, sql: string): bigint[][] {
  const o = execute(db, sql);
  if (o.kind !== "query") throw new Error(`expected a query, got ${o.kind}`);
  return o.rows.map((r) =>
    r.map((v) => {
      if (v.kind !== "int") throw new Error(`expected int, got ${v.kind}`);
      return v.int;
    }),
  );
}

test("serial desugars to an owned sequence and auto-numbers from 1", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id serial PRIMARY KEY, b bigserial, s smallserial, v text)");
  const rows = queryRows(db, "INSERT INTO t (v) VALUES ('a'), ('b') RETURNING id, b, s");
  assert.deepEqual(rows, [
    [1n, 1n, 1n],
    [2n, 2n, 2n],
  ]);
  // The owned sequences exist under PG's derived names and keep advancing.
  assert.equal(oneInt(db, "SELECT nextval('t_id_seq')"), 3n);
  assert.equal(oneInt(db, "SELECT nextval('t_b_seq')"), 3n);
  assert.equal(oneInt(db, "SELECT nextval('t_s_seq')"), 3n);
});

test("serial column is NOT NULL; an explicit value overrides the default without advancing", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id serial PRIMARY KEY, v text)");
  assert.equal(errCode(db, "INSERT INTO t (id, v) VALUES (NULL, 'x')"), "23502");
  execute(db, "INSERT INTO t (id, v) VALUES (100, 'y')"); // sequence untouched
  assert.deepEqual(queryRows(db, "INSERT INTO t (v) VALUES ('z') RETURNING id"), [[1n]]);
});

test("an explicit DEFAULT on a serial column is 42601", () => {
  const db = new Engine();
  assert.equal(errCode(db, "CREATE TABLE t (id serial DEFAULT 5)"), "42601");
});

test("the serial auto-name collision-resolves with a numeric suffix", () => {
  const db = new Engine();
  execute(db, "CREATE SEQUENCE t_id_seq");
  execute(db, "CREATE TABLE t (id serial)");
  execute(db, "INSERT INTO t (id) VALUES (DEFAULT)");
  // t_id_seq (the manual one) was never advanced; t_id_seq1 produced the row's 1.
  assert.equal(oneInt(db, "SELECT nextval('t_id_seq1')"), 2n);
  assert.equal(oneInt(db, "SELECT nextval('t_id_seq')"), 1n);
});

test("DROP SEQUENCE of an owned sequence is 2BP01; DROP TABLE auto-drops it", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id serial PRIMARY KEY)");
  assert.equal(errCode(db, "DROP SEQUENCE t_id_seq"), "2BP01");
  execute(db, "DROP TABLE t");
  assert.equal(errCode(db, "SELECT nextval('t_id_seq')"), "42P01"); // auto-dropped
  execute(db, "CREATE SEQUENCE t_id_seq"); // the name is free to reuse
});

test("the owned-by link persists (format_version 13) — auto-drop survives a reopen", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-serial-"));
  const path = join(dir, "serial_owned_reopen.jed");
  try {
    const db = create(path);
    execute(db, "CREATE TABLE t (id serial PRIMARY KEY, v text)");
    execute(db, "INSERT INTO t (v) VALUES ('a')");
    commit(db);
    close(db);

    const db2 = open(path);
    // The owner link round-tripped: still 2BP01 to drop the sequence directly.
    assert.equal(errCode(db2, "DROP SEQUENCE t_id_seq"), "2BP01");
    execute(db2, "DROP TABLE t");
    assert.equal(errCode(db2, "SELECT nextval('t_id_seq')"), "42P01");
    close(db2);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("serial is recognized only in a column-type position — a CAST to it is undefined", () => {
  const db = new Engine();
  assert.equal(errCode(db, "SELECT 1::serial"), "42704");
});
