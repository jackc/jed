// CREATE TABLE: type resolution (canonical + aliases), single-PK / unique-name
// enforcement, and the rejected cases.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, execute } from "../src/lib.ts";
import { typeScalar } from "../src/types.ts";
import { dbWith, errCode } from "./util.ts";

test("create table then describe via the catalog", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, a i16, b i64)"]);
  const t = db.table("t");
  assert.ok(t);
  assert.deepStrictEqual(
    t!.columns.map((c) => [c.name, typeScalar(c.type), c.primaryKey, c.notNull]),
    [
      ["id", "i32", true, true], // PRIMARY KEY ⇒ NOT NULL
      ["a", "i16", false, false],
      ["b", "i64", false, false],
    ],
  );
});

test("SQL-standard type aliases resolve to canonical types", () => {
  const db = dbWith([
    "CREATE TABLE t (a smallint, b int, c integer, d bigint)",
  ]);
  assert.deepStrictEqual(
    db.table("t")!.columns.map((c) => typeScalar(c.type)),
    ["i16", "i32", "i32", "i64"],
  );
});

test("case-insensitive table lookup", () => {
  const db = dbWith(["CREATE TABLE Foo (id i32 PRIMARY KEY)"]);
  assert.ok(db.table("foo"));
  assert.ok(db.table("FOO"));
});

test("duplicate table name traps 42P07", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)"]);
  assert.equal(errCode(() => execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)")), "42P07");
});

test("duplicate column name traps 42701", () => {
  assert.equal(
    errCode(() => execute(new Database(), "CREATE TABLE t (a i16, a i32)")),
    "42701",
  );
});

test("unknown type traps 42704", () => {
  assert.equal(
    errCode(() => execute(new Database(), "CREATE TABLE t (a notatype)")),
    "42704",
  );
});

test("two primary keys trap 42P16", () => {
  assert.equal(
    errCode(() => execute(new Database(), "CREATE TABLE t (a i16 PRIMARY KEY, b i16 PRIMARY KEY)")),
    "42P16",
  );
});
