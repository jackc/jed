// CHECK constraints — `[CONSTRAINT name] CHECK ( expr )` in both positions
// (spec/design/constraints.md §4, grammar.md §29). Covers what the corpus suite
// (ddl/check.test) cannot: catalog introspection (names, evaluation order, persisted
// expression text), the on-disk round-trip (v4 catalog check list), a corrupted stored
// expression (XX001), and the metered evaluation cost. Mirrors
// impl/rust/tests/check_constraint.rs and impl/go/check_constraint_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import { EngineError } from "../src/errors.ts";
import { type Database, execute } from "../src/lib.ts";
import { loadDatabase, toImage } from "../src/format.ts";
import { dbWith, errCode } from "./util.ts";

function errInfo(fn: () => void): { code: string; message: string } {
  try {
    fn();
  } catch (e) {
    if (e instanceof EngineError) {
      // EngineError.message carries the "CODE: " display prefix; strip it so the
      // assertions mirror the Rust/Go message text.
      return { code: e.code(), message: e.message.replace(`${e.code()}: `, "") };
    }
    throw e;
  }
  throw new Error("expected an EngineError, but no error was thrown");
}

function checkNames(db: Database, table: string): string[] {
  return db.table(table)!.checks.map((c) => c.name);
}

test("auto-naming matches PostgreSQL", () => {
  const db = dbWith([
    "CREATE TABLE t (a int CHECK (a > 0), b int, CHECK (b > a), CHECK (1 < 2), CHECK (b < 100))",
    // Two same-column checks on one column, then a table-level one on it.
    "CREATE TABLE t2 (a int CHECK (a > 0) CHECK (a < 10), CHECK (a = 5))",
    // A table-level check FIRST gets the unsuffixed name (textual order).
    "CREATE TABLE t3 (CHECK (a > 0), a int CHECK (a < 5))",
    // An explicit name occupying a would-be auto name: the auto skips to the next free.
    "CREATE TABLE t9 (a int CONSTRAINT t9_a_check CHECK (a > 0) CHECK (a < 5))",
  ]);
  assert.deepEqual(checkNames(db, "t"), ["t_a_check", "t_b_check", "t_check", "t_check1"]);
  assert.deepEqual(checkNames(db, "t2"), ["t2_a_check", "t2_a_check1", "t2_a_check2"]);
  assert.deepEqual(checkNames(db, "t3"), ["t3_a_check", "t3_a_check1"]);
  assert.deepEqual(checkNames(db, "t9"), ["t9_a_check", "t9_a_check1"]);
  // The persisted expression text is the re-rendered token sequence.
  assert.deepEqual(
    db.table("t")!.checks.map((c) => c.exprText),
    ["a > 0", "b < 100", "b > a", "1 < 2"],
  );
});

test("DDL errors match PostgreSQL", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => execute(db, "CREATE TABLE x (a int CHECK (a + 1))")),
    "42804",
  );
  // Subqueries — scalar, EXISTS, IN — are rejected structurally, before any resolution
  // (the inner table need not exist).
  for (const sql of [
    "CREATE TABLE x (a int CHECK (a > (SELECT v FROM nowhere)))",
    "CREATE TABLE x (a int CHECK (EXISTS (SELECT v FROM nowhere)))",
    "CREATE TABLE x (a int CHECK (a IN (SELECT v FROM nowhere)))",
  ]) {
    const e = errInfo(() => execute(db, sql));
    assert.equal(e.code, "0A000", sql);
    assert.equal(e.message, "cannot use subquery in check constraint");
  }
  let e = errInfo(() => execute(db, "CREATE TABLE x (a int CHECK (sum(a) > 0))"));
  assert.equal(e.code, "42803");
  assert.equal(e.message, "aggregate functions are not allowed in check constraints");
  e = errInfo(() => execute(db, "CREATE TABLE x (a int CHECK (a > $1))"));
  assert.equal(e.code, "42P02");
  assert.equal(e.message, "there is no parameter $1");
  assert.equal(
    errCode(() => execute(db, "CREATE TABLE x (a int CHECK (nope > 0))")),
    "42703",
  );
  assert.equal(
    errCode(() => execute(db, "CREATE TABLE x (a int CHECK (other.a > 0))")),
    "42P01",
  );
  // A forward reference is fine (checks resolve after all columns are known); so is a
  // reference qualified by this table's name.
  execute(db, "CREATE TABLE fwd (CHECK (b > 0), b int)");
  execute(db, "CREATE TABLE q (a int CHECK (q.a > 0))");
  // Duplicate explicit name.
  e = errInfo(() =>
    execute(db, "CREATE TABLE x (a int CONSTRAINT cc CHECK (a > 0) CONSTRAINT cc CHECK (a < 5))"),
  );
  assert.equal(e.code, "42710");
  assert.equal(e.message, "constraint cc for relation x already exists");
  // An explicit name colliding with an EARLIER auto name (derived names never yield).
  assert.equal(
    errCode(() =>
      execute(db, "CREATE TABLE tb (a int CHECK (a > 0), CONSTRAINT tb_a_check CHECK (a < 5))"),
    ),
    "42710",
  );
  // PRIMARY KEY constraints resolve before any check expression (PG's order).
  e = errInfo(() =>
    execute(db, "CREATE TABLE tc (a int CHECK (nope > 0), PRIMARY KEY (alsonope))"),
  );
  assert.equal(e.code, "42703");
  assert.ok(e.message.includes("named in key"), e.message);
  // ALL validation precedes ALL naming: a 42703 in a later check beats a 42710 between
  // earlier ones.
  assert.equal(
    errCode(() =>
      execute(
        db,
        "CREATE TABLE td (a int CONSTRAINT cc CHECK (a > 0), CONSTRAINT cc CHECK (nope > 0))",
      ),
    ),
    "42703",
  );
  // The DEFAULT is NOT checked against CHECK at CREATE TABLE.
  execute(db, "CREATE TABLE t7 (a int DEFAULT -5 CHECK (a > 0))");
  // CHECK () is a syntax error.
  assert.equal(
    errCode(() => execute(db, "CREATE TABLE x (a int, CHECK ())")),
    "42601",
  );
  // Columns may be NAMED check / constraint (the keywords stay non-reserved).
  execute(db, "CREATE TABLE odd (check int, constraint i16)");
  execute(db, "INSERT INTO odd VALUES (1, 2)");
});

test("violations match PostgreSQL order", () => {
  const db = dbWith([
    "CREATE TABLE t (a int CHECK (a > 0), b int, CHECK (b > a))",
    // zz is defined first but aa evaluates first (name order, oracle-probed).
    "CREATE TABLE t5 (a int, CONSTRAINT zz CHECK (a > 0), CONSTRAINT aa CHECK (a > 5))",
    "CREATE TABLE tn (a int NOT NULL CHECK (a > 0))",
    "CREATE TABLE tu (k int PRIMARY KEY, v int CHECK (v > 0))",
  ]);
  let e = errInfo(() => execute(db, "INSERT INTO t VALUES (-1, 5)"));
  assert.equal(e.code, "23514");
  assert.equal(e.message, "new row for relation t violates check constraint t_a_check");
  // Violating both: the first in name order reports.
  e = errInfo(() => execute(db, "INSERT INTO t VALUES (-1, -5)"));
  assert.ok(e.message.endsWith("t_a_check"), e.message);
  e = errInfo(() => execute(db, "INSERT INTO t VALUES (5, 1)"));
  assert.ok(e.message.endsWith("t_check"), e.message);
  e = errInfo(() => execute(db, "INSERT INTO t5 VALUES (-1)"));
  assert.ok(e.message.endsWith("violates check constraint aa"), e.message);
  // NULL passes a check (UNKNOWN is not FALSE).
  execute(db, "INSERT INTO t VALUES (NULL, NULL)");
  // NOT NULL fires before CHECK on the same row.
  assert.equal(
    errCode(() => execute(db, "INSERT INTO tn VALUES (NULL)")),
    "23502",
  );
  // CHECK fires before the duplicate-key check.
  execute(db, "INSERT INTO tu VALUES (1, 5)");
  assert.equal(
    errCode(() => execute(db, "INSERT INTO tu VALUES (1, -1)")),
    "23514",
  );
  // A runtime error inside a check propagates as itself.
  execute(db, "CREATE TABLE dz (a int CHECK (10 / a > 0))");
  assert.equal(
    errCode(() => execute(db, "INSERT INTO dz VALUES (0)")),
    "22012",
  );
});

test("two-phase pass and defaults", () => {
  const db = dbWith([
    "CREATE TABLE t (a int CHECK (a > 0))",
    "CREATE TABLE src (v int)",
    "INSERT INTO src VALUES (3), (-3)",
    "CREATE TABLE t7 (a int DEFAULT -5 CHECK (a > 0), b int)",
  ]);
  // Multi-row INSERT: the second row violates → nothing stored.
  assert.equal(
    errCode(() => execute(db, "INSERT INTO t VALUES (1), (-1)")),
    "23514",
  );
  assert.equal(db.rowsInKeyOrder("t").length, 0);
  // INSERT ... SELECT flows through the same per-row checks.
  assert.equal(
    errCode(() => execute(db, "INSERT INTO t SELECT v FROM src")),
    "23514",
  );
  // UPDATE: a later row violates → no row changes.
  execute(db, "INSERT INTO t VALUES (1), (2)");
  assert.equal(
    errCode(() => execute(db, "UPDATE t SET a = a - 1")),
    "23514",
  );
  const rows = db.rowsInKeyOrder("t");
  assert.equal(rows.length, 2);
  assert.deepEqual(
    rows.map((r) => (r[0]!.kind === "int" ? r[0]!.int : null)),
    [1n, 2n],
  );
  // An UPDATE that passes every check applies.
  execute(db, "UPDATE t SET a = a + 10");
  // The stored default is evaluated per row like any value: a check-violating default
  // traps 23514 at INSERT, not CREATE.
  assert.equal(
    errCode(() => execute(db, "INSERT INTO t7 VALUES (DEFAULT, 1)")),
    "23514",
  );
  assert.equal(
    errCode(() => execute(db, "INSERT INTO t7 (b) VALUES (1)")),
    "23514",
  );
  execute(db, "INSERT INTO t7 VALUES (2, 1)");
});

test("the full expression surface works inside a check", () => {
  const db = dbWith([
    "CREATE TABLE e (n int, flag boolean, note text, price numeric(8,2), " +
      "CHECK (CASE WHEN n IS NULL THEN TRUE ELSE n BETWEEN 0 AND 100 END), " +
      "CHECK (flag), " +
      "CHECK (note LIKE 'ok%' OR note IN ('a', 'b')), " +
      "CHECK (abs(n) <= CAST(100 AS int)), " +
      "CONSTRAINT price_pos CHECK (price >= 0.50))",
  ]);
  execute(db, "INSERT INTO e VALUES (50, TRUE, 'ok then', 1.00), (NULL, TRUE, 'a', 0.50)");
  for (const sql of [
    "INSERT INTO e VALUES (101, TRUE, 'a', 1.00)",
    "INSERT INTO e VALUES (1, FALSE, 'a', 1.00)",
    "INSERT INTO e VALUES (1, TRUE, 'c', 1.00)",
  ]) {
    assert.equal(
      errCode(() => execute(db, sql)),
      "23514",
      sql,
    );
  }
  const e = errInfo(() => execute(db, "INSERT INTO e VALUES (1, TRUE, 'a', 0.49)"));
  assert.ok(e.message.endsWith("price_pos"), e.message);
});

test("check evaluation is metered", () => {
  const db = dbWith(["CREATE TABLE c (a int CHECK (a > 0))"]);
  // One interior node (>) × one row.
  let o = execute(db, "INSERT INTO c VALUES (1)");
  assert.equal(o.cost, 1n);
  // Two rows × one node.
  o = execute(db, "INSERT INTO c VALUES (2), (3)");
  assert.equal(o.cost, 2n);
  // UPDATE: page_read(1) + 3×storage_row_read + 3×(a + 1) + 3×(a > 0) = 10.
  o = execute(db, "UPDATE c SET a = a + 1");
  assert.equal(o.cost, 10n);
  // The ceiling aborts mid-validation deterministically.
  db.setMaxCost(2n);
  assert.equal(
    errCode(() => execute(db, "INSERT INTO c VALUES (4), (5), (6)")),
    "54P01",
  );
  db.setMaxCost(0n);
  assert.equal(db.rowsInKeyOrder("c").length, 3);
});

test("round-trips through the on-disk image", () => {
  const db = dbWith([
    "CREATE TABLE t (a int PRIMARY KEY, b int CHECK (b > 0), price numeric(8,2), " +
      "CONSTRAINT price_range CHECK (price >= 0.50 AND price <= 9999.99), note text, " +
      "CHECK (note = 'ok' OR note = 'a''b'))",
    "INSERT INTO t VALUES (1, 5, 1.00, 'ok'), (2, NULL, 9999.99, 'a''b'), (3, 100, 0.50, 'ok')",
  ]);
  const image = toImage(db, 256, 1n);
  const loaded = loadDatabase(image);
  assert.deepEqual(
    loaded.table("t")!.checks.map((c) => [c.name, c.exprText]),
    [
      ["price_range", "price >= 0.50 AND price <= 9999.99"],
      ["t_b_check", "b > 0"],
      ["t_note_check", "note = 'ok' OR note = 'a''b'"],
    ],
  );
  // Still enforced, with the same message.
  const e = errInfo(() => execute(loaded, "INSERT INTO t VALUES (4, -1, 1.00, 'ok')"));
  assert.equal(e.code, "23514");
  assert.equal(e.message, "new row for relation t violates check constraint t_b_check");
  assert.equal(
    errCode(() => execute(loaded, "INSERT INTO t VALUES (4, 1, 0.10, 'ok')")),
    "23514",
  );
  assert.equal(
    errCode(() => execute(loaded, "INSERT INTO t VALUES (4, 1, 1.00, 'nope')")),
    "23514",
  );
  execute(loaded, "INSERT INTO t VALUES (4, 1, 1.00, 'a''b')");
  // A second generation (load → image → load) is byte-stable: the text is written back
  // verbatim.
  const reloaded = loadDatabase(toImage(loaded, 256, 1n));
  assert.deepEqual(checkNames(reloaded, "t"), ["price_range", "t_b_check", "t_note_check"]);

  // A stored expression that no longer parses is XX001 (the file lied): patch the text
  // `b > 0` to the same-length garbage `b > (`.
  const needle = new TextEncoder().encode("b > 0");
  let at = -1;
  outer: for (let i = 0; i + needle.length <= image.length; i++) {
    for (let j = 0; j < needle.length; j++) {
      if (image[i + j] !== needle[j]) continue outer;
    }
    at = i;
    break;
  }
  assert.ok(at >= 0, "stored check text not found in image");
  const corrupt = image.slice();
  corrupt[at + 4] = "(".charCodeAt(0);
  assert.equal(
    errCode(() => loadDatabase(corrupt)),
    "XX001",
  );
});
