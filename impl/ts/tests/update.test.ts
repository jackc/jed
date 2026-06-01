// UPDATE: in-place replacement, old-row assignment semantics (swap), the two-phase
// all-or-nothing guarantee, and the rejected cases (PK column, duplicate target,
// overflow, not-null).

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, execute } from "../src/lib.ts";
import { dbWith, errCode, query } from "./util.ts";

function setup() {
  return dbWith([
    "CREATE TABLE t (id int32 PRIMARY KEY, a int16, b int16)",
    "INSERT INTO t VALUES (1, 10, 11)",
    "INSERT INTO t VALUES (2, 20, 22)",
    "INSERT INTO t VALUES (3, 30, 33)",
  ]);
}

test("update one row by key, leaving others untouched", () => {
  const db = setup();
  execute(db, "UPDATE t SET a = 99 WHERE id = 2");
  assert.deepStrictEqual(query(db, "SELECT a FROM t WHERE id = 2"), [["99"]]);
  assert.deepStrictEqual(query(db, "SELECT a FROM t WHERE id = 1"), [["10"]]);
});

test("assignments read the OLD row, so SET a=b, b=a swaps", () => {
  const db = setup();
  execute(db, "UPDATE t SET a = b, b = a WHERE id = 1");
  assert.deepStrictEqual(query(db, "SELECT a, b FROM t WHERE id = 1"), [["11", "10"]]);
});

test("no WHERE touches every row", () => {
  const db = setup();
  execute(db, "UPDATE t SET b = 0");
  assert.deepStrictEqual(query(db, "SELECT b FROM t ORDER BY id"), [["0"], ["0"], ["0"]]);
});

test("update to NULL in a nullable column", () => {
  const db = setup();
  execute(db, "UPDATE t SET a = NULL WHERE id = 3");
  assert.deepStrictEqual(query(db, "SELECT a FROM t WHERE id = 3"), [["NULL"]]);
});

test("updating a primary key column is unsupported (0A000), row unchanged", () => {
  const db = setup();
  assert.equal(errCode(() => execute(db, "UPDATE t SET id = 5 WHERE id = 2")), "0A000");
  assert.deepStrictEqual(query(db, "SELECT id FROM t WHERE id = 2"), [["2"]]);
});

test("duplicate target column traps 42701", () => {
  assert.equal(errCode(() => execute(setup(), "UPDATE t SET a = 1, a = 2 WHERE id = 1")), "42701");
});

test("overflow traps 22003 and leaves the row unchanged", () => {
  const db = setup();
  assert.equal(errCode(() => execute(db, "UPDATE t SET a = 40000 WHERE id = 2")), "22003");
  assert.deepStrictEqual(query(db, "SELECT a FROM t WHERE id = 2"), [["20"]]);
});

test("update is all-or-nothing across rows", () => {
  // Row 2's source overflows int16, so NO row is modified — not even rows 1 and 3.
  const db = dbWith([
    "CREATE TABLE m (id int32 PRIMARY KEY, n int16, src int64)",
    "INSERT INTO m VALUES (1, 1, 5)",
    "INSERT INTO m VALUES (2, 2, 99999)",
    "INSERT INTO m VALUES (3, 3, 7)",
  ]);
  assert.equal(errCode(() => execute(db, "UPDATE m SET n = src")), "22003");
  assert.deepStrictEqual(query(db, "SELECT n FROM m ORDER BY id"), [["1"], ["2"], ["3"]]);
});

test("unknown column traps 42703; missing table traps 42P01", () => {
  assert.equal(errCode(() => execute(setup(), "UPDATE t SET nope = 1")), "42703");
  assert.equal(errCode(() => execute(new Database(), "UPDATE nope SET a = 1")), "42P01");
});
