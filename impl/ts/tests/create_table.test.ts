// CREATE TABLE: type resolution (canonical + aliases), single-PK / unique-name
// enforcement, and the rejected cases.

import assert from "node:assert/strict";
import { test } from "node:test";
import { typeScalar } from "../src/types.ts";
import { Database, loadEngine } from "../src/tooling.ts";
import { dbWith, errCode } from "./util.ts";
import { memDb } from "./mem_db.ts";

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
  const db = dbWith(["CREATE TABLE t (a smallint, b int, c integer, d bigint)"]);
  assert.deepStrictEqual(
    db.table("t")!.columns.map((c) => typeScalar(c.type)),
    ["i16", "i32", "i32", "i64"],
  );
});

test("bare names fold and host lookup is exact", () => {
  // A bare name folds to lowercase; the host API looks a name up exactly (grammar.md §3).
  const db = dbWith(["CREATE TABLE Foo (Id i32 PRIMARY KEY)"]);
  assert.ok(db.table("foo"));
  assert.equal(db.table("FOO"), undefined);
  assert.equal(db.table("foo")!.columns[0]!.name, "id");
  db.execute('CREATE TABLE "Foo" ("Id" i32 PRIMARY KEY)');
  assert.equal(db.table("Foo")!.columns[0]!.name, "Id");
});

// The on-disk catalog emits each name group in ascending BYTE order of the exact name
// (format_version 37): uppercase ASCII sorts before lowercase, so B < C < a, and the reload keeps
// every name exact (a bare lookup never reaches a mixed-case name).
test("catalog order is exact-name bytes across reload", () => {
  const db = dbWith([
    "CREATE TABLE a (id i32 PRIMARY KEY)",
    'CREATE TABLE "C" (id i32 PRIMARY KEY)',
    'CREATE TABLE "B" (id i32 PRIMARY KEY, "V" i32, v i32)',
    'CREATE INDEX "Zed" ON "B" (v)',
    'CREATE INDEX alpha ON "B" ("V")',
    'CREATE SEQUENCE "Seq"',
    "CREATE SEQUENCE aseq",
  ]);
  const image = db.toImage(8192, 1n);
  assert.deepEqual(loadEngine(image).tableNames(), ["B", "C", "a"]);
  const loaded = Database.fromImage(image);
  const b = loaded.table("B")!;
  assert.deepEqual(
    b.indexes.map((i) => i.name),
    ["Zed", "alpha"],
  );
  assert.equal(b.columns[1]!.name, "V");
  assert.equal(b.columns[2]!.name, "v");
  assert.equal(loaded.table("b"), undefined);
  loaded.execute(`SELECT nextval('"Seq"')`);
  assert.equal(
    errCode(() => loaded.execute("SELECT nextval('Seq')")),
    "42P01",
    "the bare text folds to seq",
  );
  // Re-serializing the catalog is byte-identical (the reader trusts the emitted order).
  assert.deepEqual(Database.fromImage(image).toImage(8192, 1n), image);
});

test("duplicate table name traps 42P07", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)"]);
  assert.equal(
    errCode(() => db.execute("CREATE TABLE t (id i32 PRIMARY KEY)")),
    "42P07",
  );
});

test("duplicate column name traps 42701", () => {
  assert.equal(
    errCode(() => memDb().session().execute("CREATE TABLE t (a i16, a i32)")),
    "42701",
  );
});

test("unknown type traps 42704", () => {
  assert.equal(
    errCode(() => memDb().session().execute("CREATE TABLE t (a notatype)")),
    "42704",
  );
});

test("two primary keys trap 42P16", () => {
  assert.equal(
    errCode(() =>
      memDb().session().execute("CREATE TABLE t (a i16 PRIMARY KEY, b i16 PRIMARY KEY)"),
    ),
    "42P16",
  );
});
