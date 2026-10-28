// Host-API behaviors of the view/update closure, failed-block poisoning at the parse stage, and the
// never-waiting host operations (spec/design/api.md §2.2/§2.3, transactions.md §6) — per-core because
// the corpus is single-handle SQL-in/rows-out (CLAUDE.md §10). Mirrors impl/go/closure_txn_test.go
// and impl/rust/tests/closure_txn.rs.
//
//   - Transaction control inside a closure is 2D000 (invalid_transaction_termination): SQL
//     COMMIT/ROLLBACK through the closure's Transaction aborts the block (PostgreSQL's rule for a
//     procedure called within a block), and the handle's own commit/rollback is refused without
//     aborting. The closure's writes are never silently discarded: a closure that swallows the error
//     gets 25P02 from update, not a normal return that committed nothing — and the writer gate is
//     released either way (TS cannot block, so a leaked gate would fail every later write 25001).
//   - A parse error (42601), the 54000 input-size limit, and a prepareStatement parse error inside an
//     open block poison it like an execution error; BEGIN in a failed block is 25P02.
//   - close / attach / detach never wait for the writer gate: 55006 while a write block is open, and
//     fine once it ends.

import assert from "node:assert/strict";
import { test } from "node:test";
import { attachMemory, EngineError, intValue, type Outcome, queryOutcome } from "../src/tooling.ts";
import { memDb } from "./mem_db.ts";

function code(fn: () => unknown): string {
  try {
    fn();
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  throw new Error("expected an error, got none");
}

function rowCount(o: Outcome): number {
  if (o.kind !== "query") throw new Error("expected a query result");
  return o.rows.length;
}

test("SQL COMMIT inside update is 2D000 and never discards writes", () => {
  const db = memDb();
  const s = db.session();
  s.execute("CREATE TABLE t (id i32 PRIMARY KEY)");

  // The closure propagates the 2D000: update rolls back and reports it.
  assert.throws(
    () =>
      s.update((tx) => {
        tx.execute("INSERT INTO t VALUES (1)");
        assert.equal(
          code(() => tx.execute("COMMIT")),
          "2D000",
        );
        // the block is failed: a later statement is 25P02
        assert.equal(
          code(() => tx.execute("INSERT INTO t VALUES (5)")),
          "25P02",
        );
        throw new Error("propagate");
      }),
    /propagate/,
  );
  assert.equal(s.inTransaction(), false);
  assert.equal(rowCount(queryOutcome(s, "SELECT * FROM t")), 0);

  // The closure swallows the error and returns normally: update still reports that nothing committed.
  assert.equal(
    code(() =>
      s.update((tx) => {
        tx.execute("INSERT INTO t VALUES (1)");
        try {
          tx.execute("ROLLBACK");
        } catch {
          // swallowed
        }
        try {
          tx.execute("INSERT INTO t VALUES (5)");
        } catch {
          // swallowed
        }
      }),
    ),
    "25P02",
  );
  assert.equal(rowCount(queryOutcome(s, "SELECT * FROM t")), 0);

  // The gate was released: this session and another both write again (no 25001).
  s.execute("INSERT INTO t VALUES (2)");
  const other = db.session();
  other.execute("INSERT INTO t VALUES (3)");
  assert.equal(rowCount(queryOutcome(other, "SELECT * FROM t")), 2);
  other.close();
  s.close();
});

test("the closure handle's commit/rollback are 2D000 without aborting", () => {
  const s = memDb().session();
  s.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
  s.update((tx) => {
    tx.execute("INSERT INTO t VALUES (1)");
    assert.equal(
      code(() => tx.commit()),
      "2D000",
    );
    assert.equal(
      code(() => tx.rollback()),
      "2D000",
    );
    // not a statement error: the block is still open and usable
    tx.execute("INSERT INTO t VALUES (2)");
  });
  assert.equal(rowCount(queryOutcome(s, "SELECT * FROM t")), 2);
  // view: the same gate
  assert.equal(
    code(() =>
      s.view((tx) => {
        assert.equal(
          code(() => tx.execute("COMMIT")),
          "2D000",
        );
      }),
    ),
    "25P02",
  );
  s.close();
});

test("savepoints work inside an update closure", () => {
  const s = memDb().session();
  s.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
  s.update((tx) => {
    for (const sql of [
      "INSERT INTO t VALUES (1)",
      "SAVEPOINT a",
      "INSERT INTO t VALUES (2)",
      "ROLLBACK TO a",
      "INSERT INTO t VALUES (3)",
      "RELEASE a",
    ]) {
      tx.execute(sql);
    }
    // a nested BEGIN stays 25001 and does not abort
    assert.equal(
      code(() => tx.execute("BEGIN")),
      "25001",
    );
  });
  const o = queryOutcome(s, "SELECT id FROM t ORDER BY id");
  if (o.kind !== "query") throw new Error("expected a query result");
  assert.deepEqual(o.rows, [[intValue(1n)], [intValue(3n)]]);
  s.close();
});

test("parse-stage errors poison an open block", () => {
  const s = memDb().session();
  s.setMaxSqlLength(64);
  s.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
  const expectPoisoned = (what: string) => {
    assert.equal(
      code(() => s.execute("SELECT id FROM t")),
      "25P02",
      `after ${what}`,
    );
    assert.equal(
      code(() => s.execute("BEGIN")),
      "25P02",
      `BEGIN after ${what}`,
    );
    s.execute("ROLLBACK");
    s.execute("SELECT id FROM t");
  };

  s.execute("BEGIN");
  assert.equal(
    code(() => s.execute("SELECT 1 FROM")),
    "42601",
  );
  expectPoisoned("a 42601");

  s.execute("BEGIN");
  assert.equal(
    code(() => s.execute(`SELECT '${"x".repeat(80)}'`)),
    "54000",
  );
  expectPoisoned("a 54000");

  s.execute("BEGIN");
  assert.equal(
    code(() => s.prepareStatement("SELEC 1")),
    "42601",
  );
  expectPoisoned("a prepareStatement 42601");

  // the closure's Transaction too
  assert.equal(
    code(() =>
      s.update((tx) => {
        assert.equal(
          code(() => tx.execute("SELECT 1 FROM")),
          "42601",
        );
        assert.equal(
          code(() => tx.execute("SELECT id FROM t")),
          "25P02",
        );
      }),
    ),
    "25P02",
  );
  assert.equal(s.inTransaction(), false);
  s.close();
});

test("close / attach / detach never wait for an open write block", () => {
  const db = memDb();
  db.attach("aux", attachMemory(), false);
  const s = db.session();
  s.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
  s.begin(true);
  s.execute("INSERT INTO t VALUES (1)");
  assert.equal(
    code(() => db.close()),
    "55006",
  );
  assert.equal(
    code(() => db.attach("more", attachMemory(), false)),
    "55006",
  );
  assert.equal(
    code(() => db.detach("aux")),
    "55006",
  );
  // The refusals disturbed nothing: the block commits and the operations then succeed.
  s.commit();
  assert.equal(rowCount(queryOutcome(s, "SELECT * FROM t")), 1);
  db.attach("more", attachMemory(), false);
  db.detach("aux");
  s.close();
  db.close();
});
