// unnest — the polymorphic set-returning function (AF3, spec/design/array-functions.md §9), the
// engine's second FROM-clause SRF after generate_series. These complement the conformance corpus
// (spec/conformance/suites/query/unnest.test) with finer-grained assertions: the generator's output
// column name/type (the bound element type), the NULL/empty semantics, multidimensional flattening,
// the generated_row cost contract + the maxCost ceiling, and the deferred-form / strictness errors
// NOT in the oracle corpus (the SELECT-list position 42883, the bare-untyped-NULL 42P18, a wrong
// arity / non-array 42883). The TS core mirrors Rust/Go exactly (CLAUDE.md §2).

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database } from "../src/tooling.ts";
import { type Handle, dbWith, errCode, query } from "./util.ts";

function cost(db: Handle, sql: string): bigint {
  return db.execute(sql).cost;
}

function rows1(ns: number[]): string[][] {
  return ns.map((n) => [String(n)]);
}

// qOut runs a query and narrows the result to the query Outcome (so columnNames / columnTypes
// are accessible without a union-member error).
function qOut(db: Handle, sql: string) {
  const o = db.execute(sql);
  if (o.kind !== "query") throw new Error(`expected a query result for ${sql}`);
  return o;
}

test("unnest names its column at the bound element type", () => {
  const db = Database.newInMemory().session();
  // An untyped ARRAY[…] literal is i64[] (jed's literal typing).
  const out = qOut(db, "SELECT * FROM unnest(ARRAY[10,20,30])");
  assert.deepStrictEqual(out.columnNames, ["unnest"]);
  assert.deepStrictEqual(out.columnTypes, ["i64"]);
  // A typed '{…}'::i32[] literal pins the element type; a text[] argument → a text column.
  assert.deepStrictEqual(qOut(db, "SELECT * FROM unnest('{1,2,3}'::i32[])").columnTypes, ["i32"]);
  assert.deepStrictEqual(qOut(db, "SELECT * FROM unnest(ARRAY['a','b'])").columnTypes, ["text"]);
});

test("unnest of the empty array or a NULL array yields zero rows", () => {
  const db = Database.newInMemory().session();
  for (const sql of ["SELECT * FROM unnest('{}'::i32[])", "SELECT * FROM unnest(NULL::i32[])"]) {
    assert.deepStrictEqual(query(db, sql), [], sql);
    assert.equal(cost(db, sql), 0n, sql);
  }
});

test("unnest alias renames the single column", () => {
  const db = Database.newInMemory().session();
  assert.deepStrictEqual(
    query(db, "SELECT g.g FROM unnest(ARRAY[7,8]) AS g ORDER BY g.g"),
    rows1([7, 8]),
  );
  assert.equal(
    errCode(() => db.execute("SELECT g.unnest FROM unnest(ARRAY[7,8]) AS g")),
    "42703",
  );
});

test("unnest takes a correlated outer column AND an earlier sibling (implicitly lateral, §44)", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, xs i32[])",
    "INSERT INTO t VALUES (1, ARRAY[10,20]), (2, '{30}'), (3, NULL), (4, '{}')",
  ]);
  // A correlated OUTER column resolves into the SRF arg of an enclosing-query subquery (the SRF is
  // the subquery's sole/first FROM item, so its args see the enclosing query — functions.md §10).
  assert.deepStrictEqual(
    query(db, "SELECT id, (SELECT count(*) FROM unnest(o.xs)) AS n FROM t o ORDER BY id"),
    [
      ["1", "2"],
      ["2", "1"],
      ["3", "0"],
      ["4", "0"],
    ],
  );
  // A sibling FROM table's column IS now in scope (an SRF is implicitly lateral, grammar.md §44; the
  // rows are pinned by suites/joins/lateral.test). Here we assert the prior 42703/42P01 rejection is
  // lifted: the bare and qualified forms succeed and explode each row (NULL/empty → no rows ⇒ 3 rows).
  for (const sql of [
    "SELECT id, u FROM t CROSS JOIN unnest(xs) AS u",
    "SELECT id, u FROM t CROSS JOIN unnest(t.xs) AS u",
  ]) {
    const out = db.execute(sql);
    assert.equal(out.kind, "query");
    if (out.kind !== "query") return;
    assert.equal(out.rows.length, 3);
  }
});

test("unnest strictness + deferred-form errors", () => {
  const db = Database.newInMemory().session();
  for (const sql of [
    "SELECT * FROM unnest(5)",
    "SELECT * FROM unnest('hi')",
    "SELECT * FROM unnest(ARRAY[1], ARRAY[2])",
  ]) {
    assert.equal(
      errCode(() => db.execute(sql)),
      "42883",
      sql,
    );
  }
  // A bare untyped NULL is indeterminate (jed's polymorphic posture); the SELECT-list SRF is deferred.
  assert.equal(
    errCode(() => db.execute("SELECT * FROM unnest(NULL)")),
    "42P18",
  );
  assert.equal(
    errCode(() => db.execute("SELECT unnest(ARRAY[1,2,3])")),
    "42883",
  );
});

test("unnest generated_row cost and the maxCost ceiling", () => {
  const db = Database.newInMemory().session();
  // '{…}'::i32[] is a const (no operator_eval): 3 generated_row + 3 row_produced.
  assert.equal(cost(db, "SELECT * FROM unnest('{1,2,3}'::i32[])"), 6n);
  // A large array aborts deterministically once accrued cost reaches the ceiling (54P01), before
  // the whole thing materializes — the guard fires mid-generation, like generate_series.
  const big = Array.from({ length: 1000 }, (_, i) => String(i + 1)).join(",");
  db.setMaxCost(50n);
  assert.equal(
    errCode(() => db.execute(`SELECT * FROM unnest('{${big}}'::i32[])`)),
    "54P01",
  );
  db.setMaxCost(0n);
});
