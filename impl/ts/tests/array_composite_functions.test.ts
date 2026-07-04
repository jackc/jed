// AF7 (spec/design/array-functions.md §13): the polymorphic array function/operator surface over a
// COMPOSITE element type, plus unnest(composite[]). These complement the oracle corpus
// (suites/expr/array_composite_functions.test, suites/query/unnest_composite.test) with the two
// pieces the corpus can't carry: (a) the ARRAY[ROW(…)] constructor under a composite-column context
// (a jed extension PG rejects without a ::addr cast — the AC1 path), and (b) finer assertions on the
// composite-specific NULL rules. Every expected value is pinned against PostgreSQL 18. Mirrors
// impl/rust/tests/array_composite_functions.rs and impl/go/array_composite_functions_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import type { Session } from "../src/tooling.ts";
import { type Handle, dbWith, errCode, query, queryOutcome } from "./util.ts";

function addrDb(): Session {
  return dbWith(["CREATE TYPE addr AS (street text, zip i32)"]);
}

// val runs a one-row, one-column query and returns the rendered value ("NULL" for SQL-NULL).
function val(db: Handle, sql: string): string {
  const rows = query(db, sql);
  assert.equal(rows.length, 1, sql);
  assert.equal(rows[0]!.length, 1, sql);
  return rows[0]![0]!;
}

// col runs a one-column query and returns the rendered values.
function col(db: Handle, sql: string): string[] {
  return query(db, sql).map((r) => {
    assert.equal(r.length, 1, sql);
    return r[0]!;
  });
}

function qOut(db: Handle, sql: string) {
  const o = queryOutcome(db, sql);
  if (o.kind !== "query") throw new Error(`expected a query result for ${sql}`);
  return o;
}

test("AF7 introspectors over composite elements", () => {
  const db = addrDb();
  assert.equal(val(db, `SELECT array_length('{"(a,1)","(b,2)"}'::addr[], 1)`), "2");
  assert.equal(val(db, `SELECT cardinality('{"(a,1)"}'::addr[])`), "1");
  assert.equal(val(db, `SELECT array_ndims('{"(a,1)"}'::addr[])`), "1");
  assert.equal(val(db, `SELECT array_dims('{"(a,1)","(b,2)"}'::addr[])`), "[1:2]");
  assert.equal(val(db, `SELECT num_nulls(VARIADIC '{"(a,1)",NULL}'::addr[])`), "1");
  assert.equal(val(db, `SELECT num_nonnulls('(a,)'::addr)`), "1"); // a NULL field is a present value
});

test("AF7 containment over composite: strict value-level, total-order field-level", () => {
  const db = addrDb();
  // A composite element with a NULL FIELD is comparable (record_eq) — @> matches it...
  assert.equal(val(db, `SELECT '{"(a,)"}'::addr[] @> '{"(a,)"}'::addr[]`), "true");
  // ...but a WHOLE-element NULL matches nothing, including another NULL (strict).
  assert.equal(val(db, `SELECT '{"(a,1)",NULL}'::addr[] @> '{NULL}'::addr[]`), "false");
  assert.equal(val(db, `SELECT '{"(a,1)"}'::addr[] <@ '{"(a,1)","(b,2)"}'::addr[]`), "true");
  assert.equal(val(db, `SELECT '{"(a,1)"}'::addr[] && '{"(a,1)","(b,2)"}'::addr[]`), "true");
  assert.equal(val(db, `SELECT (NULL::addr[] @> '{"(a,1)"}'::addr[]) IS NULL`), "true");
});

// The AF7 code change #2: x op ANY/ALL(composite[]) uses the composite TOTAL ORDER, not bare-ROW 3VL.
test("AF7 quantified over composite uses total order, not 3VL", () => {
  const db = addrDb();
  assert.equal(val(db, `SELECT '(b,2)'::addr = ANY('{"(a,1)","(b,2)"}'::addr[])`), "true");
  // THE FIX: a composite NULL FIELD is comparable (PG record_eq), so = ANY is TRUE (not bare-ROW NULL).
  assert.equal(val(db, `SELECT '(a,)'::addr = ANY('{"(a,)"}'::addr[])`), "true");
  assert.equal(val(db, `SELECT '(a,)'::addr = ANY('{"(a,2)"}'::addr[])`), "false");
  // A WHOLE-element NULL is still UNKNOWN (strict at the value level).
  assert.equal(val(db, `SELECT ('(a,1)'::addr = ANY('{NULL}'::addr[])) IS NULL`), "true");
  // Ordering quantifiers use the composite total order: the NULL zip sorts last.
  assert.equal(val(db, `SELECT '(a,1)'::addr < ANY('{"(a,)"}'::addr[])`), "true");
  assert.equal(val(db, `SELECT '(a,)'::addr > ANY('{"(a,1)"}'::addr[])`), "true");
  assert.equal(val(db, `SELECT '(a,)'::addr = ALL('{"(a,)","(a,)"}'::addr[])`), "true");
  // Empty → ANY FALSE, ALL TRUE; NULL array → NULL.
  assert.equal(val(db, `SELECT '(a,1)'::addr = ANY('{}'::addr[])`), "false");
  assert.equal(val(db, `SELECT '(a,1)'::addr = ALL('{}'::addr[])`), "true");
  assert.equal(val(db, `SELECT ('(a,1)'::addr = ANY(NULL::addr[])) IS NULL`), "true");
});

// The AF7 code change #1: unnest(composite[]).
test("AF7 unnest(composite[])", () => {
  const db = addrDb();
  const out = qOut(db, `SELECT * FROM unnest('{"(a,1)","(b,2)"}'::addr[])`);
  assert.deepStrictEqual(out.columnNames, ["unnest"]);
  assert.deepStrictEqual(out.columnTypes, ["addr"]);
  assert.deepStrictEqual(col(db, `SELECT * FROM unnest('{"(a,1)","(b,2)"}'::addr[])`), [
    "(a,1)",
    "(b,2)",
  ]);
  // A NULL element → a NULL row; empty/NULL array → zero rows.
  assert.deepStrictEqual(col(db, `SELECT * FROM unnest('{"(a,1)",NULL}'::addr[])`), [
    "(a,1)",
    "NULL",
  ]);
  assert.equal(val(db, `SELECT count(*) FROM unnest('{}'::addr[])`), "0");
  assert.equal(val(db, `SELECT count(*) FROM unnest(NULL::addr[])`), "0");
  // Field access into the composite output column.
  assert.deepStrictEqual(col(db, `SELECT (u).zip FROM unnest('{"(a,1)","(b,2)"}'::addr[]) AS u`), [
    "1",
    "2",
  ]);
  // ORDER BY the whole composite column (the composite total order).
  assert.deepStrictEqual(
    col(db, `SELECT * FROM unnest('{"(b,2)","(a,1)"}'::addr[]) AS u ORDER BY u`),
    ["(a,1)", "(b,2)"],
  );
  // A non-array argument is still 42883.
  assert.equal(
    errCode(() => db.execute(`SELECT * FROM unnest('(a,1)'::addr)`)),
    "42883",
  );
});

// The jed extension: ARRAY[ROW(…)] under a composite-column context (not in the PG corpus).
test("AF7 ARRAY[ROW(…)] under a composite-column context", () => {
  const db = addrDb();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, items addr[])");
  db.execute("INSERT INTO t VALUES (1, ARRAY[ROW('Main', 90210), ROW('Side', 5)])");
  assert.equal(val(db, "SELECT (SELECT count(*) FROM unnest(o.items)) FROM t o ORDER BY id"), "2");
  assert.equal(val(db, "SELECT array_length(items, 1) FROM t ORDER BY id"), "2");
  // The other operand must be a typed composite literal (a bare ARRAY[ROW]/ROW does not adapt).
  assert.equal(val(db, `SELECT items @> '{"(Side,5)"}'::addr[] FROM t ORDER BY id`), "true");
  assert.equal(val(db, `SELECT '(Side,5)'::addr = ANY(items) FROM t ORDER BY id`), "true");
});
