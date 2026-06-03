// INSERT: positional type-checking, the overflow / not-null / duplicate-key traps, and
// no-PK synthetic rowid behaviour. int64 extremes must round-trip exactly (the bigint
// path — the dimension this core exists to exercise).

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, execute } from "../src/lib.ts";
import { dbWith, errCode, query } from "./util.ts";

function nums(): Database {
  return dbWith(["CREATE TABLE nums (id int32 PRIMARY KEY, small int16, big int64)"]);
}

test("insert round-trips int64 extremes exactly", () => {
  const db = nums();
  execute(db, "INSERT INTO nums VALUES (1, -32768, -9223372036854775808)");
  execute(db, "INSERT INTO nums VALUES (2, 32767, 9223372036854775807)");
  assert.deepStrictEqual(query(db, "SELECT small, big FROM nums ORDER BY id"), [
    ["-32768", "-9223372036854775808"],
    ["32767", "9223372036854775807"],
  ]);
});

test("wrong number of values traps 42601", () => {
  const db = nums();
  assert.equal(errCode(() => execute(db, "INSERT INTO nums VALUES (1, 2)")), "42601");
});

test("NULL into a NOT NULL (primary key) column traps 23502", () => {
  const db = nums();
  assert.equal(
    errCode(() => execute(db, "INSERT INTO nums VALUES (NULL, 1, 1)")),
    "23502",
  );
});

test("out-of-range value traps 22003", () => {
  const db = nums();
  // 40000 exceeds int16 max (32767)
  assert.equal(
    errCode(() => execute(db, "INSERT INTO nums VALUES (1, 40000, 1)")),
    "22003",
  );
});

test("duplicate primary key traps 23505", () => {
  const db = nums();
  execute(db, "INSERT INTO nums VALUES (1, 1, 1)");
  assert.equal(errCode(() => execute(db, "INSERT INTO nums VALUES (1, 2, 2)")), "23505");
});

test("a nullable non-PK column accepts NULL", () => {
  const db = nums();
  execute(db, "INSERT INTO nums VALUES (1, NULL, NULL)");
  assert.deepStrictEqual(query(db, "SELECT small, big FROM nums WHERE id = 1"), [["NULL", "NULL"]]);
});

test("no-PK table accepts repeated rows (synthetic rowid)", () => {
  const db = dbWith(["CREATE TABLE r (a int16)"]);
  execute(db, "INSERT INTO r VALUES (5)");
  execute(db, "INSERT INTO r VALUES (5)");
  assert.equal(query(db, "SELECT a FROM r").length, 2);
});

test("insert into a missing table traps 42P01", () => {
  assert.equal(
    errCode(() => execute(new Database(), "INSERT INTO nope VALUES (1)")),
    "42P01",
  );
});

// --- multi-row INSERT (spec/design/grammar.md §12) --------------------------------

test("multi-row INSERT stores all rows in key order", () => {
  const db = nums();
  // One statement, rows out of key order; storage yields them in PK order.
  execute(db, "INSERT INTO nums VALUES (3, 30, 300), (1, 10, 100), (2, 20, 200)");
  assert.deepStrictEqual(query(db, "SELECT id FROM nums ORDER BY id"), [["1"], ["2"], ["3"]]);
});

test("multi-row INSERT is all-or-nothing on overflow", () => {
  const db = nums();
  // The second row overflows int16 — the whole statement fails, storing nothing.
  assert.equal(
    errCode(() => execute(db, "INSERT INTO nums VALUES (1, 10, 100), (2, 99999, 200)")),
    "22003",
  );
  assert.equal(query(db, "SELECT id FROM nums").length, 0);
});

test("multi-row INSERT duplicate key within the batch traps 23505 and stores nothing", () => {
  const db = nums();
  assert.equal(
    errCode(() => execute(db, "INSERT INTO nums VALUES (1, 1, 1), (1, 2, 2)")),
    "23505",
  );
  assert.equal(query(db, "SELECT id FROM nums").length, 0);
});

test("multi-row INSERT duplicate against a stored row traps 23505, leaving it alone", () => {
  const db = nums();
  execute(db, "INSERT INTO nums VALUES (1, 1, 1)");
  // The batch's second row collides with stored row 1; the new row 2 must not land.
  assert.equal(
    errCode(() => execute(db, "INSERT INTO nums VALUES (2, 2, 2), (1, 9, 9)")),
    "23505",
  );
  assert.deepStrictEqual(query(db, "SELECT id FROM nums ORDER BY id"), [["1"]]);
});

test("multi-row INSERT with a wrong-arity row traps 42601 and stores nothing", () => {
  const db = nums();
  assert.equal(
    errCode(() => execute(db, "INSERT INTO nums VALUES (1, 1, 1), (2, 2)")),
    "42601",
  );
  assert.equal(query(db, "SELECT id FROM nums").length, 0);
});

test("no-PK multi-row INSERT keeps insertion order; a failed batch stores nothing", () => {
  const db = dbWith(["CREATE TABLE log (a int16)"]);
  // No PK ⇒ monotonic synthetic rowids, allocated left-to-right; key order = insertion order.
  execute(db, "INSERT INTO log VALUES (30), (10), (20)");
  assert.deepStrictEqual(query(db, "SELECT a FROM log"), [["30"], ["10"], ["20"]]);
  // A failing batch (second row overflows) stores neither row.
  assert.equal(errCode(() => execute(db, "INSERT INTO log VALUES (40), (99999)")), "22003");
  assert.deepStrictEqual(query(db, "SELECT a FROM log"), [["30"], ["10"], ["20"]]);
});
