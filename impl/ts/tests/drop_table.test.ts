// DROP TABLE — remove a table (its definition + all its rows) from the catalog. The
// inverse of CREATE TABLE: a missing table is 42P01 and there is no IF EXISTS this
// slice; single table, no CASCADE/RESTRICT (spec/design/grammar.md §13).

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, execute } from "../src/lib.ts";
import { dbWith, errCode, query } from "./util.ts";

test("drop removes the table and its rows", () => {
  const db = dbWith([
    "CREATE TABLE t (id int32 PRIMARY KEY, v int16)",
    "INSERT INTO t VALUES (1, 10), (2, 20)",
  ]);
  const out = execute(db, "DROP TABLE t");
  assert.deepStrictEqual(out, { kind: "statement", cost: 0n, rowsAffected: null });
  assert.equal(db.table("t"), undefined);
  assert.deepStrictEqual(db.rowsInKeyOrder("t"), []);
});

test("every access path against a dropped table traps 42P01", () => {
  const db = dbWith(["CREATE TABLE t (id int32 PRIMARY KEY, v int16)", "DROP TABLE t"]);
  for (const sql of [
    "SELECT id FROM t",
    "INSERT INTO t VALUES (1, 1)",
    "UPDATE t SET v = 0",
    "DELETE FROM t",
  ]) {
    assert.equal(errCode(() => execute(db, sql)), "42P01", sql);
  }
});

test("dropping a missing table traps 42P01 (no IF EXISTS this slice)", () => {
  const db = new Database();
  assert.equal(errCode(() => execute(db, "DROP TABLE nope")), "42P01");
  execute(db, "CREATE TABLE t (id int32 PRIMARY KEY)");
  execute(db, "DROP TABLE t");
  assert.equal(errCode(() => execute(db, "DROP TABLE t")), "42P01");
});

test("the name is free to re-create after a drop", () => {
  const db = dbWith([
    "CREATE TABLE t (id int32 PRIMARY KEY, v int16)",
    "INSERT INTO t VALUES (1, 10)",
    "DROP TABLE t",
    "CREATE TABLE t (id int32 PRIMARY KEY, w int64)",
  ]);
  assert.deepStrictEqual(db.rowsInKeyOrder("t"), []);
  assert.equal(db.table("t")!.columns[1]!.name, "w");
});

test("drop is case-insensitive on the table name", () => {
  const db = dbWith(["create table T (id int32 primary key)", "DROP TABLE t"]);
  assert.equal(db.table("t"), undefined);
});

test("dropping one table leaves the others intact", () => {
  const db = dbWith([
    "CREATE TABLE a (id int32 PRIMARY KEY)",
    "CREATE TABLE b (id int32 PRIMARY KEY)",
    "INSERT INTO b VALUES (2)",
    "DROP TABLE a",
  ]);
  assert.equal(db.table("a"), undefined);
  assert.ok(db.table("b"));
  assert.deepStrictEqual(query(db, "SELECT id FROM b"), [["2"]]);
});

test("DROP TABLE syntax errors trap 42601", () => {
  const db = dbWith(["CREATE TABLE t (id int32 PRIMARY KEY)"]);
  assert.equal(errCode(() => execute(db, "DROP TABLE")), "42601"); // no table name
  assert.equal(errCode(() => execute(db, "DROP TABLE t extra")), "42601"); // trailing input
  // DROP INDEX is its own statement now (spec/design/indexes.md §2): a missing index is
  // 42704, not a syntax error; DROP of any other object kind is still unparsed.
  assert.equal(errCode(() => execute(db, "DROP INDEX x")), "42704");
  assert.equal(errCode(() => execute(db, "DROP VIEW v")), "42601");
});
