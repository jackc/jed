// generate_series — the engine's first set-returning function, a FROM-clause row source
// (spec/design/functions.md §10, grammar.md §35). These complement the conformance corpus
// (spec/conformance/suites/query/generate_series.test) with finer-grained assertions: the
// generator's PostgreSQL edge cases (NULL → empty, step zero → 22023, descending step, the
// positive-default-step empty case, i64-overflow clean-stop), the synthetic-relation wiring
// (output column name/type, alias + qualified resolution, CROSS JOIN composition), the
// non-LATERAL rule ($N / correlated outer arg vs. a rejected sibling reference), the
// generated_row cost contract + the maxCost ceiling, and the deferred-form errors.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, execute, executeParams, intValue } from "../src/lib.ts";
import { dbWith, errCode, query } from "./util.ts";

function cost(db: Database, sql: string): bigint {
  return execute(db, sql).cost;
}

function rows1(ns: number[]): string[][] {
  return ns.map((n) => [String(n)]);
}

test("two-arg generate_series names and types its column", () => {
  const db = new Database();
  const out = execute(db, "SELECT * FROM generate_series(1, 5)");
  assert.equal(out.kind, "query");
  if (out.kind !== "query") return;
  assert.deepStrictEqual(out.columnNames, ["generate_series"]);
  // Integer literals default to int64, so the promoted column type is int64.
  assert.deepStrictEqual(out.columnTypes, ["int64"]);
  assert.deepStrictEqual(query(db, "SELECT * FROM generate_series(1, 5)"), rows1([1, 2, 3, 4, 5]));
  // 5 generated_row + 5 row_produced; the integer-literal args are leaves (no operator_eval).
  assert.equal(out.cost, 10n);
});

test("three-arg step and descending", () => {
  const db = new Database();
  assert.deepStrictEqual(query(db, "SELECT * FROM generate_series(1, 10, 2)"), rows1([1, 3, 5, 7, 9]));
  assert.deepStrictEqual(query(db, "SELECT * FROM generate_series(5, 1, -1)"), rows1([5, 4, 3, 2, 1]));
  assert.deepStrictEqual(query(db, "SELECT * FROM generate_series(3, 3)"), rows1([3]));
});

test("empty cases: start past stop and NULL args", () => {
  const db = new Database();
  assert.deepStrictEqual(query(db, "SELECT * FROM generate_series(5, 1)"), []);
  assert.equal(cost(db, "SELECT * FROM generate_series(5, 1)"), 0n);
  for (const sql of [
    "SELECT * FROM generate_series(NULL, 5)",
    "SELECT * FROM generate_series(1, NULL)",
    "SELECT * FROM generate_series(1, 5, NULL)",
  ]) {
    assert.deepStrictEqual(query(db, sql), [], sql);
    assert.equal(cost(db, sql), 0n, sql);
  }
});

test("step of zero is invalid_parameter_value (22023)", () => {
  const db = new Database();
  assert.equal(errCode(() => execute(db, "SELECT * FROM generate_series(1, 5, 0)")), "22023");
});

test("alias forms and qualified column", () => {
  const db = new Database();
  // PG's single-column function-alias rule: `AS g` (or implicit `g`) renames the column to `g`.
  assert.deepStrictEqual(query(db, "SELECT * FROM generate_series(1, 3) g"), rows1([1, 2, 3]));
  const out = execute(db, "SELECT * FROM generate_series(1, 3) AS g");
  assert.equal(out.kind, "query");
  if (out.kind === "query") assert.deepStrictEqual(out.columnNames, ["g"]);
  assert.deepStrictEqual(query(db, "SELECT g.g FROM generate_series(1, 3) AS g"), rows1([1, 2, 3]));
  assert.equal(errCode(() => execute(db, "SELECT g.generate_series FROM generate_series(1, 3) AS g")), "42703");
  assert.deepStrictEqual(
    query(db, "SELECT generate_series.generate_series FROM generate_series(1, 2)"),
    rows1([1, 2]),
  );
});

test("WHERE / ORDER BY / LIMIT compose", () => {
  const db = new Database();
  assert.deepStrictEqual(query(db, "SELECT * FROM generate_series(1, 5) WHERE generate_series > 2"), rows1([3, 4, 5]));
  assert.deepStrictEqual(query(db, "SELECT * FROM generate_series(1, 5) ORDER BY generate_series DESC LIMIT 2"), rows1([5, 4]));
});

test("CROSS JOIN with a base table", () => {
  const db = dbWith(["CREATE TABLE t (id int32 PRIMARY KEY)", "INSERT INTO t VALUES (10), (20)"]);
  assert.deepStrictEqual(
    query(db, "SELECT * FROM t CROSS JOIN generate_series(1, 3) ORDER BY id, generate_series"),
    [
      ["10", "1"],
      ["10", "2"],
      ["10", "3"],
      ["20", "1"],
      ["20", "2"],
      ["20", "3"],
    ],
  );
});

test("$N parameter argument", () => {
  const db = new Database();
  const out = executeParams(db, "SELECT * FROM generate_series(1, $1)", [intValue(3n)]);
  assert.equal(out.kind, "query");
  if (out.kind !== "query") return;
  assert.deepStrictEqual(out.rows.map((r) => r.map((v) => v.kind === "int" ? String(v.int) : "?")), [["1"], ["2"], ["3"]]);
});

test("correlated outer argument in a subquery (non-LATERAL)", () => {
  const db = dbWith([
    "CREATE TABLE t (id int32 PRIMARY KEY, n int32)",
    "INSERT INTO t VALUES (1, 0), (2, 2), (3, 3)",
  ]);
  assert.deepStrictEqual(
    query(db, "SELECT (SELECT count(*) FROM generate_series(1, o.n)) FROM t o ORDER BY id"),
    rows1([0, 2, 3]),
  );
});

test("a sibling reference is rejected (non-LATERAL)", () => {
  const db = dbWith(["CREATE TABLE t (id int32 PRIMARY KEY, n int32)", "INSERT INTO t VALUES (1, 3)"]);
  assert.equal(errCode(() => execute(db, "SELECT * FROM t CROSS JOIN generate_series(1, t.n)")), "42P01");
});

test("generated_row cost and the maxCost ceiling", () => {
  const db = new Database();
  assert.equal(cost(db, "SELECT * FROM generate_series(1, 4)"), 8n);
  db.setMaxCost(50n);
  assert.equal(errCode(() => execute(db, "SELECT * FROM generate_series(1, 1000000000)")), "54P01");
  db.setMaxCost(0n);
});

test("mixed-width promotes to the wider type", () => {
  const db = new Database();
  const out = execute(db, "SELECT * FROM generate_series(CAST(1 AS int16), CAST(5 AS int32))");
  assert.equal(out.kind, "query");
  if (out.kind !== "query") return;
  assert.deepStrictEqual(out.columnTypes, ["int32"]);
});

test("i64 overflow while stepping stops cleanly (bigint parity)", () => {
  const db = new Database();
  // Stepping past i64::MAX must STOP, not run forever (bigint never overflows): only the last
  // representable element is emitted, matching Rust/Go's checked_add stop.
  assert.deepStrictEqual(
    query(db, "SELECT * FROM generate_series(9223372036854775806, 9223372036854775807, 2)"),
    [["9223372036854775806"]],
  );
});

test("deferred-form and bad-call errors", () => {
  const db = new Database();
  assert.equal(errCode(() => execute(db, "SELECT generate_series(1, 5)")), "42883"); // SELECT-list SRF deferred
  assert.equal(errCode(() => execute(db, "SELECT * FROM generate_series(1, 5) AS g(n)")), "0A000"); // column-alias list
  assert.equal(errCode(() => execute(db, "SELECT * FROM generate_series(1)")), "42883"); // wrong arity
  assert.equal(errCode(() => execute(db, "SELECT * FROM generate_series('a', 5)")), "42883"); // non-integer arg
  assert.equal(errCode(() => execute(db, "SELECT * FROM nope(1, 5)")), "42883"); // unknown table function
});
