// Array function/operator surface — AF6 (spec/design/array-functions.md §12): the VARIADIC call
// syntax + variadic overload resolution, spent on the engine's first VARIADIC built-ins
// num_nulls / num_nonnulls (count the NULL / non-NULL arguments → int32). Every expected value is
// pinned against PostgreSQL 18. Mirrors impl/rust/tests/array_variadic.rs.

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

test("VARIADIC spread form counts", () => {
  const db = new Database();
  const cases: [string, string][] = [
    ["SELECT num_nulls(1, NULL, 3)", "1"],
    ["SELECT num_nonnulls(1, NULL, 3)", "2"],
    ["SELECT num_nulls(NULL)", "1"], // a single NULL arg — never NULL (non-strict)
    ["SELECT num_nonnulls(NULL)", "0"],
    ["SELECT num_nulls(1, 'a', true, NULL)", "1"], // heterogeneous ("any" element family)
    ["SELECT num_nonnulls(1, 'a', true, NULL)", "3"],
    ["SELECT num_nulls(ARRAY[1,NULL,3])", "0"], // a single non-VARIADIC array is ONE value
    ["SELECT num_nonnulls(ARRAY[1,NULL,3])", "1"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("VARIADIC array form counts elements", () => {
  const db = new Database();
  const cases: [string, string][] = [
    ["SELECT num_nulls(VARIADIC ARRAY[1,NULL,3])", "1"],
    ["SELECT num_nonnulls(VARIADIC ARRAY[1,NULL,3])", "2"],
    ["SELECT num_nulls(VARIADIC '{}'::int32[])", "0"], // empty array → 0
    ["SELECT num_nulls(VARIADIC '{{1,2},{NULL,4}}'::int32[])", "1"], // multidim flattens
    ["SELECT num_nulls(VARIADIC NULL::int32[])", "NULL"], // NULL whole-array → NULL
    ["SELECT num_nonnulls(VARIADIC NULL::int32[])", "NULL"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("VARIADIC errors", () => {
  const db = new Database();
  assert.equal(errCode(() => execute(db, "SELECT num_nulls(VARIADIC 5)")), "42804"); // non-array
  assert.equal(errCode(() => execute(db, "SELECT num_nulls(VARIADIC NULL)")), "42804"); // bare NULL
  assert.equal(errCode(() => execute(db, "SELECT num_nulls()")), "42883"); // spread needs ≥1 arg
  assert.equal(errCode(() => execute(db, "SELECT abs(VARIADIC ARRAY[1])")), "42883"); // non-variadic fn
  assert.equal(errCode(() => execute(db, "SELECT num_nulls(x => 1)")), "42883"); // named notation
  assert.equal(errCode(() => execute(db, "SELECT num_nulls(VARIADIC ARRAY[1], 2)")), "42601"); // last only
});

test("VARIADIC over a column", () => {
  const db = new Database();
  execute(db, "CREATE TABLE t (id int32 PRIMARY KEY, xs int32[])");
  execute(db, "INSERT INTO t VALUES (1, ARRAY[1,NULL,3]), (2, '{}'), (3, NULL)");
  const rows = query(db, "SELECT num_nulls(VARIADIC xs) FROM t ORDER BY id");
  assert.deepEqual(rows, [["1"], ["0"], ["NULL"]]);
});
