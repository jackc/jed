// Phase 5 (P5.3b): the shared handle — snapshot-isolated readers + a single writer and the oldest-
// live-version watermark (spec/design/transactions.md §8/§10). JS has no shared-memory threads, so
// this core gives snapshot ISOLATION (a pinned reader sees one stable version even as a writer
// interleaves commits), not CPU parallelism. The SQL transaction semantics are pinned by the shared
// conformance corpus (suites/transactions/); these per-core tests cover what the corpus cannot
// express — that a reader pins a consistent snapshot and that the watermark tracks live readers.

import assert from "node:assert/strict";
import { test } from "node:test";
import { EngineError, type Session, Database } from "../src/lib.ts";

// count runs SELECT count(*) FROM t against a read handle and returns the bigint count.
function count(r: Session): bigint {
  const rows = [...r.query("SELECT count(*) FROM t")];
  const v = rows[0][0];
  assert.equal(v.kind, "int", "expected an int count");
  return v.kind === "int" ? v.int : -1n;
}

// seeded builds a shared db with table t holding the given ids, committed via a write handle.
function seeded(...ids: number[]): Database {
  const db = Database.newInMemory();
  const w = db.writeSession();
  w.execute("CREATE TABLE t (id bigint PRIMARY KEY)");
  for (const id of ids) w.execute(`INSERT INTO t VALUES (${id})`);
  w.commit();
  return db;
}

test("write then read sees committed rows", () => {
  const db = seeded(1, 2, 3);
  assert.equal(db.version, 1n); // one commit ⇒ version 1
  const r = db.readSession();
  try {
    assert.equal(count(r), 3n);
  } finally {
    r.close();
  }
});

test("a read handle rejects writes (25006) without poisoning", () => {
  const db = seeded(1);
  const r = db.readSession();
  try {
    assert.throws(
      () => r.execute("INSERT INTO t VALUES (2)"),
      (e: unknown) => e instanceof EngineError && e.code() === "25006",
    );
    assert.equal(count(r), 1n); // still usable, still the pinned snapshot
  } finally {
    r.close();
  }
});

test("a pinned reader is isolated from a writer that commits between its calls", () => {
  // The interleaving analog of "readers parallel with a writer": a reader pins a snapshot, a writer
  // opens + commits a new version, and the already-pinned reader still sees its original snapshot.
  const db = seeded(1);
  const pinned = db.readSession(); // pins version 1 (one row)
  try {
    const before = count(pinned);
    assert.equal(before, 1n);

    const w = db.writeSession();
    w.execute("INSERT INTO t VALUES (2)"); // staged
    // While the writer is open, a *fresh* reader still sees only the committed row.
    const during = db.readSession();
    try {
      assert.equal(count(during), 1n);
    } finally {
      during.close();
    }
    w.commit(); // publish version 2

    assert.equal(count(pinned), 1n); // snapshot isolation: pinned reader unchanged by the commit
    assert.equal(db.version, 2n);
    const fresh = db.readSession();
    try {
      assert.equal(count(fresh), 2n); // a fresh reader sees both rows
    } finally {
      fresh.close();
    }
  } finally {
    pinned.close();
  }
});

test("a second writer while one is open is rejected (25001)", () => {
  const db = seeded(1);
  const w = db.writeSession();
  try {
    assert.throws(
      () => db.writeSession(),
      (e: unknown) => e instanceof EngineError && e.code() === "25001",
    );
  } finally {
    w.rollback(); // release the writer flag
  }
  // After the first writer ends, a new writer can proceed.
  const w2 = db.writeSession();
  w2.execute("INSERT INTO t VALUES (2)");
  w2.commit();
  const r = db.readSession();
  try {
    assert.equal(count(r), 2n);
  } finally {
    r.close();
  }
});

test("oldestLiveTxid tracks pinned readers", () => {
  const db = seeded(1); // version 1
  assert.equal(db.version, 1n);
  assert.equal(db.oldestLiveTxid(), 1n); // no readers ⇒ the committed version

  const r1 = db.readSession(); // pins version 1
  assert.equal(db.oldestLiveTxid(), 1n);

  const w = db.writeSession();
  w.execute("INSERT INTO t VALUES (2)");
  w.commit(); // version 2
  assert.equal(db.version, 2n);
  assert.equal(db.oldestLiveTxid(), 1n); // r1 still pins v1 ⇒ watermark held at 1

  const r2 = db.readSession(); // pins version 2
  assert.equal(db.oldestLiveTxid(), 1n); // still held by r1

  r1.close();
  assert.equal(db.oldestLiveTxid(), 2n); // r1 gone ⇒ watermark advances to r2's version

  r2.close();
  assert.equal(db.oldestLiveTxid(), 2n); // no readers ⇒ the committed version
});

test("a rolled-back writer publishes nothing", () => {
  const db = seeded(1);
  const w = db.writeSession();
  w.execute("INSERT INTO t VALUES (2)");
  w.rollback();
  const r = db.readSession();
  try {
    assert.equal(count(r), 1n); // the rolled-back insert never became visible
    assert.equal(db.version, 1n); // version unchanged by a rollback
  } finally {
    r.close();
  }
});
