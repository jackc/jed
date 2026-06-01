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

test("unknown column traps 42703", () => {
  assert.equal(errCode(() => execute(seed(), "SELECT nope FROM t")), "42703");
});

test("select from a missing table traps 42P01", () => {
  assert.equal(errCode(() => execute(seed(), "SELECT * FROM nope")), "42P01");
});
