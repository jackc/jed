// SELECT: point lookup, ORDER BY (NULLs first), IS [NOT] NULL, three-valued WHERE,
// cross-type comparison via the promotion tower, and CAST range re-checking.

import assert from "node:assert/strict";
import { test } from "node:test";
import { execute } from "../src/lib.ts";
import { dbWith, errCode, query } from "./util.ts";

function seed() {
  return dbWith([
    "CREATE TABLE t (id int32 PRIMARY KEY, v int16)",
    "INSERT INTO t VALUES (1, 10)",
    "INSERT INTO t VALUES (2, NULL)",
    "INSERT INTO t VALUES (3, 30)",
  ]);
}

test("point lookup by primary key", () => {
  assert.deepStrictEqual(query(seed(), "SELECT v FROM t WHERE id = 1"), [["10"]]);
});

test("ORDER BY puts NULLs first (ascending)", () => {
  assert.deepStrictEqual(query(seed(), "SELECT v FROM t ORDER BY v"), [
    ["NULL"],
    ["10"],
    ["30"],
  ]);
});

test("IS NULL / IS NOT NULL", () => {
  assert.deepStrictEqual(query(seed(), "SELECT id FROM t WHERE v IS NULL"), [["2"]]);
  assert.deepStrictEqual(query(seed(), "SELECT id FROM t WHERE v IS NOT NULL ORDER BY id"), [
    ["1"],
    ["3"],
  ]);
});

test("a comparison with NULL is UNKNOWN, so the row is not selected", () => {
  // v = 10 must NOT match row 2 (v IS NULL): NULL = 10 is UNKNOWN, not TRUE.
  assert.deepStrictEqual(query(seed(), "SELECT id FROM t WHERE v = 10"), [["1"]]);
});

test("range comparison keeps only TRUE rows, NULLs excluded", () => {
  assert.deepStrictEqual(query(seed(), "SELECT id FROM t WHERE v >= 10 ORDER BY id"), [
    ["1"],
    ["3"],
  ]);
});

test("cross-type comparison promotes (int16 column vs int64 literal)", () => {
  const db = dbWith([
    "CREATE TABLE w (id int32 PRIMARY KEY, a int16)",
    "INSERT INTO w VALUES (1, 100)",
  ]);
  assert.deepStrictEqual(query(db, "SELECT id FROM w WHERE a = 100"), [["1"]]);
});

test("CAST narrowing out of range traps 22003", () => {
  const db = dbWith([
    "CREATE TABLE w (id int32 PRIMARY KEY, big int64)",
    "INSERT INTO w VALUES (1, 100000)",
  ]);
  assert.equal(
    errCode(() => execute(db, "SELECT CAST(big AS int16) FROM w")),
    "22003",
  );
});

test("CAST within range projects the value", () => {
  const db = dbWith([
    "CREATE TABLE w (id int32 PRIMARY KEY, big int64)",
    "INSERT INTO w VALUES (1, 1000)",
  ]);
  assert.deepStrictEqual(query(db, "SELECT CAST(big AS int16) FROM w"), [["1000"]]);
});

test("SELECT * projects all columns in declaration order", () => {
  assert.deepStrictEqual(query(seed(), "SELECT * FROM t WHERE id = 3"), [["3", "30"]]);
});

function limitDB() {
  return dbWith([
    "CREATE TABLE t (id int32 PRIMARY KEY, v int32)",
    "INSERT INTO t VALUES (1, 10)",
    "INSERT INTO t VALUES (2, 20)",
    "INSERT INTO t VALUES (3, 30)",
    "INSERT INTO t VALUES (4, 40)",
    "INSERT INTO t VALUES (5, 50)",
  ]);
}

test("LIMIT caps and OFFSET skips; the two clauses commute", () => {
  const db = limitDB();
  assert.deepStrictEqual(query(db, "SELECT id FROM t ORDER BY id LIMIT 2"), [["1"], ["2"]]);
  assert.deepStrictEqual(query(db, "SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 1"), [["2"], ["3"]]);
  assert.deepStrictEqual(query(db, "SELECT id FROM t ORDER BY id OFFSET 1 LIMIT 2"), [["2"], ["3"]]);
  assert.deepStrictEqual(query(db, "SELECT id FROM t ORDER BY id OFFSET 3"), [["4"], ["5"]]);
  // LIMIT 0 and an OFFSET past the end are empty (not errors); a huge LIMIT clamps.
  assert.deepStrictEqual(query(db, "SELECT id FROM t ORDER BY id LIMIT 0"), []);
  assert.deepStrictEqual(query(db, "SELECT id FROM t ORDER BY id OFFSET 10"), []);
  assert.equal(query(db, "SELECT id FROM t ORDER BY id LIMIT 100").length, 5);
});

test("LIMIT/OFFSET window reduces produced cost (slice before projection)", () => {
  // 5 scanned + 2 produced = 7 (spec/design/cost.md §3).
  const o = execute(limitDB(), "SELECT id FROM t ORDER BY id LIMIT 2");
  assert.equal(o.cost, 7n);
});

test("a negative LIMIT traps 2201W and a negative OFFSET traps 2201X", () => {
  assert.equal(errCode(() => execute(seed(), "SELECT id FROM t LIMIT -1")), "2201W");
  assert.equal(errCode(() => execute(seed(), "SELECT id FROM t OFFSET -1")), "2201X");
});

test("a duplicate LIMIT or OFFSET clause is a syntax error 42601", () => {
  assert.equal(errCode(() => execute(seed(), "SELECT id FROM t LIMIT 1 LIMIT 2")), "42601");
  assert.equal(errCode(() => execute(seed(), "SELECT id FROM t OFFSET 1 OFFSET 2")), "42601");
});

test("unknown column traps 42703", () => {
  assert.equal(errCode(() => execute(seed(), "SELECT nope FROM t")), "42703");
});

test("select from a missing table traps 42P01", () => {
  assert.equal(errCode(() => execute(seed(), "SELECT * FROM nope")), "42P01");
});

test("an out-of-range literal in a comparison traps 22003 (context-adaptive typing)", () => {
  // A literal that cannot be represented in the compared column's type is a type error
  // (spec/design/types.md §6), not a silent non-match — for every operator.
  const db = dbWith([
    "CREATE TABLE t (id int32 PRIMARY KEY, small int16)",
    "INSERT INTO t VALUES (1, 30000)",
  ]);
  assert.deepStrictEqual(query(db, "SELECT id FROM t WHERE small = 30000"), [["1"]]);
  for (const sql of [
    "SELECT id FROM t WHERE small = 100000",
    "SELECT id FROM t WHERE small < 100000",
    "SELECT id FROM t WHERE small > 100000",
  ]) {
    assert.equal(errCode(() => execute(db, sql)), "22003", sql);
  }
  // The context is the compared column: 5e9 fits int64 but not int32 (the id column).
  assert.equal(errCode(() => execute(db, "SELECT id FROM t WHERE id = 5000000000")), "22003");
});
