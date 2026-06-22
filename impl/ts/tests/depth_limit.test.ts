// Expression / query nesting-depth limit (CLAUDE.md §13; spec/design/cost.md §7). The §13
// native-stack-safety gate: the recursive-descent parser and the resolve/eval walks recurse to a
// statement's nesting depth, so deeply-nested untrusted input would overflow the call stack BEFORE
// the cost meter runs — 54P01 cannot catch it (in this core an overflow is an uncatchable-by-design
// V8 RangeError). A fixed MAX_EXPR_DEPTH checked in the parser throws 54001 (statement_too_complex)
// instead. The conformance corpus (spec/conformance/suites/resource/depth_limit.test) pins the
// cross-core boundary; this exercises the per-vector boundary and that the abort is max_cost-blind.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, EngineError, execute, parseSQL } from "../src/lib.ts";
import { MAX_EXPR_DEPTH } from "../src/parser.ts";

function depthDB(): Database {
  const db = new Database();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  execute(db, "INSERT INTO t VALUES (1, 1)");
  return db;
}

// codeOf returns the SQLSTATE of running sql, or "ok" if it succeeded.
function codeOf(db: Database, sql: string): string {
  try {
    execute(db, sql);
    return "ok";
  } catch (e) {
    return e instanceof EngineError ? e.code() : `non-engine:${e}`;
  }
}

// depthChain builds `1 + 1 + … + 1` with n `+` operators over one row; its parsed depth is n+1.
function depthChain(n: number): string {
  return "SELECT " + "1 + ".repeat(n) + "1 FROM t";
}

test("the depth limit is generous (256)", () => {
  // Far above any realistic query, so ordinary SQL is never rejected (spec/design/cost.md §7).
  assert.strictEqual(MAX_EXPR_DEPTH, 256);
});

test("a deep operator chain aborts with 54001", () => {
  const db = depthDB();
  // One level past the limit aborts at parse time (the additive loop's counter, O(1) stack).
  assert.strictEqual(codeOf(db, depthChain(MAX_EXPR_DEPTH)), "54001");
  assert.strictEqual(codeOf(db, depthChain(MAX_EXPR_DEPTH * 4)), "54001");
  // A moderately-nested expression still evaluates end to end.
  assert.strictEqual(codeOf(db, depthChain(64)), "ok");
});

test("the exact boundary is MAX_EXPR_DEPTH", () => {
  // Pin the precise accept/reject boundary at the parser (where the 54001 is raised): a `1+1+…`
  // chain parses with O(1) parser stack, so MAX_EXPR_DEPTH-1 levels parse fine and MAX_EXPR_DEPTH
  // is the first rejected depth. This is the cross-core contract the corpus mirrors.
  assert.doesNotThrow(() => parseSQL(depthChain(MAX_EXPR_DEPTH - 1)));
  assert.throws(
    () => parseSQL(depthChain(MAX_EXPR_DEPTH)),
    (e: unknown) => e instanceof EngineError && e.code() === "54001",
  );
});

test("the abort is independent of max_cost", () => {
  // The overflow this guards strikes during PARSE, before the meter runs — so even an unlimited (or
  // tiny) ceiling cannot let a stack-busting statement through (CLAUDE.md §13).
  const db = depthDB();
  db.setMaxCost(0n); // unlimited
  assert.strictEqual(codeOf(db, depthChain(MAX_EXPR_DEPTH * 8)), "54001");
  db.setMaxCost(1n); // tightest possible ceiling
  assert.strictEqual(codeOf(db, depthChain(MAX_EXPR_DEPTH * 8)), "54001");
});

test("every nesting vector aborts, not crashes", () => {
  // Each recursion vector — nested parens, ARRAY, NOT, unary minus, scalar subqueries, postfix
  // casts, and UNION chains — is bounded by the same counter and returns 54001 deterministically
  // rather than overflowing the native stack. n well past the limit for each.
  const db = depthDB();
  const n = MAX_EXPR_DEPTH * 2;
  const vectors = [
    `SELECT ${"(".repeat(n)}1${")".repeat(n)} FROM t`,
    `SELECT ${"ARRAY[".repeat(n)}1${"]".repeat(n)} FROM t`,
    `SELECT ${"NOT ".repeat(n)}true FROM t`,
    `SELECT ${"- ".repeat(n)}1 FROM t`,
    `SELECT ${"(SELECT ".repeat(n)}1${")".repeat(n)} FROM t`,
    `SELECT 1${"::int4".repeat(n)} FROM t`,
    `SELECT 1${" UNION ALL SELECT 1".repeat(n)}`,
  ];
  for (const sql of vectors) {
    assert.strictEqual(
      codeOf(db, sql),
      "54001",
      `a ${n}-deep vector should be 54001: ${sql.slice(0, 40)}…`,
    );
  }
});

test("deep nesting in WHERE and CHECK is bounded", () => {
  // The guard sits in the parser, so it protects every clause holding an expression — WHERE and a
  // CHECK constraint included (these reach the pre-resolve structural walks, which the parser bound
  // keeps shallow).
  const db = depthDB();
  const pred = "1 + ".repeat(MAX_EXPR_DEPTH + 2) + "1";
  assert.strictEqual(codeOf(db, `SELECT v FROM t WHERE ${pred} = 0`), "54001");
  assert.strictEqual(codeOf(db, `CREATE TABLE u (a i32 CHECK (${pred} > 0))`), "54001");
});
