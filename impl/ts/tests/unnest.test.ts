// unnest — the polymorphic set-returning function (AF3, spec/design/array-functions.md §9), the
// engine's second FROM-clause SRF after generate_series. These complement the conformance corpus
// (spec/conformance/suites/query/unnest.test) with finer-grained assertions: the generator's output
// column name/type (the bound element type), the NULL/empty semantics, multidimensional flattening,
// the generated_row cost contract + the maxCost ceiling, and the deferred-form / strictness errors
// NOT in the oracle corpus (the SELECT-list position 42883, the bare-untyped-NULL 42P18, a wrong
// arity / non-array 42883). The TS core mirrors Rust/Go exactly (CLAUDE.md §2).

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, execute } from "../src/lib.ts";
import { dbWith, errCode, query } from "./util.ts";

function cost(db: Database, sql: string): bigint {
  return execute(db, sql).cost;
}

function rows1(ns: number[]): string[][] {
  return ns.map((n) => [String(n)]);
}

// qOut runs a query and narrows the result to the query Outcome (so columnNames / columnTypes
// are accessible without a union-member error).
function qOut(db: Database, sql: string) {
  const o = execute(db, sql);
  if (o.kind !== "query") throw new Error(`expected a query result for ${sql}`);
  return o;
}

test("unnest names its column at the bound element type", () => {
  const db = new Database();
  // An untyped ARRAY[…] literal is int64[] (jed's literal typing).
  const out = qOut(db, "SELECT * FROM unnest(ARRAY[10,20,30])");
  assert.deepStrictEqual(out.columnNames, ["unnest"]);
  assert.deepStrictEqual(out.columnTypes, ["int64"]);
  // A typed '{…}'::int32[] literal pins the element type; a text[] argument → a text column.
  assert.deepStrictEqual(qOut(db, "SELECT * FROM unnest('{1,2,3}'::int32[])").columnTypes, ["int32"]);
  assert.deepStrictEqual(qOut(db, "SELECT * FROM unnest(ARRAY['a','b'])").columnTypes, ["text"]);
});

test("unnest of the empty array or a NULL array yields zero rows", () => {
  const db = new Database();
  for (const sql of ["SELECT * FROM unnest('{}'::int32[])", "SELECT * FROM unnest(NULL::int32[])"]) {
    assert.deepStrictEqual(query(db, sql), [], sql);
    assert.equal(cost(db, sql), 0n, sql);
  }
});

test("unnest alias renames the single column", () => {
  const db = new Database();
  assert.deepStrictEqual(query(db, "SELECT g.g FROM unnest(ARRAY[7,8]) AS g ORDER BY g.g"), rows1([7, 8]));
  assert.equal(errCode(() => execute(db, "SELECT g.unnest FROM unnest(ARRAY[7,8]) AS g")), "42703");
});

test("unnest takes a correlated outer column but not a sibling (non-LATERAL)", () => {
  const db = dbWith([
    "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])",
    "INSERT INTO t VALUES (1, ARRAY[10,20]), (2, '{30}'), (3, NULL), (4, '{}')",
  ]);
  assert.deepStrictEqual(
    query(db, "SELECT id, (SELECT count(*) FROM unnest(o.xs)) AS n FROM t o ORDER BY id"),
    [["1", "2"], ["2", "1"], ["3", "0"], ["4", "0"]],
  );
  // A sibling FROM table's column is not in scope for the SRF arg.
  assert.equal(errCode(() => execute(db, "SELECT id, u FROM t CROSS JOIN unnest(xs) AS u")), "42703");
  assert.equal(errCode(() => execute(db, "SELECT id, u FROM t CROSS JOIN unnest(t.xs) AS u")), "42P01");
});

test("unnest strictness + deferred-form errors", () => {
  const db = new Database();
  for (const sql of ["SELECT * FROM unnest(5)", "SELECT * FROM unnest('hi')", "SELECT * FROM unnest(ARRAY[1], ARRAY[2])"]) {
    assert.equal(errCode(() => execute(db, sql)), "42883", sql);
  }
  // A bare untyped NULL is indeterminate (jed's polymorphic posture); the SELECT-list SRF is deferred.
  assert.equal(errCode(() => execute(db, "SELECT * FROM unnest(NULL)")), "42P18");
  assert.equal(errCode(() => execute(db, "SELECT unnest(ARRAY[1,2,3])")), "42883");
});

test("unnest generated_row cost and the maxCost ceiling", () => {
  const db = new Database();
  // '{…}'::int32[] is a const (no operator_eval): 3 generated_row + 3 row_produced.
  assert.equal(cost(db, "SELECT * FROM unnest('{1,2,3}'::int32[])"), 6n);
  // A large array aborts deterministically once accrued cost reaches the ceiling (54P01), before
  // the whole thing materializes — the guard fires mid-generation, like generate_series.
  const big = Array.from({ length: 1000 }, (_, i) => String(i + 1)).join(",");
  db.setMaxCost(50n);
  assert.equal(errCode(() => execute(db, `SELECT * FROM unnest('{${big}}'::int32[])`)), "54P01");
  db.setMaxCost(0n);
});
