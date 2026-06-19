// Subqueries — scalar `(SELECT …)`, `x [NOT] IN (SELECT …)`, `[NOT] EXISTS (SELECT …)`, both
// uncorrelated and CORRELATED. These complement the conformance corpus
// (spec/conformance/suites/subquery) with finer-grained per-feature assertions: the uncorrelated
// fold (execute once → constant, cost added once), the typed NULL of an empty scalar, three-valued
// IN, EXISTS ignoring the select list; and for correlated subqueries the scope-chain resolution,
// per-outer-row execution + cost, correlation in a JOIN ON and inside an aggregate argument,
// multi-level + skip-level (grandparent) correlation, and the error / narrowing codes
// (21000 / 42601 / 0A000). See spec/design/grammar.md §26.

import assert from "node:assert/strict";
import { test } from "node:test";
import { EngineError, execute, executeParams, intValue } from "../src/lib.ts";
import type { Value } from "../src/lib.ts";
import { dbWith, errCode } from "./util.ts";

function ab() {
  return dbWith([
    "CREATE TABLE a (id int32 PRIMARY KEY, k int32)",
    "CREATE TABLE b (id int32 PRIMARY KEY, k int32)",
    "CREATE TABLE one (id int32 PRIMARY KEY)",
    "INSERT INTO a VALUES (1, 10), (2, 20), (3, 30)",
    "INSERT INTO b VALUES (1, 20), (2, 30), (3, 40)",
    "INSERT INTO one VALUES (1)",
  ]);
}

test("a subquery's cost is added once, the folded constant a leaf", () => {
  const db = ab();
  const base = execute(db, "SELECT id FROM a WHERE k = 999").cost;
  const withSub = execute(db, "SELECT id FROM a WHERE k = (SELECT max(k) FROM b)").cost;
  // The folded constant is a leaf, so the only delta is the subquery's own cost (1 page_read +
  // 3 scan + 3 accumulate + 1 produced = 8), added exactly once.
  assert.strictEqual(withSub - base, 8n);
});

// ---- a correlated subquery's structural error is raised at plan time (kept per review) ------

test("a subquery's inner error is raised over an empty outer (plan-once)", () => {
  // The subquery is planned once, so a >1-column error fires even when the outer is empty. The
  // corpus pins the same guarantee via an empty inner filter, not an empty outer — so this is kept.
  const db = dbWith([
    "CREATE TABLE e (id int32 PRIMARY KEY, v int32)",
    "CREATE TABLE f (id int32 PRIMARY KEY, v int32)",
    "INSERT INTO f VALUES (1, 1)",
  ]);
  assert.strictEqual(
    errCode(() => execute(db, "SELECT (SELECT id, v FROM f WHERE v = e.v) FROM e")),
    "42601",
  );
});

// ---- subqueries in UPDATE / DELETE (spec/design/grammar.md §26) -----------------------------
// A subquery is legal in a DELETE/UPDATE WHERE and an UPDATE assignment RHS. An uncorrelated one
// folds once (cost added once); a correlated one references the TARGET row via the per-row outer
// environment and re-runs per matching row. The mutation stays two-phase / all-or-nothing: the
// subquery reads the pre-statement snapshot (DELETE collects keys first; UPDATE writes in phase 2).

test("a correlated mutation subquery's cost is per row, not folded once", () => {
  // A correlated DELETE subquery re-runs per scanned row; an uncorrelated one folds once, so the
  // correlated cost exceeds the uncorrelated baseline — proving the per-row execution (CLAUDE.md §13).
  const corr = execute(ab(), "DELETE FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k)").cost;
  const uncorr = execute(ab(), "DELETE FROM a WHERE k IN (SELECT k FROM b)").cost;
  assert.ok(corr > uncorr, `correlated ${corr} should exceed uncorrelated ${uncorr}`);
});

// ---- bind parameters inside a subquery (spec/design/grammar.md §26) -------------------------
// A $N inside a subquery is allowed once it gets a type from an INNER context; inference is
// statement-wide (one ParamTypes threaded through the whole plan tree), so the same $N may be used
// inside and outside, and a correlated subquery may compare a $N against the outer row.

function idsP(db: ReturnType<typeof ab>, sql: string, params: Value[]): bigint[] {
  const o = executeParams(db, sql, params);
  if (o.kind !== "query") throw new Error(`expected a query result for ${sql}`);
  return o.rows.map((r) => (r[0]!.kind === "int" ? r[0]!.int : -1n));
}

test("a $N inside a subquery, typed by an inner context", () => {
  const db = ab();
  // $1 typed by `b.k = $1` (inner) AND correlated to the outer a.k: survive iff some b.k equals
  // both $1 and a.k. a.k ∈ {10,20,30}, b.k ∈ {20,30,40}; with $1=20 only a.id=2 survives.
  assert.deepStrictEqual(
    idsP(db, "SELECT id FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = $1 AND b.k = a.k) ORDER BY id", [intValue(20n)]),
    [2n],
  );
  // $1 typed by `b.id = $1` inside an IN subquery (b.id=1 -> b.k=20 -> a.id=2).
  assert.deepStrictEqual(
    idsP(db, "SELECT id FROM a WHERE k IN (SELECT b.k FROM b WHERE b.id = $1) ORDER BY id", [intValue(1n)]),
    [2n],
  );
  // The same $1 used OUTSIDE and INSIDE the subquery — one statement-wide inference.
  assert.deepStrictEqual(
    idsP(db, "SELECT id FROM a WHERE k > $1 AND EXISTS (SELECT 1 FROM b WHERE b.k = $1 + 10) ORDER BY id", [intValue(10n)]),
    [2n, 3n],
  );
});

test("a $N with no type context anywhere is 42P18", () => {
  // A $N whose only position is a context-free select-list slot can't be typed -> 42P18, even with
  // a value bound (the type, not the value, is missing). PG diverges (defaults to text).
  const db = ab();
  let code = "";
  try {
    executeParams(db, "SELECT id FROM a WHERE k = (SELECT $1 FROM b LIMIT 1)", [intValue(10n)]);
  } catch (e) {
    if (e instanceof EngineError) code = e.code();
    else throw e;
  }
  assert.strictEqual(code, "42P18");
});
