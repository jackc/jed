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
