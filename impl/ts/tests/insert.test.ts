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
