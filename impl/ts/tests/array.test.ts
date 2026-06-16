// Array types (spec/design/array.md) — the S1–S4 vertical slice: a structural int32[] column, the
// ARRAY[…] constructor + the '{…}' literal, the compact value codec (S2), btree-NULL element
// comparison / ORDER BY / DISTINCT (S4), and array_out rendering. Mirrors impl/rust/tests/array.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, execute, loadDatabase, toImage } from "../src/lib.ts";
import { errCode, query } from "./util.ts";

function run(db: Database, sql: string): void {
  execute(db, sql);
}

test("array column round-trips ARRAY[…] and '{…}'", () => {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[], tags text[])");
  run(db, "INSERT INTO t VALUES (1, ARRAY[10, 20, 30], ARRAY['a', 'b'])");
  run(db, "INSERT INTO t VALUES (2, '{40,50}', '{}')");
  assert.deepStrictEqual(query(db, "SELECT id, xs, tags FROM t ORDER BY id"), [
    ["1", "{10,20,30}", "{a,b}"],
    ["2", "{40,50}", "{}"],
  ]);
});

test("array column survives a whole-image round-trip", () => {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[], tags text[])");
  run(db, "INSERT INTO t VALUES (1, ARRAY[10, 20, 30], ARRAY['a', 'b'])");
  run(db, "INSERT INTO t VALUES (2, ARRAY[1, NULL, 3], '{}')");
  run(db, "INSERT INTO t VALUES (3, NULL, NULL)");
  const loaded = loadDatabase(toImage(db, 4096, 1n));
  assert.deepStrictEqual(query(loaded, "SELECT id, xs, tags FROM t ORDER BY id"), [
    ["1", "{10,20,30}", "{a,b}"],
    ["2", "{1,NULL,3}", "{}"],
    ["3", "NULL", "NULL"],
  ]);
});

test("array NULL levels are distinct; IS NULL is whole-value", () => {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])");
  run(db, "INSERT INTO t VALUES (1, ARRAY[1, NULL, 3])");
  run(db, "INSERT INTO t VALUES (2, NULL)");
  run(db, "INSERT INTO t VALUES (3, '{}')");
  assert.deepStrictEqual(query(db, "SELECT xs FROM t ORDER BY id"), [["{1,NULL,3}"], ["NULL"], ["{}"]]);
  assert.deepStrictEqual(query(db, "SELECT id FROM t WHERE xs IS NULL ORDER BY id"), [["2"]]);
  assert.deepStrictEqual(query(db, "SELECT id FROM t WHERE xs IS NOT NULL ORDER BY id"), [["1"], ["3"]]);
});

test("array equality uses btree (not 3VL) NULL semantics", () => {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])");
  run(db, "INSERT INTO t VALUES (1, ARRAY[1, 2, 3])");
  run(db, "INSERT INTO t VALUES (2, ARRAY[1, NULL, 3])");
  run(db, "INSERT INTO t VALUES (3, ARRAY[1, 2])");
  assert.deepStrictEqual(query(db, "SELECT id FROM t WHERE xs = ARRAY[1,2,3]"), [["1"]]);
  // {1,NULL,3} = {1,NULL,3} is TRUE (NULLs mutually equal — not UNKNOWN).
  assert.deepStrictEqual(query(db, "SELECT id FROM t WHERE xs = ARRAY[1,NULL,3]"), [["2"]]);
  assert.deepStrictEqual(query(db, "SELECT id FROM t WHERE xs = ARRAY[1,2]"), [["3"]]);
});

test("array ORDER BY is element-wise, shorter prefix first", () => {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])");
  run(db, "INSERT INTO t VALUES (1, ARRAY[1, 2, 3])");
  run(db, "INSERT INTO t VALUES (2, ARRAY[1, 2])");
  run(db, "INSERT INTO t VALUES (3, ARRAY[1, 3])");
  run(db, "INSERT INTO t VALUES (4, ARRAY[1])");
  assert.deepStrictEqual(query(db, "SELECT xs FROM t ORDER BY xs"), [["{1}"], ["{1,2}"], ["{1,2,3}"], ["{1,3}"]]);
});

test("DISTINCT over arrays dedups by structural equality", () => {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])");
  run(db, "INSERT INTO t VALUES (1, ARRAY[1, 2])");
  run(db, "INSERT INTO t VALUES (2, ARRAY[1, 2])");
  run(db, "INSERT INTO t VALUES (3, ARRAY[3])");
  const got = query(db, "SELECT DISTINCT xs FROM t")
    .map((r) => r[0]!)
    .sort();
  assert.deepStrictEqual(got, ["{1,2}", "{3}"]);
});

test("array_out quoting matches PG (backslash-escaped)", () => {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, tags text[])");
  run(db, `INSERT INTO t VALUES (1, ARRAY['a,b', '', 'NULL', 'x"y'])`);
  assert.deepStrictEqual(query(db, "SELECT tags FROM t"), [[`{"a,b","","NULL","x\\"y"}`]]);
});

test("over-range array element traps 22003", () => {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int16[])");
  assert.equal(errCode(() => run(db, "INSERT INTO t VALUES (1, ARRAY[100000])")), "22003");
});

test("array PRIMARY KEY is 0A000", () => {
  const db = new Database();
  assert.equal(errCode(() => run(db, "CREATE TABLE t (xs int32[] PRIMARY KEY)")), "0A000");
});

test("malformed array literal is 22P02", () => {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])");
  assert.equal(errCode(() => run(db, "INSERT INTO t VALUES (1, '{1,2')")), "22P02");
});

test("comparing arrays of different element types is 42804", () => {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[], ts text[])");
  run(db, "INSERT INTO t VALUES (1, ARRAY[1], ARRAY['a'])");
  assert.equal(errCode(() => run(db, "SELECT id FROM t WHERE xs = ts")), "42804");
});

// S3: a[i] is 1-based; the element type is the column's element type (spec/design/array.md §6).
test("subscript is 1-based", () => {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[], tags text[])");
  run(db, "INSERT INTO t VALUES (1, ARRAY[10, 20, 30], ARRAY['a', 'b'])");
  assert.deepStrictEqual(query(db, "SELECT xs[1] FROM t"), [["10"]]);
  assert.deepStrictEqual(query(db, "SELECT xs[3] FROM t"), [["30"]]);
  assert.deepStrictEqual(query(db, "SELECT tags[2] FROM t"), [["b"]]);
});

// S3: an out-of-bounds subscript (0, negative, or past the end) yields NULL — never an error (PG).
test("subscript out of bounds is NULL", () => {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])");
  run(db, "INSERT INTO t VALUES (1, ARRAY[10, 20, 30])");
  assert.deepStrictEqual(query(db, "SELECT xs[0] FROM t"), [["NULL"]]);
  assert.deepStrictEqual(query(db, "SELECT xs[4] FROM t"), [["NULL"]]);
  assert.deepStrictEqual(query(db, "SELECT xs[-1] FROM t"), [["NULL"]]);
});

// S3: a NULL subscript, a subscript of a NULL array, and a subscript reading a NULL element all NULL.
test("subscript NULL cases are NULL", () => {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])");
  run(db, "INSERT INTO t VALUES (1, ARRAY[1, NULL, 3])");
  run(db, "INSERT INTO t VALUES (2, NULL)");
  assert.deepStrictEqual(query(db, "SELECT xs[NULL] FROM t WHERE id = 1"), [["NULL"]]); // NULL index
  assert.deepStrictEqual(query(db, "SELECT xs[1] FROM t WHERE id = 2"), [["NULL"]]); // NULL array
  assert.deepStrictEqual(query(db, "SELECT xs[2] FROM t WHERE id = 1"), [["NULL"]]); // NULL element
});

// S3: subscripting a non-array base is 42804 at resolve.
test("subscript of a non-array is 42804", () => {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, n int32)");
  run(db, "INSERT INTO t VALUES (1, 5)");
  assert.equal(errCode(() => run(db, "SELECT n[1] FROM t")), "42804");
});

// S3: the index can be an arbitrary integer expression, and an ARRAY[…] constructor subscripts directly.
test("subscript with expression index and constructor base", () => {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])");
  run(db, "INSERT INTO t VALUES (1, ARRAY[10, 20, 30])");
  assert.deepStrictEqual(query(db, "SELECT xs[1 + 1] FROM t"), [["20"]]);
  assert.deepStrictEqual(query(db, "SELECT (ARRAY[100, 200, 300])[3] FROM t"), [["300"]]);
});
