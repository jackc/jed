// Array function/operator surface — AF1 (spec/design/array-functions.md): the polymorphic
// anyarray/anyelement resolution plus the introspection (array_ndims/array_length/array_lower/
// array_upper/cardinality/array_dims) and builder (array_append/array_prepend/array_cat) functions.
// Every expected value is pinned against PostgreSQL 18. Mirrors impl/rust/tests/array_functions.rs.

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

test("introspection over 1-D arrays", () => {
  const db = new Database();
  const cases: [string, string][] = [
    ["SELECT array_length(ARRAY[10,20,30], 1)", "3"],
    ["SELECT array_length(ARRAY[10,20,30], 2)", "NULL"],
    ["SELECT array_length(ARRAY[10,20,30], 0)", "NULL"],
    ["SELECT cardinality(ARRAY[10,20,30])", "3"],
    ["SELECT array_ndims(ARRAY[10,20,30])", "1"],
    ["SELECT array_dims(ARRAY[10,20,30])", "[1:3]"],
    ["SELECT array_lower(ARRAY[10,20,30], 1)", "1"],
    ["SELECT array_upper(ARRAY[10,20,30], 1)", "3"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("introspection over empty and NULL arrays", () => {
  const db = new Database();
  const cases: [string, string][] = [
    ["SELECT array_length('{}'::int32[], 1)", "NULL"],
    ["SELECT array_ndims('{}'::int32[])", "NULL"],
    ["SELECT array_dims('{}'::int32[])", "NULL"],
    ["SELECT cardinality('{}'::int32[])", "0"],
    ["SELECT array_length(NULL::int32[], 1)", "NULL"],
    ["SELECT cardinality(NULL::int32[])", "NULL"],
    // A NULL dimension argument propagates (jed requires the cast — bare NULL in a typed slot is
    // 42883, jed's existing strictness, a divergence from PG).
    ["SELECT array_length(ARRAY[1,2,3], NULL::int32)", "NULL"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("introspection over multidim + custom lower bounds", () => {
  const db = new Database();
  const cases: [string, string][] = [
    ["SELECT array_lower('[2:4]={7,8,9}'::int32[], 1)", "2"],
    ["SELECT array_upper('[2:4]={7,8,9}'::int32[], 1)", "4"],
    ["SELECT array_dims('[2:4]={7,8,9}'::int32[])", "[2:4]"],
    ["SELECT array_ndims(ARRAY[ARRAY[1,2,3],ARRAY[4,5,6]])", "2"],
    ["SELECT array_length(ARRAY[ARRAY[1,2,3],ARRAY[4,5,6]], 2)", "3"],
    ["SELECT cardinality(ARRAY[ARRAY[1,2,3],ARRAY[4,5,6]])", "6"],
    ["SELECT array_dims(ARRAY[ARRAY[1,2,3],ARRAY[4,5,6]])", "[1:2][1:3]"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("builders append/prepend/cat", () => {
  const db = new Database();
  const cases: [string, string][] = [
    ["SELECT array_append(ARRAY[1,2,3], 4)", "{1,2,3,4}"],
    ["SELECT array_prepend(0, ARRAY[1,2,3])", "{0,1,2,3}"],
    ["SELECT array_append(NULL::int32[], 5)", "{5}"],
    ["SELECT array_append('{}'::int32[], 5)", "{5}"],
    ["SELECT array_append(ARRAY[1,2], NULL)", "{1,2,NULL}"],
    ["SELECT array_cat(ARRAY[1,2], ARRAY[3,4])", "{1,2,3,4}"],
    ["SELECT array_cat(NULL::int64[], ARRAY[1,2])", "{1,2}"],
    ["SELECT array_cat(NULL::int64[], NULL::int64[])", "NULL"],
    ["SELECT array_cat('{}'::int64[], '{}'::int64[])", "{}"],
    ["SELECT array_cat(ARRAY[ARRAY[1,2],ARRAY[3,4]], ARRAY[5,6])", "{{1,2},{3,4},{5,6}}"],
    ["SELECT array_cat(ARRAY[5,6], ARRAY[ARRAY[1,2],ARRAY[3,4]])", "{{5,6},{1,2},{3,4}}"],
    ["SELECT array_dims(array_append('[2:4]={7,8,9}'::int32[], 10))", "[2:5]"],
    ["SELECT array_dims(array_prepend(6, '[2:4]={7,8,9}'::int32[]))", "[2:5]"],
    ["SELECT array_append(ARRAY['a','b'], 'c')", "{a,b,c}"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("error cases", () => {
  const db = new Database();
  const cases: [string, string][] = [
    ["SELECT array_append(ARRAY[ARRAY[1,2],ARRAY[3,4]], 9)", "22000"],
    ["SELECT array_prepend(9, ARRAY[ARRAY[1,2],ARRAY[3,4]])", "22000"],
    ["SELECT array_cat(ARRAY[ARRAY[1,2]], ARRAY[ARRAY[3,4,5]])", "2202E"],
    ["SELECT array_cat(ARRAY[1,2], ARRAY['a','b'])", "42883"],
    ["SELECT array_length(5, 1)", "42883"],
    ["SELECT array_append(ARRAY[1,2], 'x')", "42883"],
  ];
  for (const [sql, want] of cases) assert.equal(errCode(() => execute(db, sql)), want, sql);
});
