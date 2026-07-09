// SELECT: point lookup, ORDER BY (NULLs last), IS [NOT] NULL, three-valued WHERE,
// cross-type comparison via the promotion tower, and CAST range re-checking.

import assert from "node:assert/strict";
import { test } from "node:test";
import { dbWith, errCode } from "./util.ts";

function seed() {
  return dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, v i16)",
    "INSERT INTO t VALUES (1, 10)",
    "INSERT INTO t VALUES (2, NULL)",
    "INSERT INTO t VALUES (3, 30)",
  ]);
}

function limitDB() {
  return dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
    "INSERT INTO t VALUES (1, 10)",
    "INSERT INTO t VALUES (2, 20)",
    "INSERT INTO t VALUES (3, 30)",
    "INSERT INTO t VALUES (4, 40)",
    "INSERT INTO t VALUES (5, 50)",
  ]);
}

test("LIMIT/OFFSET window reduces produced cost (slice before projection)", () => {
  // ORDER BY on a NON-primary-key column (`v`) is a blocking sort the scan does not satisfy, so it
  // reads every row before windowing; only windowed rows charge row_produced:
  // 1 page_read (t is one leaf) + 5 scanned + 2 produced = 8 (spec/design/cost.md §3). (Ordering by
  // the PK instead short-circuits — pinned cross-core in query/limit_offset.test, cost 5.)
  const o = limitDB().execute("SELECT id FROM t ORDER BY v LIMIT 2");
  assert.equal(o.cost, 8n);
});

test("unknown column traps 42703", () => {
  assert.equal(
    errCode(() => seed().execute("SELECT nope FROM t")),
    "42703",
  );
});
