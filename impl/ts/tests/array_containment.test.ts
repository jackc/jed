// Array function/operator surface — AF4 (spec/design/array-functions.md §10): the containment /
// overlap operators `@>` (contains), `<@` (contained by), `&&` (overlaps). Every expected value is
// pinned against PostgreSQL 18 (the strict-element-equality NULL rule especially — §10.1 #1).
// Mirrors impl/rust/tests/array_containment.rs.
//
// jed types a bare integer literal / ARRAY[…] constructor as int64, so the tests pair bare arrays
// with int64[] casts; the element hint comes from the FIRST array operand (§5 #8).

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

test("@> — contains basics", () => {
  const db = new Database();
  const cases: [string, string][] = [
    ["SELECT ARRAY[1,2,3] @> ARRAY[2]", "true"],
    ["SELECT ARRAY[1,2,3] @> ARRAY[2,4]", "false"],
    ["SELECT ARRAY[1,2,3] @> ARRAY[3,2,1]", "true"], // order irrelevant
    ["SELECT ARRAY[1,2,2,3] @> ARRAY[2,2,2]", "true"], // duplicates irrelevant
    ["SELECT ARRAY[1,2,3] @> '{}'::int64[]", "true"], // empty contained by anything
    ["SELECT '{}'::int64[] @> ARRAY[1]", "false"],
    ["SELECT '{}'::int64[] @> '{}'::int64[]", "true"],
    ["SELECT ARRAY['a','b','c'] @> ARRAY['b']", "true"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("<@ / && — contained-by and overlap", () => {
  const db = new Database();
  const cases: [string, string][] = [
    ["SELECT ARRAY[2] <@ ARRAY[1,2,3]", "true"],
    ["SELECT ARRAY[2,4] <@ ARRAY[1,2,3]", "false"],
    ["SELECT '{}'::int64[] <@ ARRAY[1]", "true"],
    ["SELECT ARRAY[1,2] && ARRAY[2,3]", "true"],
    ["SELECT ARRAY[1,2] && ARRAY[3,4]", "false"],
    ["SELECT ARRAY[1,2] && '{}'::int64[]", "false"], // empty overlaps nothing
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("@> / && — STRICT element equality (a NULL element matches nothing)", () => {
  const db = new Database();
  const cases: [string, string][] = [
    ["SELECT ARRAY[1,2,NULL] @> ARRAY[2]", "true"],
    ["SELECT ARRAY[1,2,NULL] @> '{NULL}'::int64[]", "false"],
    ["SELECT ARRAY[1,2,3] @> '{NULL}'::int64[]", "false"],
    ["SELECT '{NULL,NULL}'::int64[] @> '{NULL}'::int64[]", "false"],
    ["SELECT ARRAY[1,NULL] && '{NULL}'::int64[]", "false"],
    ["SELECT ARRAY[1,NULL] && ARRAY[1]", "true"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("@> / && — a NULL whole-array operand propagates to NULL", () => {
  const db = new Database();
  const cases: [string, string][] = [
    ["SELECT NULL::int64[] @> ARRAY[1]", "NULL"],
    ["SELECT ARRAY[1] @> NULL::int64[]", "NULL"],
    ["SELECT NULL::int64[] && ARRAY[1]", "NULL"],
    ["SELECT ARRAY[1] <@ NULL::int64[]", "NULL"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("@> / && — any dimensionality (flattened multiset)", () => {
  const db = new Database();
  const cases: [string, string][] = [
    ["SELECT ARRAY[ARRAY[1,2],ARRAY[3,4]] @> ARRAY[3]", "true"],
    ["SELECT ARRAY[ARRAY[1,2],ARRAY[3,4]] && ARRAY[4,9]", "true"],
    ["SELECT '[5:7]={10,20,30}'::int32[] @> ARRAY[20]", "true"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("@> — precedence and literal adaptation", () => {
  const db = new Database();
  const cases: [string, string][] = [
    ["SELECT ARRAY[1,2] || ARRAY[3] @> ARRAY[3]", "true"], // (a||b) @> c — shares ||'s rung
    ["SELECT ARRAY[3] @> ARRAY[1 + 2]", "true"], // binds looser than +
    ["SELECT ARRAY[1,2] @> ARRAY[2] = true", "true"], // binds tighter than =
    ["SELECT '{1,2,3}'::int32[] @> ARRAY[2]", "true"], // bare ARRAY adapts to int32
    ["SELECT '{2}'::int32[] <@ ARRAY[1,2,3]", "true"],
  ];
  for (const [sql, want] of cases) assert.equal(val(db, sql), want, sql);
});

test("@> / && — errors", () => {
  const db = new Database();
  const cases: [string, string][] = [
    ["SELECT 5 @> ARRAY[1]", "42883"], // non-array operand
    ["SELECT ARRAY[1] @> 5", "42883"],
    ["SELECT ARRAY[1,2] @> ARRAY['a','b']", "42883"], // element-type mismatch
    ["SELECT ARRAY[1] && 5", "42883"],
    ["SELECT 1 @ 2", "42601"], // lone @ — no unary-@
    ["SELECT 1 & 2", "42601"], // lone & — no bitwise-and
  ];
  for (const [sql, want] of cases) assert.equal(errCode(() => execute(db, sql)), want, sql);
});
