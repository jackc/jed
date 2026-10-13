// INSERT ... ON CONFLICT (UPSERT) — the pieces the oracle corpus
// (spec/conformance/suites/dml/insert_on_conflict.test) cannot express: the jed-specific
// divergences from PostgreSQL (spec/design/upsert.md §9) and the affected-row count (the
// command-tag count a `statement ok` does not assert). The PG-agreeing behavior — DO NOTHING /
// DO UPDATE, arbiter inference / ON CONSTRAINT, the 21000 second-affect rule, non-arbiter 23505 —
// is the corpus's job. Mirrors impl/rust/tests/on_conflict.rs and impl/go/on_conflict_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import { type Handle, dbWith, errCode, queryOutcome } from "./util.ts";

function affected(db: Handle, sql: string): number | null {
  const out = queryOutcome(db, sql);
  assert.equal(out.kind, "statement", `expected a statement outcome from ${sql}`);
  return out.kind === "statement" ? out.rowsAffected : null;
}

// DIVERGENCE (upsert.md §9): assigning a PRIMARY KEY column in DO UPDATE is still 0A000 — a
// deferred follow-on. The standalone UPDATE re-keying has landed (§11 step 6); extending it to
// the upsert conflict path is separate. PostgreSQL allows it.
test("ON CONFLICT DO UPDATE assigning a PK column is 0A000", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, v i32)", "INSERT INTO t VALUES (1, 10)"]);
  assert.equal(
    errCode(() =>
      db.execute(
        "INSERT INTO t VALUES (1, 5) ON CONFLICT (id) DO UPDATE SET id = excluded.id + 100",
      ),
    ),
    "0A000",
  );
});

// The affected-row count (api.md §4) the corpus's `statement ok` cannot assert: an ON CONFLICT
// counts the inserted + updated rows; rows skipped by DO NOTHING (or a DO UPDATE WHERE that is
// false) are not counted.
test("ON CONFLICT affected-row counts", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
    "INSERT INTO t VALUES (1, 10), (2, 20)",
  ]);
  // DO NOTHING over a batch: id 1 conflicts (skip), id 3 inserts → 1 affected.
  assert.equal(affected(db, "INSERT INTO t VALUES (1, 99), (3, 30) ON CONFLICT DO NOTHING"), 1);
  // DO UPDATE: id 2 updates, id 4 inserts → 2 affected.
  assert.equal(
    affected(
      db,
      "INSERT INTO t VALUES (2, 22), (4, 40) ON CONFLICT (id) DO UPDATE SET v = excluded.v",
    ),
    2,
  );
  // All conflict and are skipped → 0 affected.
  assert.equal(affected(db, "INSERT INTO t VALUES (1, 0), (2, 0) ON CONFLICT DO NOTHING"), 0);
  // A DO UPDATE WHERE that is false updates nothing → 0 affected.
  assert.equal(
    affected(
      db,
      "INSERT INTO t VALUES (1, 7) ON CONFLICT (id) DO UPDATE SET v = excluded.v WHERE false",
    ),
    0,
  );
});
