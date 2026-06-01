// DELETE: by predicate, no-WHERE (all rows), Kleene (NULL rows not matched), and the
// load-bearing no-PK rowid fix — DELETE then INSERT must not collide on a freed rowid.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, execute } from "../src/lib.ts";
import { dbWith, errCode, query } from "./util.ts";

function setup() {
  return dbWith([
    "CREATE TABLE t (id int32 PRIMARY KEY, v int16)",
    "INSERT INTO t VALUES (1, 10)",
    "INSERT INTO t VALUES (2, NULL)",
    "INSERT INTO t VALUES (3, 30)",
  ]);
}

test("delete by predicate removes only matching rows", () => {
  const db = setup();
  execute(db, "DELETE FROM t WHERE id = 2");
  assert.deepStrictEqual(query(db, "SELECT id FROM t ORDER BY id"), [["1"], ["3"]]);
});

test("no WHERE deletes every row", () => {
  const db = setup();
  execute(db, "DELETE FROM t");
  assert.deepStrictEqual(query(db, "SELECT id FROM t"), []);
});

test("Kleene: a NULL predicate does not match (only TRUE deletes)", () => {
  const db = setup();
  execute(db, "DELETE FROM t WHERE v = 10");
  // row 2 (v IS NULL) is NOT deleted: NULL = 10 is UNKNOWN.
  assert.deepStrictEqual(query(db, "SELECT id FROM t ORDER BY id"), [["2"], ["3"]]);
});

test("no-PK: DELETE then INSERT does not collide on a freed rowid", () => {
  const db = dbWith([
    "CREATE TABLE r (a int16)",
    "INSERT INTO r VALUES (1)",
    "INSERT INTO r VALUES (2)",
    "INSERT INTO r VALUES (3)",
  ]);
  execute(db, "DELETE FROM r WHERE a = 2");
  // The next rowid is monotonic (never reused), so this must NOT raise 23505.
  execute(db, "INSERT INTO r VALUES (4)");
  assert.deepStrictEqual(query(db, "SELECT a FROM r ORDER BY a"), [["1"], ["3"], ["4"]]);
});

test("delete from a missing table traps 42P01", () => {
  assert.equal(errCode(() => execute(new Database(), "DELETE FROM nope")), "42P01");
});
