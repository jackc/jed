// Array function/operator surface — AF1 (spec/design/array-functions.md): the polymorphic
// anyarray/anyelement resolution plus the introspection (array_ndims/array_length/array_lower/
// array_upper/cardinality/array_dims) and builder (array_append/array_prepend/array_cat) functions.
// Every expected value is pinned against PostgreSQL 18. Mirrors impl/rust/tests/array_functions.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Engine, execute } from "../src/lib.ts";
import { errCode, query } from "./util.ts";

// val runs a one-column, one-row scalar query and returns the rendered value.
function val(db: Engine, sql: string): string {
  const rows = query(db, sql);
  assert.equal(rows.length, 1, sql);
  assert.equal(rows[0]!.length, 1, sql);
  return rows[0]![0]!;
}

test("introspection over multidim + custom lower bounds", () => {
  const db = new Engine();
  const cases: [string, string][] = [
    ["SELECT array_lower('[2:4]={7,8,9}'::i32[], 1)", "2"],
    ["SELECT array_upper('[2:4]={7,8,9}'::i32[], 1)", "4"],
    ["SELECT array_dims('[2:4]={7,8,9}'::i32[])", "[2:4]"],
    ["SELECT array_ndims(ARRAY[ARRAY[1,2,3],ARRAY[4,5,6]])", "2"],
    ["SELECT array_length(ARRAY[ARRAY[1,2,3],ARRAY[4,5,6]], 2)", "3"],
    ["SELECT cardinality(ARRAY[ARRAY[1,2,3],ARRAY[4,5,6]])", "6"],
    ["SELECT array_dims(ARRAY[ARRAY[1,2,3],ARRAY[4,5,6]])", "[1:2][1:3]"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("error cases", () => {
  const db = new Engine();
  const cases: [string, string][] = [
    ["SELECT array_append(ARRAY[ARRAY[1,2],ARRAY[3,4]], 9)", "22000"],
    ["SELECT array_prepend(9, ARRAY[ARRAY[1,2],ARRAY[3,4]])", "22000"],
    ["SELECT array_cat(ARRAY[ARRAY[1,2]], ARRAY[ARRAY[3,4,5]])", "2202E"],
    ["SELECT array_cat(ARRAY[1,2], ARRAY['a','b'])", "42883"],
    ["SELECT array_length(5, 1)", "42883"],
    ["SELECT array_append(ARRAY[1,2], 'x')", "42883"],
  ];
  for (const [sql, want] of cases)
    assert.equal(
      errCode(() => execute(db, sql)),
      want,
      sql,
    );
});
