// Common table expressions — `WITH [RECURSIVE] name [(cols)] AS [NOT] MATERIALIZED (query) [, …]
// <query>` (spec/design/cte.md, spec/design/recursive-cte.md). The row/name/error assertions and the
// inline/materialize cost contract live in the shared conformance corpus
// (spec/conformance/suites/cte/*.test). What remains here is what the corpus cannot express: the
// MATERIALIZED / NOT MATERIALIZED hint cost split (13/23), and — for WITH RECURSIVE — the
// cost-ceiling termination of a non-terminating recursion (54P01) and the inert materialization hint.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, EngineError, execute } from "../src/lib.ts";
import { dbWith, errCode, query } from "./util.ts";

// A 3-row, single-node table t(id, n) = {(1,10),(2,20),(3,30)}.
function t3(): Database {
  return dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, n i32)",
    "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
  ]);
}

function cost(db: Database, sql: string): bigint {
  return execute(db, sql).cost;
}

test("MATERIALIZED / NOT MATERIALIZED hints force the mode", () => {
  const db = t3();
  // MATERIALIZED forces a single-reference CTE to buffer: body once (7) + 3 cte_scan_row + 3
  // row_produced = 13 (vs the inlined 10).
  assert.strictEqual(
    cost(db, "WITH c AS MATERIALIZED (SELECT id FROM t) SELECT id FROM c ORDER BY id"),
    13n,
  );
  // NOT MATERIALIZED forces a two-reference CTE to inline (each reference re-runs the body): two
  // bodies (2x7) + 9 row_produced = 23 (vs the materialized 22).
  assert.strictEqual(
    cost(
      db,
      "WITH c AS NOT MATERIALIZED (SELECT id FROM t) SELECT a.id, b.id FROM c a CROSS JOIN c b",
    ),
    23n,
  );
});

// A non-terminating recursion (UNION ALL with no stopping predicate) is bounded by the cost ceiling.
// Each iteration is cheap (a 1-row working table), so this trips 54P01 ONLY through the CONTINUOUS
// cross-iteration meter (recursive-cte.md §5) — the untrusted-query safety mechanism doing real
// work. A per-iteration meter would never fire here, so the corpus cannot express it.
test("WITH RECURSIVE unbounded recursion aborts at the cost ceiling", () => {
  const db = new Database();
  db.setMaxCost(1000n);
  assert.throws(
    () =>
      execute(
        db,
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c) SELECT n FROM c",
      ),
    (e: unknown) => e instanceof EngineError && e.code() === "54P01",
    "an unbounded recursion must abort 54P01, not loop forever",
  );
});

// A recursion whose total cost fits under the ceiling runs to completion (the ceiling bounds the
// actual accrued cost); the 5-row counter accrues 29 (the corpus cost contract).
test("WITH RECURSIVE under the ceiling succeeds", () => {
  const db = new Database();
  db.setMaxCost(1000n);
  assert.strictEqual(
    cost(
      db,
      "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 5) SELECT n FROM c",
    ),
    29n,
  );
});

// A recursive CTE is ALWAYS materialized — NOT MATERIALIZED is inert (recursive-cte.md §1), so a
// single-reference recursive CTE still iterates to a fixpoint (3 rows, cost 17) rather than inlining.
test("WITH RECURSIVE materialization hint is inert", () => {
  const db = new Database();
  for (const hint of ["", "MATERIALIZED ", "NOT MATERIALIZED "]) {
    const r = execute(
      db,
      `WITH RECURSIVE c(n) AS ${hint}(SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 3) SELECT n FROM c ORDER BY n`,
    );
    assert.equal(r.kind, "query", `hint ${JSON.stringify(hint)} kind`);
    if (r.kind !== "query") throw new Error("unreachable");
    assert.strictEqual(r.rows.length, 3, `hint ${JSON.stringify(hint)} rows`);
    assert.strictEqual(r.cost, 17n, `hint ${JSON.stringify(hint)} cost`);
  }
});

// The nested-WITH narrowing (spec/design/cte.md §7): a nested WITH establishes its OWN CTE scope and
// does NOT inherit the enclosing statement's CTE bindings — a documented DIVERGENCE from PostgreSQL
// (which inherits them), so it cannot live in the oracle corpus. Inside the nested WITH, an enclosing
// CTE name with no base table is 42P01; one that shadows a base table reads the BASE TABLE (PG would
// read the CTE).
test("nested WITH does not inherit enclosing CTEs", () => {
  // (a) No base table named e: the inner reference to the enclosing CTE e is unresolved -> 42P01.
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, n i32)",
    "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
  ]);
  assert.strictEqual(
    errCode(() =>
      execute(
        db,
        "WITH e AS (SELECT 1 AS v) SELECT * FROM (WITH ic AS (SELECT v FROM e) SELECT v FROM ic) s",
      ),
    ),
    "42P01",
  );

  // (b) A base table e exists: inside the nested WITH the enclosing CTE e is invisible, so the
  // reference resolves to the BASE TABLE (rows are the table's, not the CTE's). PG diverges.
  const db2 = dbWith(["CREATE TABLE e (v i32 PRIMARY KEY)", "INSERT INTO e VALUES (7), (8)"]);
  assert.deepEqual(
    query(
      db2,
      "WITH e AS (SELECT 1 AS v) SELECT v FROM (WITH ic AS (SELECT v FROM e) SELECT v FROM ic) s ORDER BY v",
    ),
    [["7"], ["8"]],
  );
});
