// UPDATE: value replacement, old-row assignment semantics (swap), the two-phase
// all-or-nothing guarantee, the rejected cases (duplicate target, overflow, not-null), and
// PRIMARY KEY re-keying (§11 step 6). The PG-divergent re-keying cases (an end-state-valid
// key swap / cascade that PG rejects on the per-row transient) live here rather than the
// oracle corpus, the same divergence UNIQUE carries (indexes.md §8).

import assert from "node:assert/strict";
import { test } from "node:test";
import { type Handle, dbWith, errCode } from "./util.ts";
import { memDb } from "./mem_db.ts";

function setup() {
  return dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, a i16, b i16)",
    "INSERT INTO t VALUES (1, 10, 11)",
    "INSERT INTO t VALUES (2, 20, 22)",
    "INSERT INTO t VALUES (3, 30, 33)",
  ]);
}

// The (id, a, b) rows of t in storage-key order as "id/a/b" strings, for end-state asserts.
function idsABC(db: Handle): string[] {
  return db.rowsInKeyOrder("t").map((r) =>
    r
      .map((c) => {
        if (c.kind !== "int") throw new Error("expected int");
        return c.int.toString();
      })
      .join("/"),
  );
}

test("unknown column traps 42703; missing table traps 42P01", () => {
  assert.equal(
    errCode(() => setup().execute("UPDATE t SET nope = 1")),
    "42703",
  );
  assert.equal(
    errCode(() => memDb().session().execute("UPDATE nope SET a = 1")),
    "42P01",
  );
});

// Re-keying validates against the statement's END STATE (like UNIQUE, indexes.md §8): a
// swap of two primary keys keeps both keys present, so jed accepts it — where PostgreSQL's
// per-row check fails on the transient collision. Each row's non-key columns move with it.
test("PK swap is end-state valid (jed accepts, PG rejects the transient)", () => {
  const db = setup();
  db.execute("UPDATE t SET id = 3 - id WHERE id <= 2");
  assert.deepEqual(idsABC(db), ["1/20/22", "2/10/11", "3/30/33"]);
});

// A cascade that shifts every key up by one is likewise end-state-valid, so jed re-keys all
// three rows — where PostgreSQL rejects the per-row transient (id 1 → 2 while 2 still exists).
test("PK increment cascade succeeds", () => {
  const db = setup();
  db.execute("UPDATE t SET id = id + 1");
  assert.deepEqual(idsABC(db), ["2/10/11", "3/20/22", "4/30/33"]);
});

// Re-keying onto a DISTINCT existing (non-updated) row's key collides — 23505, all-or-nothing.
test("PK collision with an existing row traps 23505", () => {
  const db = setup();
  assert.equal(
    errCode(() => db.execute("UPDATE t SET id = 3 WHERE id = 1")),
    "23505",
  );
  assert.deepEqual(idsABC(db), ["1/10/11", "2/20/22", "3/30/33"]);
});
