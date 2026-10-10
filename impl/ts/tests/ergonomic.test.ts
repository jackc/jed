// The better-sqlite3-style ergonomic layer (spec/design/api.md §11): db.prepare(sql) → a Statement
// with run/get/all/iterate over native JS params + rows-as-objects. Per-core unit tests, NOT the
// shared corpus: this is a host-API surface (api.md §1), and these pin the JS↔Value mapping + the
// API shape the corpus cannot express. The underlying SQL behavior is the corpus's job — every
// method funnels through the same parser + executor the raw Value[] path uses.

import assert from "node:assert/strict";
import { test } from "node:test";

import { attachMemory, type Database, EngineError } from "../src/tooling.ts";
import { intValue } from "../src/value.ts";
import { memDb } from "./mem_db.ts";

function seeded(): Database {
  const db = memDb();
  db.run("CREATE TABLE t (id i32 PRIMARY KEY, name text, score f64, flag boolean)");
  db.run("INSERT INTO t VALUES ($1, $2, $3, $4)", 1, "ada", 9.5, true);
  db.run("INSERT INTO t VALUES ($1, $2, $3, $4)", 2, "bob", 7.25, false);
  return db;
}

// prepare(...).run returns a command tag whose `changes` is the affected-row count; DDL carries none.
test("run returns the affected-row count; DDL is 0", () => {
  const db = memDb();
  assert.strictEqual(db.run("CREATE TABLE t (id i32 PRIMARY KEY, name text)").changes, 0);

  const ins = db.prepare("INSERT INTO t VALUES ($1, $2)");
  assert.strictEqual(ins.run(1, "ada").changes, 1);
  assert.strictEqual(
    db.run("INSERT INTO t VALUES ($1, $2), ($3, $4)", 2, "bob", 3, "cy").changes,
    2,
  );
});

// get/all return rows as plain objects keyed by output column name, with the result value mapping:
// int→bigint (i64 exact), f64→number, boolean→boolean, text→string.
test("get and all return rows-as-objects with native scalar types", () => {
  const db = seeded();

  const row = db.prepare("SELECT id, name, score, flag FROM t WHERE id = $1").get(1);
  assert.deepStrictEqual(row, { id: 1n, name: "ada", score: 9.5, flag: true });
  // int is bigint, not number (i64 exactness — jed's identity).
  assert.strictEqual(typeof row?.id, "bigint");

  const all = db.prepare("SELECT id, name FROM t ORDER BY id").all();
  assert.deepStrictEqual(all, [
    { id: 1n, name: "ada" },
    { id: 2n, name: "bob" },
  ]);

  // get on an empty result is undefined (not null, not throw).
  assert.strictEqual(db.get("SELECT id FROM t WHERE id = $1", 999), undefined);

  // Object keys are the OUTPUT column names (aliases included).
  assert.deepStrictEqual(db.get("SELECT id AS the_id FROM t WHERE id = 2"), { the_id: 2n });
});

// iterate yields the same row objects lazily.
test("iterate yields row objects", () => {
  const db = seeded();
  const ids = [...db.prepare("SELECT id FROM t ORDER BY id").iterate()].map((r) => r.id);
  assert.deepStrictEqual(ids, [1n, 2n]);
});

// The param mapping: bigint→int, an integer-valued number→int (so run(1) binds an integer), boolean,
// string, null→NULL, a Uint8Array→bytea, and a raw engine Value passes through.
test("param mapping covers the native JS types", () => {
  const db = memDb();
  db.run("CREATE TABLE t (id i32 PRIMARY KEY, name text, data bytea)");

  db.run("INSERT INTO t VALUES ($1, $2, $3)", 1, "by-number", null); // number → int, null → NULL
  db.run("INSERT INTO t VALUES ($1, $2, $3)", 2n, "by-bigint", new Uint8Array([1, 2, 3]));
  db.run("INSERT INTO t (id, name) VALUES ($1, $2)", intValue(3n), "by-value"); // raw Value passthrough

  assert.strictEqual(db.get("SELECT name FROM t WHERE id = $1", 1)?.name, "by-number");
  assert.strictEqual(db.get("SELECT name FROM t WHERE id = $1", 2)?.name, "by-bigint");
  assert.strictEqual(db.get("SELECT name FROM t WHERE id = $1", 3)?.name, "by-value");

  // bytea round-trips as a Uint8Array; a NULL column reads as null.
  const r2 = db.get("SELECT data FROM t WHERE id = 2");
  assert.ok(r2?.data instanceof Uint8Array);
  assert.deepStrictEqual([...(r2.data as Uint8Array)], [1, 2, 3]);
  assert.strictEqual(db.get("SELECT data FROM t WHERE id = 1")?.data, null);
});

// Rich types without a clean JS counterpart return their canonical text (lossless, predictable):
// decimal, uuid, and the temporal types. (Driven through FROM-less SELECT casts/typed literals —
// typed literals are not accepted inside INSERT ... VALUES, so those are bound instead.)
test("rich types map to their canonical text", () => {
  const db = memDb();
  const row = db.get(
    "SELECT 12.50::numeric(10,2) AS amount, " +
      "'00112233-4455-6677-8899-aabbccddeeff'::uuid AS uid, " +
      "timestamp '2020-01-02 03:04:05' AS at",
  );
  assert.strictEqual(row?.amount, "12.50"); // decimal → canonical text, scale preserved
  assert.strictEqual(row?.uid, "00112233-4455-6677-8899-aabbccddeeff");
  assert.strictEqual(row?.at, "2020-01-02 03:04:05");
  assert.strictEqual(typeof row?.amount, "string");
});

// The ergonomic methods are on every handle: a durable Session, and a Transaction (whose work rolls
// back with the block when the update() closure throws).
test("methods on Session and Transaction", () => {
  const db = memDb();
  db.run("CREATE TABLE t (id i32 PRIMARY KEY, name text)");

  const s = db.session({});
  try {
    s.update((tx) => {
      tx.run("INSERT INTO t VALUES ($1, $2)", 1, "ada");
      assert.strictEqual(tx.get("SELECT count(*) AS n FROM t")?.n, 1n);
    });
    assert.strictEqual(
      s.get("SELECT count(*) AS n FROM t")?.n,
      1n,
      "committed through the session",
    );

    // A throwing update() rolls the block back — the second insert does not persist.
    assert.throws(() =>
      s.update((tx) => {
        tx.run("INSERT INTO t VALUES ($1, $2)", 2, "bob");
        throw new Error("boom");
      }),
    );
    assert.strictEqual(s.get("SELECT count(*) AS n FROM t")?.n, 1n, "rolled back");
  } finally {
    s.close();
  }
});

// get/all/iterate close their cursor (JS has no destructor; streaming.md §5), so no reader-liveness pin
// outlives the call — not past session.close(), and not into a later detach, which a live reader blocks
// (55006). iterate releases on exhaustion, on an early break, and when its consumer stops it with
// return(); a mid-drain error still releases. A raw query cursor left open is the control.
test("get, all, and iterate release their reader pin", () => {
  const db = memDb();
  db.attach("aux", attachMemory(), false);
  db.run("CREATE TABLE aux.a (id i32 PRIMARY KEY)");
  db.run("INSERT INTO aux.a VALUES (1), (2), (3)");
  const unpinned = () => assert.strictEqual(db.oldestLiveTxid(), db.version, "no reader pin held");

  const s = db.session({});
  const open = s.query("SELECT id FROM aux.a", []);
  assert.throws(
    () => db.detach("aux"),
    (e: unknown) => e instanceof EngineError && e.code() === "55006",
    "an unclosed cursor blocks detach",
  );
  open.close();
  unpinned();

  const stmt = s.prepare("SELECT id FROM aux.a ORDER BY id");
  assert.strictEqual(stmt.get()?.id, 1n);
  unpinned();
  assert.strictEqual(s.prepare("SELECT id FROM aux.a WHERE id > 9").get(), undefined);
  unpinned();
  assert.strictEqual(stmt.all().length, 3);
  unpinned();
  assert.deepEqual(
    [...stmt.iterate()].map((r) => r.id),
    [1n, 2n, 3n],
  );
  unpinned();
  for (const row of stmt.iterate()) {
    assert.strictEqual(row.id, 1n);
    break;
  }
  unpinned();
  const it = stmt.iterate();
  assert.strictEqual(it.next().value?.id, 1n);
  it.return?.();
  unpinned();
  stmt.iterate(); // never started: opens no cursor
  unpinned();

  const trap = s.prepare("SELECT 1 / (id - 2) AS q FROM aux.a");
  assert.throws(
    () => trap.all(),
    (e: unknown) => e instanceof EngineError && e.code() === "22012",
  );
  unpinned();

  // The Database one-shots route through the same Statement.
  assert.strictEqual(db.get("SELECT id FROM aux.a ORDER BY id")?.id, 1n);
  assert.strictEqual(db.all("SELECT id FROM aux.a").length, 3);
  unpinned();

  s.close();
  db.detach("aux");
});
