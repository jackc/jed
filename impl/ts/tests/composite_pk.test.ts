// Composite PRIMARY KEY — the table-level `PRIMARY KEY (a, b, …)` constraint
// (spec/design/constraints.md §3, grammar.md §28). Covers what the corpus suite
// (ddl/composite_pk.test) cannot: catalog flag introspection, the stored key order
// (the concatenated encoding of encoding.md §2.3), and the on-disk round-trip (a
// composite-PK table reloads as a KEYED table, not a rowid table). Mirrors
// impl/rust/tests/composite_pk.rs and impl/go/composite_pk_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import { pkIndices, primaryKeyIndex } from "../src/catalog.ts";
import { Database } from "../src/tooling.ts";
import { loadEngine, toImage } from "../src/format.ts";
import { type Handle, dbWith, errCode } from "./util.ts";

// The visible tuple (first two columns) of each row, in stored key order.
function tuples(db: Handle, table: string): [bigint, bigint][] {
  return db.rowsInKeyOrder(table).map((r) => {
    const a = r[0]!;
    const b = r[1]!;
    if (a.kind !== "int" || b.kind !== "int") throw new Error("expected int pair");
    return [a.int, b.int];
  });
}

test("composite key flags members and orders by tuple", () => {
  const db = dbWith(["CREATE TABLE t (a i32, b i32, v i16, PRIMARY KEY (a, b))"]);
  const t = db.table("t")!;
  assert.deepEqual(pkIndices(t), [0, 1]);
  assert.ok(t.columns[0]!.primaryKey && t.columns[0]!.notNull);
  assert.ok(t.columns[1]!.primaryKey && t.columns[1]!.notNull);
  assert.ok(!t.columns[2]!.primaryKey);
  // Single-column pushdown accessor must NOT see a composite key.
  assert.equal(primaryKeyIndex(t), -1);

  // Insert out of tuple order; include a negative first component (sign-flip) and ties
  // on the first component broken by the second.
  for (const stmt of [
    "INSERT INTO t VALUES (2, 1, 50)",
    "INSERT INTO t VALUES (1, 2, 30)",
    "INSERT INTO t VALUES (-1, 9, 10)",
    "INSERT INTO t VALUES (1, 1, 20)",
    "INSERT INTO t VALUES (2, 0, 40)",
  ]) {
    db.execute(stmt);
  }
  assert.deepEqual(tuples(db, "t"), [
    [-1n, 9n],
    [1n, 1n],
    [1n, 2n],
    [2n, 0n],
    [2n, 1n],
  ]);
});

test("uniqueness is over the whole tuple", () => {
  const db = dbWith([
    "CREATE TABLE t (a i32, b i32, PRIMARY KEY (a, b))",
    "INSERT INTO t VALUES (1, 1)",
  ]);
  db.execute("INSERT INTO t VALUES (1, 2)"); // shared prefix: distinct row
  assert.equal(
    errCode(() => db.execute("INSERT INTO t VALUES (1, 1)")),
    "23505",
  );
  assert.equal(
    errCode(() => db.execute("INSERT INTO t VALUES (5, 5), (5, 5)")),
    "23505",
  );
  // The failed batch stored nothing (all-or-nothing).
  assert.equal(db.rowsInKeyOrder("t").length, 2);
});

test("DDL errors mirror PostgreSQL plus the jed narrowings", () => {
  const cases: [string, string][] = [
    ["CREATE TABLE t (a i32, PRIMARY KEY (a, nosuch))", "42703"],
    ["CREATE TABLE t (a i32, b i32, PRIMARY KEY (a, a))", "42701"],
    ["CREATE TABLE t (a i32 PRIMARY KEY, b i32, PRIMARY KEY (b))", "42P16"],
    ["CREATE TABLE t (a i32, b i32, PRIMARY KEY (a), PRIMARY KEY (b))", "42P16"],
    // 42P16 fires BEFORE the second constraint's members resolve (PostgreSQL's order).
    ["CREATE TABLE t (a i32 PRIMARY KEY, PRIMARY KEY (nosuch))", "42P16"],
  ];
  for (const [sql, want] of cases) {
    assert.equal(
      errCode(() => Database.newInMemory().session().execute(sql)),
      want,
      sql,
    );
  }
  // f64 IS now a key-encodable PK member (the float-order-preserving key, encoding.md §2.8 — every
  // scalar is keyable); only the recursive composite container is NOT (composite.md §6).
  {
    const db = Database.newInMemory().session();
    db.execute("CREATE TABLE fpk (a i32, s f64, PRIMARY KEY (a, s))");
    db.execute("CREATE TYPE addr AS (street text, zip i32)");
    assert.equal(
      errCode(() => db.execute("CREATE TABLE t (a i32, s addr, PRIMARY KEY (a, s))")),
      "0A000",
    );
  }
  // The list order is the KEY order — it may differ from declaration order (the original
  // 0A000 narrowing was lifted by the v5 catalog reshape, constraints.md §3): the table
  // keys by (b, a), so the stored scan order is b-major.
  {
    const rev = Database.newInMemory().session();
    rev.execute("CREATE TABLE rev (a i32, b i32, PRIMARY KEY (b, a))");
    assert.deepEqual(pkIndices(rev.table("rev")!), [1, 0]);
    rev.execute("INSERT INTO rev VALUES (1, 20), (2, 10), (3, 15)");
    const bs = rev.rowsInKeyOrder("rev").map((r) => (r[1]!.kind === "int" ? r[1]!.int : null));
    assert.deepEqual(bs, [10n, 15n, 20n], "stored order is the (b, a) tuple order");
  }

  // A single-column table constraint is the column-level form's equivalent.
  const db = Database.newInMemory().session();
  db.execute("CREATE TABLE ok (a i32, PRIMARY KEY (a))");
  const t = db.table("ok")!;
  assert.equal(primaryKeyIndex(t), 0);
  assert.ok(t.columns[0]!.notNull);
});

test("members are NOT NULL and assigning one re-keys the row", () => {
  const db = dbWith([
    "CREATE TABLE t (a i32, b i32, v i16, PRIMARY KEY (a, b))",
    "INSERT INTO t VALUES (1, 1, 10)",
  ]);
  assert.equal(
    errCode(() => db.execute("INSERT INTO t VALUES (1, NULL, 5)")),
    "23502",
  );
  assert.equal(
    errCode(() => db.execute("INSERT INTO t (a, v) VALUES (2, 5)")),
    "23502",
  );
  // Assigning a key member re-keys the row (§11 step 6): (1,1) → (9,1) → (9,9); a non-member
  // updates in place. No longer 0A000.
  db.execute("UPDATE t SET a = 9");
  db.execute("UPDATE t SET b = 9");
  db.execute("UPDATE t SET v = 11");
  const rows = db.rowsInKeyOrder("t");
  assert.equal(rows.length, 1);
  const vals = rows[0]!.map((c) => {
    if (c.kind !== "int") throw new Error("expected int");
    return c.int;
  });
  assert.deepEqual(vals, [9n, 9n, 11n]);
});

test("mixed uuid + int components order correctly", () => {
  const db = dbWith(["CREATE TABLE t (u uuid, n i32, PRIMARY KEY (u, n))"]);
  for (const stmt of [
    "INSERT INTO t VALUES ('ffffffff-ffff-ffff-ffff-ffffffffffff', -5)",
    "INSERT INTO t VALUES ('00000000-0000-0000-0000-000000000001', 7)",
    "INSERT INTO t VALUES ('00000000-0000-0000-0000-000000000001', -2)",
  ]) {
    db.execute(stmt);
  }
  const ns = db.rowsInKeyOrder("t").map((r) => {
    const n = r[1]!;
    if (n.kind !== "int") throw new Error("expected int");
    return n.int;
  });
  assert.deepEqual(ns, [-2n, 7n, -5n]);
});

test("round-trips through the on-disk image as a keyed table", () => {
  const db = dbWith([
    "CREATE TABLE t (a i32, b i32, v i16, PRIMARY KEY (a, b))",
    "INSERT INTO t VALUES (2, 1, 40), (1, 2, 20), (1, 1, 10)",
  ]);
  const image = db.toImage(256, 1n);
  const loaded = Database.fromImage(image);

  const t = loaded.table("t")!;
  assert.deepEqual(pkIndices(t), [0, 1]);
  assert.ok(t.columns[0]!.notNull && t.columns[1]!.notNull);

  assert.deepEqual(tuples(loaded, "t"), [
    [1n, 1n],
    [1n, 2n],
    [2n, 1n],
  ]);

  assert.equal(
    errCode(() => loaded.execute("INSERT INTO t VALUES (1, 2, 99)")),
    "23505",
  );
  loaded.execute("INSERT INTO t VALUES (2, 2, 50)");
  assert.equal(loaded.rowsInKeyOrder("t").length, 4);
});
