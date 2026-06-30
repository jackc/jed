// Cooperative cancellation via AbortSignal (spec/design/api.md §11.4). Per-core unit tests, NOT the
// shared corpus: cancellation is timing-dependent (CLAUDE.md §10), so it cannot live there. The TS
// divergence (cancel.ts): execution is synchronous (one event loop), so an AbortSignal cannot flip
// MID-statement — TS honors it at OPERATION BOUNDARIES only (Go/Rust poll the cost meter mid-run).
// These pin that boundary behavior deterministically.

import assert from "node:assert/strict";
import { test } from "node:test";

import { throwIfAborted } from "../src/cancel.ts";
import { Database, EngineError } from "../src/tooling.ts";

function isCanceled(e: unknown): e is EngineError {
  return e instanceof EngineError && e.code() === "57014";
}

function rowCount(db: Database, sql: string): number {
  let n = 0;
  for (const _ of db.query(sql)) n++;
  return n;
}

// The boundary helper throws 57014 for an already-aborted signal, and is a no-op for an un-aborted
// signal or none at all (the zero-overhead default path).
test("throwIfAborted: aborted throws 57014, otherwise no-op", () => {
  assert.doesNotThrow(() => throwIfAborted(undefined));
  assert.doesNotThrow(() => throwIfAborted(new AbortController().signal));

  const ac = new AbortController();
  ac.abort();
  assert.throws(() => throwIfAborted(ac.signal), isCanceled);
});

// A signal already aborted at the API entry aborts with 57014 before any work — the boundary poll, on
// both the execute and the query path (the autocommit Database surface → Session.executeCancelable).
test("an already-aborted signal aborts at the boundary", () => {
  const db = Database.newInMemory();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY)");

  const ac = new AbortController();
  ac.abort();

  assert.throws(() => db.executeCancelable("INSERT INTO t VALUES (1)", [], ac.signal), isCanceled);
  assert.throws(() => db.queryCancelable("SELECT id FROM t", [], ac.signal), isCanceled);

  // The aborted INSERT never ran — the table is untouched.
  assert.strictEqual(rowCount(db, "SELECT id FROM t"), 0);
});

// An un-aborted (or absent) signal is zero-effect: the statement runs to completion and returns every
// row, proving the boundary check adds no spurious abort.
test("an un-aborted signal completes normally", () => {
  const db = Database.newInMemory();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
  for (let i = 1; i <= 20; i++) db.execute(`INSERT INTO t VALUES (${i})`);

  const ac = new AbortController(); // never aborted
  let n = 0;
  for (const _ of db.queryCancelable("SELECT id FROM t", [], ac.signal)) n++;
  assert.strictEqual(n, 20);
});

// Inside an explicit transaction (Transaction.executeCancelable), an aborted signal throws 57014;
// thrown out of the update() closure it rolls the block back, committing nothing.
test("a cancellation inside a transaction rolls back", () => {
  const db = Database.newInMemory();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY)");

  const ac = new AbortController();
  ac.abort();

  const s = db.session({});
  try {
    assert.throws(
      () =>
        s.update((tx) => {
          tx.executeCancelable("INSERT INTO t VALUES (1)", [], ac.signal);
        }),
      isCanceled,
    );
  } finally {
    s.close();
  }

  // The whole block rolled back — nothing committed.
  assert.strictEqual(rowCount(db, "SELECT id FROM t"), 0);
});

// Boundary-only semantics (the documented TS divergence). Aborting AFTER a synchronous query has no
// retroactive effect on the already-returned result; and a signal aborted BETWEEN two statements
// aborts the second but never the first (there is no mid-statement preemption to observe).
test("the signal is observed only at statement boundaries", () => {
  const db = Database.newInMemory();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
  db.execute("INSERT INTO t VALUES (1)");

  const ac = new AbortController();
  // First statement runs to completion synchronously while the signal is still live. The cursor is
  // single-pass (cursor.ts), so drain it once into an array, then re-assert on the array.
  const rows = db.queryCancelable("SELECT id FROM t", [], ac.signal);
  const out = [...rows];
  assert.strictEqual(out.length, 1);

  // Abort now (between statements): the already-materialized result is unaffected, and the NEXT
  // cancelable call aborts at its boundary.
  ac.abort();
  assert.strictEqual(out.length, 1, "an aborted signal does not retroactively void a result");
  assert.throws(() => db.queryCancelable("SELECT id FROM t", [], ac.signal), isCanceled);
});
