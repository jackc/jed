// Common table expressions — `WITH name [(cols)] AS [NOT] MATERIALIZED (query) [, …] <query>`,
// non-recursive (spec/design/cte.md). These complement the conformance corpus
// (spec/conformance/suites/cte) with finer-grained per-feature assertions: the inline-vs-
// materialize cost split, forward-only visibility, base-table shadowing, the column-rename list,
// set-op / aggregate / JOIN bodies, CTE references inside a nested subquery, and the error /
// narrowing codes (42712 / 42P01 / 42P10 / 42703 / 0A000 / 42601). Mirrors impl/rust/tests/cte.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, execute } from "../src/lib.ts";
import { dbWith, errCode, query } from "./util.ts";

// A 3-row, single-node table t(id, n) = {(1,10),(2,20),(3,30)}.
function t3(): Database {
  return dbWith([
    "CREATE TABLE t (id int32 PRIMARY KEY, n int32)",
    "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
  ]);
}

// ints renders the first column of each row as a string (values are bigint in this core; render →
// the decimal string), matching the corpus's textual comparison.
function ints(db: Database, sql: string): string[] {
  return query(db, sql).map((r) => r[0]!);
}

function names(db: Database, sql: string): string[] {
  const o = execute(db, sql);
  if (o.kind !== "query") throw new Error(`expected a query result for ${sql}`);
  return o.columnNames;
}

function cost(db: Database, sql: string): bigint {
  return execute(db, sql).cost;
}

test("a single reference inlines (no cte_scan_row)", () => {
  const db = t3();
  assert.deepStrictEqual(ints(db, "WITH c AS (SELECT id FROM t) SELECT id FROM c ORDER BY id"), ["1", "2", "3"]);
  // A single reference INLINES: body (page_read 1 + 3 storage_row_read + 3 row_produced = 7) +
  // the outer's 3 row_produced = 10. No cte_scan_row (cost.md §3).
  assert.strictEqual(cost(db, "WITH c AS (SELECT id FROM t) SELECT id FROM c ORDER BY id"), 10n);
});

test("multiple references materialize", () => {
  const db = t3();
  // Two references MATERIALIZE: body once (7) + 6 cte_scan_row (two 3-row buffer scans) + 9
  // row_produced (3x3 product) = 22.
  const sql = "WITH c AS (SELECT id FROM t) SELECT a.id AS x, b.id AS y FROM c a CROSS JOIN c b";
  assert.strictEqual(query(db, sql).length, 9);
  assert.strictEqual(cost(db, sql), 22n);
});

test("an unreferenced CTE is planned but not executed", () => {
  const db = t3();
  // An unreferenced CTE is planned/type-checked but not executed: only SELECT 1's row_produced.
  assert.strictEqual(cost(db, "WITH c AS (SELECT id FROM t) SELECT 1"), 1n);
});

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
    cost(db, "WITH c AS NOT MATERIALIZED (SELECT id FROM t) SELECT a.id, b.id FROM c a CROSS JOIN c b"),
    23n,
  );
});

test("a later CTE references an earlier one (forward visibility)", () => {
  const db = t3();
  assert.deepStrictEqual(
    ints(
      db,
      "WITH c AS (SELECT id, n FROM t), d AS (SELECT n * 2 AS m FROM c) SELECT m FROM d ORDER BY m",
    ),
    ["20", "40", "60"],
  );
});

test("column-rename list (full + partial)", () => {
  const db = t3();
  assert.deepStrictEqual(
    names(db, "WITH c (a, b) AS (SELECT id, n FROM t) SELECT a, b FROM c ORDER BY a"),
    ["a", "b"],
  );
  // Fewer aliases than body columns: a partial rename — the first column becomes `a`, the second
  // keeps its body name `n` (PostgreSQL).
  assert.deepStrictEqual(
    names(db, "WITH c (a) AS (SELECT id, n FROM t) SELECT * FROM c ORDER BY a"),
    ["a", "n"],
  );
});

test("set-op and aggregate bodies", () => {
  const db = t3();
  assert.deepStrictEqual(
    ints(
      db,
      "WITH c AS (SELECT n FROM t WHERE id = 1 UNION ALL SELECT n FROM t WHERE id = 2) SELECT n FROM c ORDER BY n",
    ),
    ["10", "20"],
  );
  assert.deepStrictEqual(
    ints(db, "WITH c AS (SELECT count(*) AS k FROM t) SELECT k FROM c"),
    ["3"],
  );
});

test("a JOIN of two distinct CTEs", () => {
  const db = t3();
  assert.deepStrictEqual(
    ints(
      db,
      "WITH c AS (SELECT id, n FROM t), d AS (SELECT id FROM t WHERE n >= 20) " +
        "SELECT c.n FROM c JOIN d ON c.id = d.id ORDER BY c.id",
    ),
    ["20", "30"],
  );
});

test("a CTE referenced inside a nested subquery", () => {
  const db = t3();
  assert.deepStrictEqual(
    ints(
      db,
      "WITH c AS (SELECT n FROM t) SELECT id FROM t WHERE n = (SELECT max(n) FROM c) ORDER BY id",
    ),
    ["3"],
  );
});

test("shadows a base table outside the body, not inside it", () => {
  // The CTE `t` shadows the base table in the outer query, but its OWN body resolves the base
  // table (the binding is not in scope for itself — spec/design/cte.md §2).
  const db = t3();
  assert.deepStrictEqual(
    ints(db, "WITH t AS (SELECT n + 100 AS n FROM t) SELECT n FROM t ORDER BY n"),
    ["110", "120", "130"],
  );
});

test("error codes", () => {
  const db = t3();
  const cases: [string, string][] = [
    // Duplicate CTE name in one list.
    ["WITH c AS (SELECT id FROM t), c AS (SELECT id FROM t) SELECT id FROM c", "42712"],
    // Self-reference (non-recursive) — no base table `c`.
    ["WITH c AS (SELECT id FROM c) SELECT id FROM c", "42P01"],
    // Forward reference to a later CTE.
    ["WITH c AS (SELECT id FROM d), d AS (SELECT id FROM t) SELECT id FROM c", "42P01"],
    // Column-rename arity: too MANY aliases is 42P10 (too few is a legal partial rename).
    ["WITH c (a, b, x) AS (SELECT id, n FROM t) SELECT a FROM c", "42P10"],
    // A body resolves only its own scope — an unknown column is the ordinary 42703.
    ["WITH c AS (SELECT missing FROM t) SELECT id FROM c", "42703"],
    // WITH RECURSIVE is deferred.
    ["WITH RECURSIVE c AS (SELECT id FROM t) SELECT id FROM c", "0A000"],
    // A nested WITH (top-level-only narrowing) is a syntax error.
    ["WITH a AS (WITH b AS (SELECT id FROM t) SELECT id FROM b) SELECT id FROM a", "42601"],
  ];
  for (const [sql, code] of cases) {
    assert.strictEqual(
      errCode(() => execute(db, sql)),
      code,
      sql,
    );
  }
});
