// Phase 1: the general expression evaluator — integer arithmetic (+ - * / %, unary
// minus), the expression-only boolean type, comparisons-as-values, AND/OR/NOT Kleene
// connectives, operator precedence, and parentheses. These complement the conformance
// corpus (spec/conformance/suites/expr/) with finer-grained per-feature assertions.

import assert from "node:assert/strict";
import { test } from "node:test";
import { execute } from "../src/lib.ts";
import { dbWith, errCode, query } from "./util.ts";

function seed() {
  return dbWith([
    "CREATE TABLE t (id int32 PRIMARY KEY, a int32, b int32)",
    "INSERT INTO t VALUES (1, 6, 4)",
    "INSERT INTO t VALUES (2, 20, 6)",
    "INSERT INTO t VALUES (3, -7, 3)",
  ]);
}

test("arithmetic and operator precedence", () => {
  const db = seed();
  assert.deepStrictEqual(query(db, "SELECT 6 + 4 * 2 FROM t WHERE id = 1"), [["14"]]); // * before +
  assert.deepStrictEqual(query(db, "SELECT (6 + 4) * 2 FROM t WHERE id = 1"), [["20"]]); // parens
  assert.deepStrictEqual(query(db, "SELECT a + b FROM t WHERE id = 2"), [["26"]]);
  assert.deepStrictEqual(query(db, "SELECT a * b FROM t WHERE id = 2"), [["120"]]);
  assert.deepStrictEqual(query(db, "SELECT a / b FROM t WHERE id = 3"), [["-2"]]); // trunc toward 0
  assert.deepStrictEqual(query(db, "SELECT a % b FROM t WHERE id = 3"), [["-1"]]); // sign of dividend
});

test("arithmetic in WHERE", () => {
  assert.deepStrictEqual(query(seed(), "SELECT id FROM t WHERE a + b = 26 ORDER BY id"), [["2"]]);
});

test("overflow traps at the result type, not int64", () => {
  const db = dbWith([
    "CREATE TABLE e (id int32 PRIMARY KEY, a int32, b int32)",
    "INSERT INTO e VALUES (1, 2147483647, 1)",
  ]);
  assert.equal(errCode(() => execute(db, "SELECT a + b FROM e WHERE id = 1")), "22003");
  assert.deepStrictEqual(query(db, "SELECT CAST(a AS int64) + b FROM e WHERE id = 1"), [
    ["2147483648"],
  ]);
});

test("division and modulo by zero trap 22012", () => {
  const db = seed();
  assert.equal(errCode(() => execute(db, "SELECT a / 0 FROM t WHERE id = 1")), "22012");
  assert.equal(errCode(() => execute(db, "SELECT a % 0 FROM t WHERE id = 1")), "22012");
});

test("unary minus and int64 minimum", () => {
  const db = seed();
  assert.deepStrictEqual(query(db, "SELECT -a FROM t WHERE id = 1"), [["-6"]]);
  assert.deepStrictEqual(query(db, "SELECT - -a FROM t WHERE id = 1"), [["6"]]);
  assert.deepStrictEqual(query(db, "SELECT -9223372036854775808 FROM t WHERE id = 1"), [
    ["-9223372036854775808"],
  ]);
  assert.equal(errCode(() => execute(db, "SELECT 9223372036854775808 FROM t WHERE id = 1")), "22003");
  assert.equal(errCode(() => execute(db, "SELECT 9223372036854775809 FROM t WHERE id = 1")), "42601");
});

test("comparisons project booleans (true / false / NULL)", () => {
  const db = dbWith([
    "CREATE TABLE t (id int32 PRIMARY KEY, a int32, b int32)",
    "INSERT INTO t VALUES (1, 5, 5)",
    "INSERT INTO t VALUES (2, 5, 9)",
    "INSERT INTO t VALUES (3, 5, NULL)",
  ]);
  assert.deepStrictEqual(query(db, "SELECT a = b FROM t ORDER BY id"), [
    ["true"],
    ["false"],
    ["NULL"],
  ]);
  assert.deepStrictEqual(query(db, "SELECT TRUE FROM t WHERE id = 1"), [["true"]]);
  assert.deepStrictEqual(query(db, "SELECT FALSE FROM t WHERE id = 1"), [["false"]]);
});

test("Kleene connectives", () => {
  const db = dbWith([
    "CREATE TABLE tv (id int32 PRIMARY KEY, p int32, q int32)",
    "INSERT INTO tv VALUES (1, 0, 0)",
    "INSERT INTO tv VALUES (2, 0, 1)",
  ]);
  // false AND unknown = false (a dominant FALSE absorbs NULL).
  assert.deepStrictEqual(query(db, "SELECT (p = 1) AND (q = NULL) FROM tv WHERE id = 1"), [
    ["false"],
  ]);
  // true OR unknown = true.
  assert.deepStrictEqual(query(db, "SELECT (q = 1) OR (p = NULL) FROM tv WHERE id = 2"), [["true"]]);
  // NOT unknown = unknown (genuine propagation).
  assert.deepStrictEqual(query(db, "SELECT NOT (p = NULL) FROM tv WHERE id = 1"), [["NULL"]]);
});

test("type errors and boolean narrowings", () => {
  const db = seed();
  assert.equal(errCode(() => execute(db, "SELECT id FROM t WHERE a")), "42804");
  assert.equal(errCode(() => execute(db, "SELECT id FROM t WHERE a AND b")), "42804");
  assert.equal(errCode(() => execute(db, "SELECT (a = b) + 1 FROM t WHERE id = 1")), "42804");
  assert.equal(errCode(() => execute(db, "SELECT id FROM t WHERE (a = b) = (a = b)")), "42804");
  assert.equal(
    errCode(() => execute(db, "CREATE TABLE bt (id int32 PRIMARY KEY, flag boolean)")),
    "0A000",
  );
  assert.equal(errCode(() => execute(db, "SELECT CAST(a AS boolean) FROM t WHERE id = 1")), "0A000");
  assert.equal(errCode(() => execute(db, "SELECT CAST(a = b AS int32) FROM t WHERE id = 1")), "42804");
});
