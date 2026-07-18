// Composite TYPE as a key — a column whose type is a `CREATE TYPE … AS (…)` row type used as a
// PRIMARY KEY / ordered secondary index / UNIQUE column (the third container key,
// `composite-field-slots`, spec/design/encoding.md §2.15 / composite.md §6). Distinct from the
// multi-column composite PRIMARY KEY in composite_pk.test.ts (a flat tuple of scalar columns).
// Covers what the corpus cannot: the stored key ORDER (the recursive per-field encoding), catalog
// introspection, the on-disk round-trip, and the array-of-composite 0A000 narrowing. Mirrors
// impl/rust/tests/composite_key.rs and impl/go/composite_key_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import { pkIndices } from "../src/catalog.ts";
import { Database } from "../src/tooling.ts";
import { type Handle, dbWith, errCode } from "./util.ts";

function ids(db: Handle, table: string): bigint[] {
  return db.rowsInKeyOrder(table).map((r) => {
    const v = r[0]!;
    if (v.kind !== "int") throw new Error("expected int id");
    return v.int;
  });
}

// A composite-typed column is a valid sole PRIMARY KEY, and rows iterate in the composite sort key's
// order — lexicographic over fields (text then the tie-breaking i32), reproducing the in-memory
// comparator (§5) under the §2.15 memcmp key.
test("composite PK orders by field lexicographic", () => {
  const db = dbWith([
    "CREATE TYPE addr AS (street text, zip i32)",
    "CREATE TABLE t (id i32, home addr, PRIMARY KEY (home))",
    "INSERT INTO t VALUES (1, ROW('Main', 90210))",
    "INSERT INTO t VALUES (2, ROW('Elm', 100))",
    "INSERT INTO t VALUES (3, ROW('Main', 5))",
    "INSERT INTO t VALUES (4, ROW('', -1))",
  ]);
  // '' < 'Elm' < 'Main'; within 'Main', zip 5 < 90210  => ids 4, 2, 3, 1
  assert.deepEqual(ids(db, "t"), [4n, 2n, 3n, 1n]);
  assert.deepEqual(pkIndices(db.table("t")!), [1]);
  assert.equal(db.table("t")!.columns[1]!.notNull, true);
});

// Uniqueness is over the whole composite value: a duplicate composite traps 23505, a value that
// differs in ANY field is distinct.
test("composite PK uniqueness is the whole value", () => {
  const db = dbWith([
    "CREATE TYPE addr AS (street text, zip i32)",
    "CREATE TABLE t (id i32, home addr, PRIMARY KEY (home))",
    "INSERT INTO t VALUES (1, ROW('Main', 5))",
  ]);
  db.execute("INSERT INTO t VALUES (2, ROW('Main', 6))"); // distinct zip
  assert.equal(
    errCode(() => db.execute("INSERT INTO t VALUES (9, ROW('Main', 5))")),
    "23505",
  );
  assert.equal(
    errCode(() => db.execute("INSERT INTO t VALUES (7, ROW('X', 1)), (8, ROW('X', 1))")),
    "23505",
  );
  assert.equal(db.rowsInKeyOrder("t").length, 2);
});

// A composite UNIQUE constraint recurses through a NESTED composite field.
test("composite UNIQUE and nested", () => {
  const db = dbWith([
    "CREATE TYPE addr AS (street text, zip i32)",
    "CREATE TYPE line AS (a addr, b addr)",
    "CREATE TABLE t (id i32, seg line, UNIQUE (seg))",
    "INSERT INTO t VALUES (1, ROW(ROW('Main',1), ROW('Elm',2)))",
  ]);
  assert.equal(
    errCode(() => db.execute("INSERT INTO t VALUES (4, ROW(ROW('Main',1), ROW('Elm',2)))")),
    "23505",
  );
  db.execute("INSERT INTO t VALUES (5, ROW(ROW('Main',1), ROW('Elm',3)))"); // distinct nested field
  assert.equal(db.rowsInKeyOrder("t").length, 2);
});

// A secondary index over a composite column supports maintenance (INSERT/DELETE) and the composite
// value round-trips through the on-disk image.
test("composite secondary index and image round-trip", () => {
  const db = dbWith([
    "CREATE TYPE addr AS (street text, zip i32)",
    "CREATE TABLE t (id i32 PRIMARY KEY, home addr)",
    "CREATE INDEX t_home ON t (home)",
    "INSERT INTO t VALUES (1, ROW('Main', 90210))",
    "INSERT INTO t VALUES (2, ROW('Elm', 100))",
    "INSERT INTO t VALUES (3, ROW('Main', 5))",
  ]);
  const loaded = Database.fromImage(db.toImage(256, 1n));
  loaded.execute("DELETE FROM t WHERE id = 2");
  loaded.execute("INSERT INTO t VALUES (4, ROW('Elm', 100))"); // exercises index maintenance
  assert.equal(loaded.rowsInKeyOrder("t").length, 3);
  assert.ok(loaded.table("t")!.indexes.some((i) => i.name === "t_home"));
});

// A composite transitively containing an array-of-composite field is NOT keyable (the array key
// admits only scalar elements, §2.14) — the lone remaining 0A000 key case. A composite with a
// scalar-array field IS keyable.
test("array-of-composite field is not keyable", () => {
  const db = dbWith([
    "CREATE TYPE addr AS (street text, zip i32)",
    "CREATE TYPE tags AS (name text, nums i32[])",
    "CREATE TYPE poly AS (name text, spots addr[])",
  ]);
  db.execute("CREATE TABLE ok (id i32, t tags, PRIMARY KEY (t))"); // scalar-array field: keyable
  assert.equal(
    errCode(() => db.execute("CREATE TABLE t (id i32, p poly, PRIMARY KEY (p))")),
    "0A000",
  );
  assert.equal(
    errCode(() => db.execute("CREATE TABLE t (id i32, p poly, UNIQUE (p))")),
    "0A000",
  );
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, p poly)");
  assert.equal(
    errCode(() => db.execute("CREATE INDEX t_p ON t (p)")),
    "0A000",
  );
});
