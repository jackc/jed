// Structured error fields (spec/design/error-fields.md) — the constraintName / tableName /
// columnName / dataTypeName diagnostics on EngineError, modeled on pgx's pgconn.PgError. Out of the
// conformance corpus's reach (it matches on code/prose, never on a structured field — CLAUDE.md
// §10), so this is the host-API surface test. Mirrors impl/rust/tests/error_fields.rs and
// impl/go/error_fields_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import { EngineError } from "../src/tooling.ts";
import { type Handle, dbWith } from "./util.ts";

// efErr runs sql on db and returns the EngineError it throws (failing if none / a non-EngineError).
function efErr(db: Handle, sql: string): EngineError {
  try {
    db.execute(sql);
  } catch (e) {
    if (e instanceof EngineError) return e;
    throw e;
  }
  throw new Error(`expected an EngineError from ${sql}`);
}

// 23505 on a PRIMARY KEY reports the derived <table>_pkey constraint + the table; the rendered
// message is unchanged (fields are additive metadata, not a message change).
test("error fields: unique violation (primary key)", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)", "INSERT INTO t VALUES (1)"]);
  const e = efErr(db, "INSERT INTO t VALUES (1)");
  assert.equal(e.code(), "23505");
  assert.equal(e.constraintName, "t_pkey");
  assert.equal(e.tableName, "t");
  assert.equal(e.columnName, undefined);
  assert.match(e.message, /duplicate key value violates unique constraint: t_pkey$/);
});

// 23505 on a named UNIQUE index reports the index (= constraint) name.
test("error fields: unique violation (secondary index)", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, email text)",
    "CREATE UNIQUE INDEX t_email_key ON t (email)",
    "INSERT INTO t VALUES (1, 'a')",
  ]);
  const e = efErr(db, "INSERT INTO t VALUES (2, 'a')");
  assert.equal(e.code(), "23505");
  assert.equal(e.constraintName, "t_email_key");
  assert.equal(e.tableName, "t");
});

// 23514 reports the CHECK constraint + the relation.
test("error fields: check violation", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, n i32 CONSTRAINT n_pos CHECK (n > 0))"]);
  const e = efErr(db, "INSERT INTO t VALUES (1, -1)");
  assert.equal(e.code(), "23514");
  assert.equal(e.constraintName, "n_pos");
  assert.equal(e.tableName, "t");
});

// 23503 (child side) reports the FK constraint + the written table.
test("error fields: foreign key violation (insert)", () => {
  const db = dbWith([
    "CREATE TABLE p (id i32 PRIMARY KEY)",
    "CREATE TABLE c (id i32 PRIMARY KEY, pid i32 CONSTRAINT c_pid_fk REFERENCES p)",
  ]);
  const e = efErr(db, "INSERT INTO c VALUES (1, 99)");
  assert.equal(e.code(), "23503");
  assert.equal(e.constraintName, "c_pid_fk");
  assert.equal(e.tableName, "c");
});

// 23503 (parent side) reports the FK constraint + the modified (parent) table.
test("error fields: foreign key violation (delete)", () => {
  const db = dbWith([
    "CREATE TABLE p (id i32 PRIMARY KEY)",
    "CREATE TABLE c (id i32 PRIMARY KEY, pid i32 CONSTRAINT c_pid_fk REFERENCES p)",
    "INSERT INTO p VALUES (1)",
    "INSERT INTO c VALUES (1, 1)",
  ]);
  const e = efErr(db, "DELETE FROM p WHERE id = 1");
  assert.equal(e.code(), "23503");
  assert.equal(e.constraintName, "c_pid_fk");
  assert.equal(e.tableName, "p");
});

// 23P01 reports the EXCLUDE constraint (its backing GiST index name) + the table.
test("error fields: exclusion violation", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, r i32range, CONSTRAINT t_r_excl EXCLUDE USING gist (r WITH &&))",
    "INSERT INTO t VALUES (1, '[1,5)')",
  ]);
  const e = efErr(db, "INSERT INTO t VALUES (2, '[3,8)')");
  assert.equal(e.code(), "23P01");
  assert.equal(e.constraintName, "t_r_excl");
  assert.equal(e.tableName, "t");
});

// 23502 reports the column (unnamed constraint, as in PostgreSQL); the table is stamped at the DML
// boundary.
test("error fields: not-null violation", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, n i32 NOT NULL)"]);
  const e = efErr(db, "INSERT INTO t VALUES (1, NULL)");
  assert.equal(e.code(), "23502");
  assert.equal(e.columnName, "n");
  assert.equal(e.tableName, "t");
  assert.equal(e.constraintName, undefined);
});

// 22003 (integer overflow on column store) reports the data type + the table.
test("error fields: numeric value out of range", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, n i16)"]);
  const e = efErr(db, "INSERT INTO t VALUES (1, 99999)");
  assert.equal(e.code(), "22003");
  assert.equal(e.dataTypeName, "i16");
  assert.equal(e.tableName, "t");
});

// 22001 (varchar length) reports the type + the column.
test("error fields: string data right truncation", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, s varchar(3))"]);
  const e = efErr(db, "INSERT INTO t VALUES (1, 'abcd')");
  assert.equal(e.code(), "22001");
  assert.equal(e.dataTypeName, "varchar(3)");
  assert.equal(e.columnName, "s");
  assert.equal(e.tableName, "t");
});

// A non-constraint error leaves every structured field unset.
test("error fields: unrelated error has no fields", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)"]);
  const e = efErr(db, "SELECT nonesuch FROM t");
  assert.equal(e.constraintName, undefined);
  assert.equal(e.tableName, undefined);
  assert.equal(e.columnName, undefined);
  assert.equal(e.dataTypeName, undefined);
});
