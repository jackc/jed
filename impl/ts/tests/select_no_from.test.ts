// FROM-less SELECT — the select list evaluates over ONE virtual zero-column row, no table
// access (spec/design/grammar.md §34). These complement the conformance corpus
// (spec/conformance/suites/query/select_no_from.test) with finer-grained assertions: the
// virtual-row pipeline (WHERE / aggregates / DISTINCT / HAVING / LIMIT compose), the zero-scan
// cost contract (SELECT 1 = exactly 1 row_produced — spec/design/cost.md §3), composition in
// set operations / subqueries (correlated included) / INSERT ... SELECT, and the error surface
// (SELECT * → 42601 with PostgreSQL's exact message; a bare column — including the
// `SELECT distinct` lookahead consequence — → 42703; an untyped $1 → 42P18).

import assert from "node:assert/strict";
import { test } from "node:test";
import { EngineError, intValue } from "../src/tooling.ts";
import { type Handle, dbWith, errCode, query } from "./util.ts";
import { memDb } from "./mem_db.ts";

function cost(db: Handle, sql: string): bigint {
  return db.execute(sql).cost;
}

test("SELECT 1 returns one row costing one row_produced", () => {
  const db = memDb().session();
  const out = db.execute("SELECT 1");
  assert.equal(out.kind, "query");
  if (out.kind !== "query") return;
  assert.deepStrictEqual(out.columnNames, ["?column?"]);
  assert.deepStrictEqual(query(db, "SELECT 1"), [["1"]]);
  // No relation, no scan: zero page_read/storage_row_read — just the one row_produced.
  assert.equal(out.cost, 1n);
});

test("an expression select charges its operator_evals", () => {
  const db = memDb().session();
  assert.deepStrictEqual(query(db, "SELECT 1 + 2"), [["3"]]);
  // 1 operator_eval (the `+` node) + 1 row_produced.
  assert.equal(cost(db, "SELECT 1 + 2"), 2n);
});

test("WHERE filters the virtual row", () => {
  const db = memDb().session();
  assert.deepStrictEqual(query(db, "SELECT 1 WHERE false"), []);
  // The constant filter is a leaf (no operator_eval) and no row is produced.
  assert.equal(cost(db, "SELECT 1 WHERE false"), 0n);
  assert.deepStrictEqual(query(db, "SELECT 1 WHERE 1 = 1"), [["1"]]);
  assert.equal(cost(db, "SELECT 1 WHERE 1 = 1"), 2n); // the `=` + the produced row
});

test("aggregates fold the single group", () => {
  const db = memDb().session();
  // The virtual row is the one input row of the whole-table group (aggregates.md §4).
  assert.deepStrictEqual(query(db, "SELECT count(*)"), [["1"]]);
  assert.equal(cost(db, "SELECT count(*)"), 2n); // 1 aggregate_accumulate + 1 row_produced
  // A false WHERE empties the input but the single group still emits.
  assert.deepStrictEqual(query(db, "SELECT count(*) WHERE false"), [["0"]]);
  assert.equal(cost(db, "SELECT count(*) WHERE false"), 1n);
  assert.deepStrictEqual(query(db, "SELECT max(5)"), [["5"]]);
  // HAVING filters the single group away.
  assert.deepStrictEqual(query(db, "SELECT 1 HAVING false"), []);
});

test("DISTINCT and the LIMIT/OFFSET window apply to the single row", () => {
  const db = memDb().session();
  assert.deepStrictEqual(query(db, "SELECT DISTINCT 1"), [["1"]]);
  assert.deepStrictEqual(query(db, "SELECT 1 LIMIT 0"), []);
  assert.deepStrictEqual(query(db, "SELECT 1 OFFSET 1"), []);
});

test("set operation operands", () => {
  const db = memDb().session();
  const rows = query(db, "SELECT 1 UNION SELECT 2")
    .map((r) => r[0]!)
    .sort();
  assert.deepStrictEqual(rows, ["1", "2"]);
  // Each operand costs 1; the combine is unmetered (cost.md §3).
  assert.equal(cost(db, "SELECT 1 UNION SELECT 2"), 2n);
});

test("subqueries: uncorrelated fold and correlated outward resolution", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)", "INSERT INTO t VALUES (1), (2)"]);
  // Uncorrelated FROM-less inner: folded once.
  assert.deepStrictEqual(query(db, "SELECT (SELECT 1)"), [["1"]]);
  // Correlated FROM-less inner: the zero-relation scope resolves o.id purely outward,
  // re-executed per outer row.
  assert.deepStrictEqual(query(db, "SELECT (SELECT o.id) FROM t o ORDER BY id"), [["1"], ["2"]]);
  // 1 page_read + 2 storage_row_read + per outer row (×2): the subquery node's
  // operator_eval + the inner row_produced; + 2 outer row_produced = 9.
  assert.equal(cost(db, "SELECT (SELECT o.id) FROM t o ORDER BY id"), 9n);
});

test("INSERT ... SELECT source", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)"]);
  const out = db.execute("INSERT INTO t SELECT 3");
  assert.equal(out.cost, 1n); // exactly the embedded SELECT's cost
  assert.deepStrictEqual(query(db, "SELECT id FROM t"), [["3"]]);
});

test("SELECT * with no tables is 42601 with PostgreSQL's message", () => {
  const db = memDb().session();
  try {
    db.execute("SELECT *");
    assert.fail("SELECT *: expected an error");
  } catch (e) {
    assert.ok(e instanceof EngineError);
    assert.equal(e.code(), "42601");
    // The TS EngineError prefixes Error.message with the SQLSTATE.
    assert.equal(e.message, "42601: SELECT * with no tables specified is not valid");
  }
});

test("bare columns resolve nothing", () => {
  const db = memDb().session();
  assert.equal(
    errCode(() => db.execute("SELECT nope")),
    "42703",
  );
  // The DISTINCT two-token lookahead is unchanged: at end of input the word is a column
  // reference, not the modifier (grammar.md §34 — previously died at the FROM expect).
  assert.equal(
    errCode(() => db.execute("SELECT distinct")),
    "42703",
  );
  assert.equal(
    errCode(() => db.execute("SELECT from")),
    "42703",
  );
  // GROUP BY / ORDER BY keys are table columns only — always 42703 on a lone FROM-less SELECT.
  assert.equal(
    errCode(() => db.execute("SELECT 1 GROUP BY nope")),
    "42703",
  );
  assert.equal(
    errCode(() => db.execute("SELECT 1 ORDER BY nope")),
    "42703",
  );
});

test("an untyped $1 is 42P18; a sibling operand types it", () => {
  const db = memDb().session();
  assert.equal(
    errCode(() => db.execute("SELECT $1", [intValue(7n)])),
    "42P18",
  );
  // The sibling-operand rule (grammar.md §5) works without a FROM.
  const out = db.execute("SELECT $1 + 1", [intValue(7n)]);
  assert.equal(out.kind, "query");
  if (out.kind !== "query") return;
  assert.equal(out.rows.length, 1);
});
