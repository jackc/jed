// VALUES-body derived tables — FROM (VALUES (e…),(e…)) [AS] v(c…) (spec/design/grammar.md §42). A
// parenthesized VALUES list used as a FROM relation: a computed relation of literal rows, the
// FROM-position sibling of INSERT … VALUES, reusing the derived-table seam (an anonymous,
// always-inlined single-reference CTE). These complement the conformance corpus
// (spec/conformance/suites/subquery/values_body.test) with finer-grained per-feature assertions:
// the default column1… names + the column-rename list, general constant expressions, per-column
// type unification across rows, composition with WHERE/ORDER BY/JOIN/aggregates, the intrinsic
// cost, and the error / narrowing codes (42601 / 42804 / 42703 / 42803 / 42P18).

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, intValue } from "../src/tooling.ts";
import { type Handle, errCode, query } from "./util.ts";

function names(db: Handle, sql: string): string[] {
  const o = db.execute(sql);
  if (o.kind !== "query") throw new Error(`expected a query result for ${sql}`);
  return o.columnNames;
}

function types(db: Handle, sql: string): string[] {
  const o = db.execute(sql);
  if (o.kind !== "query") throw new Error(`expected a query result for ${sql}`);
  return o.columnTypes;
}

function cost(db: Handle, sql: string): bigint {
  return db.execute(sql).cost;
}

test("VALUES body — basic shape and default column name", () => {
  const db = Database.newInMemory().session();
  assert.deepStrictEqual(
    query(db, "SELECT column1 FROM (VALUES (1), (2), (3)) AS v ORDER BY column1"),
    [["1"], ["2"], ["3"]],
  );
  assert.deepStrictEqual(names(db, "SELECT * FROM (VALUES (1), (2)) AS v"), ["column1"]);
});

test("VALUES body — multi-column default names and rename list", () => {
  const db = Database.newInMemory().session();
  assert.deepStrictEqual(names(db, "SELECT * FROM (VALUES (1, 'a'), (2, 'b')) AS v"), [
    "column1",
    "column2",
  ]);
  assert.deepStrictEqual(names(db, "SELECT * FROM (VALUES (1, 'a')) AS v(n, s)"), ["n", "s"]);
  // A partial rename keeps the trailing body name.
  assert.deepStrictEqual(names(db, "SELECT * FROM (VALUES (1, 'a')) AS v(n)"), ["n", "column2"]);
  assert.deepStrictEqual(query(db, "SELECT v.n FROM (VALUES (7), (8)) AS v(n) ORDER BY v.n"), [
    ["7"],
    ["8"],
  ]);
});

test("VALUES body — per-column type unification across rows", () => {
  const db = Database.newInMemory().session();
  // int + int -> int (all bare integer literals are i64 in jed).
  assert.deepStrictEqual(types(db, "SELECT column1 FROM (VALUES (1), (2)) AS v"), ["i64"]);
  // int + decimal -> decimal; the int value coerces.
  assert.deepStrictEqual(types(db, "SELECT column1 FROM (VALUES (1), (2.5)) AS v"), ["decimal"]);
  assert.deepStrictEqual(
    query(db, "SELECT column1 FROM (VALUES (1), (2.5)) AS v ORDER BY column1"),
    [["1"], ["2.5"]],
  );
  // anything + NULL keeps the other type.
  assert.deepStrictEqual(types(db, "SELECT column1 FROM (VALUES (1), (NULL)) AS v"), ["i64"]);
  // an all-NULL column is text (unknown -> text).
  assert.deepStrictEqual(types(db, "SELECT column1 FROM (VALUES (NULL), (NULL)) AS v"), ["text"]);
});

test("VALUES body — a $N is typed by its sibling rows", () => {
  const db = Database.newInMemory().session();
  const o = db.execute("SELECT column1 FROM (VALUES (1), ($1)) AS v ORDER BY column1", [
    intValue(7n),
  ]);
  if (o.kind !== "query") throw new Error("expected a query");
  assert.deepStrictEqual(
    o.rows.map((r) => r.map((v) => (v.kind === "int" ? v.int.toString() : "?"))),
    [["1"], ["7"]],
  );
});

test("VALUES body — intrinsic cost", () => {
  const db = Database.newInMemory().session();
  // row_produced per VALUES row (3) + outer SELECT row_produced (3) = 6.
  assert.strictEqual(cost(db, "SELECT column1 FROM (VALUES (1), (2), (3)) AS v"), 6n);
  // (1+1) adds one operator_eval.
  assert.strictEqual(cost(db, "SELECT column1 FROM (VALUES (1 + 1)) AS v"), 3n);
});

test("VALUES body — errors / narrowings", () => {
  const db = Database.newInMemory().session();
  const cases: [string, string][] = [
    ["SELECT * FROM (VALUES (1), (2, 3)) AS v", "42601"], // differing arity
    ["SELECT * FROM (VALUES (1), ('a')) AS v", "42804"], // types do not unify
    ["SELECT * FROM (VALUES (oops)) AS v", "42703"], // column ref (non-LATERAL)
    ["SELECT * FROM (VALUES (sum(1))) AS v", "42803"], // aggregate
    ["SELECT * FROM (VALUES ($1)) AS v", "42P18"], // bare $1, no type
    ["SELECT * FROM (VALUES (1), (2) ORDER BY 1) AS v", "42601"], // trailing ORDER BY (deferred)
    ["SELECT * FROM (VALUES (1)) AS v(a, b)", "42P10"], // too many rename aliases
  ];
  for (const [sql, code] of cases) {
    assert.strictEqual(
      errCode(() => db.execute(sql)),
      code,
      sql,
    );
  }
});
