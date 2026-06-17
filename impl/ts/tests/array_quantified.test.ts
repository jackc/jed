// Array function/operator surface — AF5 (spec/design/array-functions.md §11): the ANY/ALL/SOME
// quantified array comparisons (x = ANY(arr), x op ALL(arr)), the array spelling of IN and its
// universal dual. Every expected value is pinned against PostgreSQL 18 (the three-valued NULL rules
// especially). Mirrors impl/rust/tests/array_quantified.rs.
//
// jed types a bare integer literal / ARRAY[…] constructor as int64, so the bare cases use int64;
// column adaptation (an int32 column vs a bare ARRAY[…]) is exercised via a table.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, execute } from "../src/lib.ts";
import { errCode, query } from "./util.ts";

// val runs a one-column, one-row scalar query and returns the rendered value.
function val(db: Database, sql: string): string {
  const rows = query(db, sql);
  assert.equal(rows.length, 1, sql);
  assert.equal(rows[0]!.length, 1, sql);
  return rows[0]![0]!;
}

test("ANY equality is IN", () => {
  const db = new Database();
  const cases: [string, string][] = [
    ["SELECT 1 = ANY(ARRAY[1,2,3])", "true"],
    ["SELECT 5 = ANY(ARRAY[1,2,3])", "false"],
    ["SELECT 2 = SOME(ARRAY[1,2,3])", "true"], // SOME is the synonym for ANY
    ["SELECT 2 = ANY('{1,2,3}'::int64[])", "true"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("ANY is three-valued", () => {
  const db = new Database();
  const cases: [string, string][] = [
    ["SELECT NULL::int64 = ANY(ARRAY[1,2,3])", "NULL"], // NULL x, non-empty → NULL
    ["SELECT 1 = ANY(ARRAY[2,NULL])", "NULL"], // no TRUE, a NULL element → NULL
    ["SELECT 2 = ANY(ARRAY[2,NULL])", "true"], // a TRUE match dominates a NULL
    ["SELECT 1 = ANY('{}'::int64[])", "false"], // empty → FALSE
    ["SELECT 1 = ANY(NULL::int64[])", "NULL"], // NULL array → NULL
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("ALL is the universal dual", () => {
  const db = new Database();
  const cases: [string, string][] = [
    ["SELECT 3 = ALL(ARRAY[3,3,3])", "true"],
    ["SELECT 3 = ALL(ARRAY[3,3,4])", "false"],
    ["SELECT 3 = ALL(ARRAY[4,NULL])", "false"], // a FALSE element dominates a NULL
    ["SELECT 3 = ALL(ARRAY[3,NULL])", "NULL"], // else a NULL → NULL
    ["SELECT 3 = ALL('{}'::int64[])", "true"], // empty → TRUE (vacuous)
    ["SELECT NULL::int64 = ALL('{}'::int64[])", "true"], // empty beats a NULL x
    ["SELECT 3 = ALL(NULL::int64[])", "NULL"], // NULL array → NULL
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("ordering operators, shape, and text elements", () => {
  const db = new Database();
  const cases: [string, string][] = [
    ["SELECT 5 < ANY(ARRAY[1,2,10])", "true"],
    ["SELECT 5 > ALL(ARRAY[1,2,3])", "true"],
    ["SELECT 5 <= ALL(ARRAY[5,6,7])", "true"],
    ["SELECT 5 >= ANY(ARRAY[9,8,5])", "true"],
    ["SELECT 5 > ALL(ARRAY[1,2,9])", "false"],
    // FLATTENED element multiset (any dimensionality).
    ["SELECT 3 = ANY(ARRAY[ARRAY[1,2],ARRAY[3,4]])", "true"],
    ["SELECT 4 = ALL(ARRAY[ARRAY[4,4],ARRAY[4,4]])", "true"],
    // A custom lower bound is irrelevant (elements, not subscripts).
    ["SELECT 20 = ANY('[5:6]={10,20}'::int64[])", "true"],
    ["SELECT 'b' = ANY(ARRAY['a','b','c'])", "true"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("column / literal adaptation", () => {
  const db = new Database();
  execute(db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])");
  execute(db, "INSERT INTO t VALUES (1, ARRAY[10,20,30]), (2, ARRAY[40,50])");
  const cases: [string, string][] = [
    ["SELECT 20 = ANY(xs) FROM t WHERE id = 1", "true"],
    ["SELECT count(*) FROM t WHERE 20 = ANY(xs)", "1"],
    ["SELECT count(*) FROM t WHERE id = ANY(ARRAY[1,2])", "2"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("errors", () => {
  const db = new Database();
  // A non-array right side is 42809.
  assert.equal(errCode(() => execute(db, "SELECT 1 = ANY(5)")), "42809");
  // An incomparable element type is 42883.
  assert.equal(errCode(() => execute(db, "SELECT 1 = ANY(ARRAY['a','b'])")), "42883");
  // A bare untyped NULL array operand is 42P18.
  assert.equal(errCode(() => execute(db, "SELECT 1 = ANY(NULL)")), "42P18");
  // The subquery quantifier form is a deferred 0A000.
  assert.equal(errCode(() => execute(db, "SELECT 1 = ANY(SELECT 1)")), "0A000");
});
