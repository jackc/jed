// Byte-level ALTER TABLE rewrite checks that the shared SQL corpus cannot express.

import assert from "node:assert/strict";
import { test } from "node:test";
import { memDb } from "./mem_db.ts";
import { queryOutcome } from "./util.ts";

test("ADD COLUMN rewrite matches equivalent fresh-table bytes", () => {
  const altered = memDb().session();
  queryOutcome(altered, "CREATE TABLE t (id i32 PRIMARY KEY)");
  queryOutcome(altered, "INSERT INTO t VALUES (1), (2)");
  queryOutcome(altered, "ALTER TABLE t ADD v i32 DEFAULT 7");

  const fresh = memDb().session();
  queryOutcome(fresh, "CREATE TABLE t (id i32 PRIMARY KEY, v i32 DEFAULT 7)");
  queryOutcome(fresh, "INSERT INTO t (id) VALUES (1), (2)");

  assert.deepEqual(altered.toImage(8192, 1n), fresh.toImage(8192, 1n));
});

test("DROP COLUMN rewrite matches equivalent fresh-table bytes", () => {
  const altered = memDb().session();
  queryOutcome(altered, "CREATE TABLE t (obsolete text, id i32 PRIMARY KEY, v i32 DEFAULT 7)");
  queryOutcome(altered, "INSERT INTO t VALUES ('a', 1, 7), ('b', 2, 8)");
  queryOutcome(altered, "ALTER TABLE t DROP obsolete");

  const fresh = memDb().session();
  queryOutcome(fresh, "CREATE TABLE t (id i32 PRIMARY KEY, v i32 DEFAULT 7)");
  queryOutcome(fresh, "INSERT INTO t VALUES (1, 7), (2, 8)");

  assert.deepEqual(altered.toImage(8192, 1n), fresh.toImage(8192, 1n));
});

test("ALTER TYPE and PRIMARY KEY rewrites match equivalent fresh-table bytes", () => {
  const image = (...sqls: string[]): Uint8Array => {
    const db = memDb().session();
    for (const sql of sqls) queryOutcome(db, sql);
    return db.toImage(8192, 1n);
  };
  assert.deepEqual(
    image(
      "CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
      "INSERT INTO t VALUES (1, 2), (2, 3)",
      "ALTER TABLE t ALTER v TYPE i64 USING v + 10",
    ),
    image("CREATE TABLE t (id i32 PRIMARY KEY, v i64)", "INSERT INTO t VALUES (1, 12), (2, 13)"),
  );
  assert.deepEqual(
    image(
      "CREATE TABLE t (id i32 NOT NULL, v text)",
      "INSERT INTO t VALUES (2, 'b'), (1, 'a')",
      "ALTER TABLE t ADD PRIMARY KEY (id)",
      "ALTER TABLE t DROP PRIMARY KEY",
    ),
    image("CREATE TABLE t (id i32 NOT NULL, v text)", "INSERT INTO t VALUES (1, 'a'), (2, 'b')"),
  );
});
