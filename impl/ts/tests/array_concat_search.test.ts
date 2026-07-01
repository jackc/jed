// Array function/operator surface — AF2 (spec/design/array-functions.md §8): the `||` concatenation
// operator and the search/edit functions array_remove/array_replace/array_position/array_positions.
// Every expected value is pinned against PostgreSQL 18. Mirrors impl/rust/tests/array_concat_search.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database } from "../src/tooling.ts";
import { type Handle, errCode, query } from "./util.ts";

// val runs a one-column, one-row scalar query and returns the rendered value.
function val(db: Handle, sql: string): string {
  const rows = query(db, sql);
  assert.equal(rows.length, 1, sql);
  assert.equal(rows[0]!.length, 1, sql);
  return rows[0]![0]!;
}

test("|| — the three concatenation forms", () => {
  const db = Database.newInMemory().session();
  const cases: [string, string][] = [
    ["SELECT ARRAY[1,2] || ARRAY[3,4]", "{1,2,3,4}"],
    ["SELECT ARRAY[1,2] || 3", "{1,2,3}"],
    ["SELECT 0 || ARRAY[1,2]", "{0,1,2}"],
    ["SELECT ARRAY['a','b'] || 'c'", "{a,b,c}"],
    ["SELECT '{1,2}'::i32[] || 3", "{1,2,3}"],
    ["SELECT '{1,2}'::i32[] || ARRAY[7,8]", "{1,2,7,8}"],
    ["SELECT ARRAY[ARRAY[1,2],ARRAY[3,4]] || ARRAY[5,6]", "{{1,2},{3,4},{5,6}}"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("|| — a bare NULL operand resolves to array_cat (identity)", () => {
  const db = Database.newInMemory().session();
  const cases: [string, string][] = [
    ["SELECT ARRAY[1,2] || NULL", "{1,2}"],
    ["SELECT NULL || ARRAY[1,2]", "{1,2}"],
    ["SELECT ARRAY[1,2] || NULL::i64[]", "{1,2}"],
    ["SELECT ARRAY[1,2] || NULL::i64", "{1,2,NULL}"], // typed null element → array_append
    ["SELECT NULL::i64[] || NULL::i64[]", "NULL"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("|| — errors", () => {
  const db = Database.newInMemory().session();
  const cases: [string, string][] = [
    ["SELECT ARRAY[1,2] || ARRAY['a','b']", "42883"],
    ["SELECT 5 || ARRAY['a','b']", "42883"],
    ["SELECT 1 || 2", "42883"],
    ["SELECT ARRAY[ARRAY[1,2],ARRAY[3,4]] || 9", "22000"],
    ["SELECT ARRAY[ARRAY[1,2]] || ARRAY[ARRAY[3,4,5]]", "2202E"],
  ];
  for (const [sql, want] of cases)
    assert.equal(
      errCode(() => db.execute(sql)),
      want,
      sql,
    );
});

test("array_remove", () => {
  const db = Database.newInMemory().session();
  const cases: [string, string][] = [
    ["SELECT array_remove(ARRAY[1,2,3,2], 2)", "{1,3}"],
    ["SELECT array_remove(NULL::i32[], 2)", "NULL"],
    ["SELECT array_remove(ARRAY[1,2,3], 9)", "{1,2,3}"],
    ["SELECT array_remove('{}'::i32[], 1)", "{}"],
    ["SELECT array_remove(ARRAY[1,NULL,2,NULL], NULL)", "{1,2}"],
    ["SELECT array_remove(ARRAY[1,NULL,2], 1)", "{NULL,2}"],
    ["SELECT array_dims(array_remove('[2:4]={1,2,3}'::i32[], 2))", "[2:3]"],
    ["SELECT array_remove('[5:7]={9,9,9}'::i32[], 9)", "{}"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
  assert.equal(
    errCode(() => db.execute("SELECT array_remove(ARRAY[ARRAY[1,2],ARRAY[3,4]], 1)")),
    "0A000",
  );
});

test("array_replace", () => {
  const db = Database.newInMemory().session();
  const cases: [string, string][] = [
    ["SELECT array_replace(ARRAY[1,2,3,2], 2, 9)", "{1,9,3,9}"],
    ["SELECT array_replace(NULL::i32[], 2, 9)", "NULL"],
    ["SELECT array_replace(ARRAY[1,2,3], 8, 9)", "{1,2,3}"],
    ["SELECT array_replace(ARRAY[1,2,3], 2, NULL)", "{1,NULL,3}"],
    ["SELECT array_replace(ARRAY[1,NULL,3], NULL, 9)", "{1,9,3}"],
    ["SELECT array_replace(ARRAY[ARRAY[1,2],ARRAY[1,4]], 1, 0)", "{{0,2},{0,4}}"],
    ["SELECT array_replace('[5:7]={10,20,10}'::i32[], 10, 99)", "[5:7]={99,20,99}"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("array_position", () => {
  const db = Database.newInMemory().session();
  const cases: [string, string][] = [
    ["SELECT array_position(ARRAY[10,20,30,20], 20)", "2"],
    ["SELECT array_position(ARRAY[10,20], 99)", "NULL"],
    ["SELECT array_position(NULL::i32[], 5)", "NULL"],
    ["SELECT array_position('{}'::i32[], 5)", "NULL"],
    ["SELECT array_position(ARRAY[1,NULL,3], NULL)", "2"],
    ["SELECT array_position(ARRAY[10,20,30,20], 20, 3)", "4"],
    ["SELECT array_position(ARRAY[10,20,30], 20, 3)", "NULL"],
    ["SELECT array_position('[5:7]={10,20,30}'::i32[], 20)", "6"],
    ["SELECT array_position('[5:8]={10,20,30,20}'::i32[], 20, 7)", "8"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
  assert.equal(
    errCode(() => db.execute("SELECT array_position(ARRAY[10,20,30], 20, NULL::i32)")),
    "22004",
  );
  assert.equal(
    errCode(() => db.execute("SELECT array_position(ARRAY[ARRAY[1,2],ARRAY[3,4]], 1)")),
    "0A000",
  );
});

test("array_positions", () => {
  const db = Database.newInMemory().session();
  const cases: [string, string][] = [
    ["SELECT array_positions(ARRAY[10,20,30,20], 20)", "{2,4}"],
    ["SELECT array_positions(ARRAY[10,20], 99)", "{}"],
    ["SELECT array_positions(NULL::i32[], 5)", "NULL"],
    ["SELECT array_positions(ARRAY[1,NULL,3,NULL], NULL)", "{2,4}"],
    ["SELECT array_positions('[5:8]={10,20,30,20}'::i32[], 20)", "{6,8}"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
  assert.equal(
    errCode(() => db.execute("SELECT array_positions(ARRAY[ARRAY[1,2],ARRAY[3,4]], 1)")),
    "0A000",
  );
});
