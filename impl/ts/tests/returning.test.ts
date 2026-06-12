// The RETURNING clause (spec/design/grammar.md §32, cost.md §3) — covers what the corpus
// suite (dml/returning.test) cannot: the Outcome variant split (statement vs query),
// output column names, pinned costs (the projection charge, the touched-set growth, the
// fold-once/correlated split), the ceiling's all-or-nothing abort, $N binding, and
// transactional behavior. Mirrored in impl/rust/tests/returning.rs and
// impl/go/returning_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, execute, executeParams, intValue, render } from "../src/lib.ts";
import { dbWith, errCode } from "./util.ts";

function setup() {
  return dbWith([
    "CREATE TABLE t (id int32 PRIMARY KEY, v int32 DEFAULT 7, w int32)",
    "INSERT INTO t VALUES (1, 10, 100), (2, 20, 200), (3, 30, 300)",
  ]);
}

// rows runs sql (which must yield a query result) and renders its rows as strings.
function rows(db: Database, sql: string): string[][] {
  const o = execute(db, sql);
  assert.equal(o.kind, "query", `expected a query result for ${sql}`);
  if (o.kind !== "query") throw new Error("unreachable");
  return o.rows.map((r) => r.map(render));
}

function cost(db: Database, sql: string): bigint {
  return execute(db, sql).cost;
}

test("INSERT VALUES RETURNING: rows and the outcome variant", () => {
  const db = setup();
  // Without RETURNING an INSERT stays a bare statement outcome.
  assert.equal(execute(db, "INSERT INTO t VALUES (10, 1, 2)").kind, "statement");
  // With it, the stored rows project back — including multi-row and the `*` glob with
  // the DEFAULT fill-in (v = 7) and the omitted column (w = NULL).
  assert.deepStrictEqual(rows(db, "INSERT INTO t VALUES (11, 5, 6) RETURNING id, v"), [
    ["11", "5"],
  ]);
  assert.deepStrictEqual(rows(db, "INSERT INTO t (id) VALUES (12), (13) RETURNING *"), [
    ["12", "7", "NULL"],
    ["13", "7", "NULL"],
  ]);
});

test("RETURNING output names and expressions", () => {
  const db = setup();
  // §8 naming: ?column? for an expression, the AS label, the canonical name for a
  // bare/qualified column. Expressions evaluate against the stored row.
  const o = execute(db, "INSERT INTO t VALUES (14, 5, 0) RETURNING v + 1, v * 2 AS dbl, t.w, id");
  assert.equal(o.kind, "query");
  if (o.kind !== "query") throw new Error("unreachable");
  assert.deepStrictEqual(o.columnNames, ["?column?", "dbl", "w", "id"]);
  assert.deepStrictEqual(rows(db, "DELETE FROM t WHERE id = 14 RETURNING v * 10, abs(w - 1)"), [
    ["50", "1"],
  ]);
});

test("INSERT ... SELECT ... RETURNING", () => {
  const db = setup();
  execute(db, "CREATE TABLE src (a int32)");
  execute(db, "INSERT INTO src VALUES (40), (41)");
  // RETURNING belongs to the INSERT: it projects the INSERTED rows (defaults filled).
  assert.deepStrictEqual(rows(db, "INSERT INTO t (id) SELECT a FROM src RETURNING id, v"), [
    ["40", "7"],
    ["41", "7"],
  ]);
  // The word `returning` is never an IMPLICIT source alias (the §15 stop set) — but an
  // explicit `AS returning` alias still parses, and the clause follows it.
  assert.deepStrictEqual(
    rows(db, "INSERT INTO t (id) SELECT a + 100 FROM src AS returning RETURNING id"),
    [["140"], ["141"]],
  );
});

test("UPDATE RETURNING projects the NEW values; zero rows stay a query result", () => {
  const db = setup();
  assert.deepStrictEqual(rows(db, "UPDATE t SET v = v + 1 WHERE id <= 2 RETURNING id, v"), [
    ["1", "11"],
    ["2", "21"],
  ]);
  // Zero matched rows: still a QUERY outcome — empty rows, names intact.
  const o = execute(db, "UPDATE t SET v = 0 WHERE id = 999 RETURNING id");
  assert.equal(o.kind, "query");
  if (o.kind !== "query") throw new Error("unreachable");
  assert.deepStrictEqual(o.columnNames, ["id"]);
  assert.deepStrictEqual(o.rows, []);
});

test("DELETE RETURNING projects the OLD values", () => {
  const db = setup();
  assert.deepStrictEqual(rows(db, "DELETE FROM t WHERE w = 200 RETURNING id, v, w"), [
    ["2", "20", "200"],
  ]);
  assert.deepStrictEqual(rows(db, "SELECT id FROM t ORDER BY id"), [["1"], ["3"]]);
});

test("RETURNING error codes", () => {
  const db = setup();
  const cases: [string, string][] = [
    // Resolution precedes execution: the unknown column beats the would-be PK duplicate.
    ["INSERT INTO t VALUES (1, 0, 0) RETURNING nosuch", "42703"],
    // Aggregates are forbidden in RETURNING (PG 42803).
    ["INSERT INTO t VALUES (90, 0, 0) RETURNING sum(v)", "42803"],
    ["UPDATE t SET v = 1 RETURNING count(*)", "42803"],
    // An unknown qualifier is 42P01 — including PG 18's old/new, which jed does not
    // implement (the documented §32 divergence).
    ["INSERT INTO t VALUES (91, 0, 0) RETURNING other.v", "42P01"],
    ["UPDATE t SET v = v + 1 RETURNING old.v", "42P01"],
    ["DELETE FROM t RETURNING new.id", "42P01"],
    // An empty item list, and any trailing clause after RETURNING, are 42601.
    ["DELETE FROM t RETURNING", "42601"],
    ["DELETE FROM t WHERE id = 1 RETURNING id ORDER BY id", "42601"],
    // `returning` is no longer an implicit alias ANYWHERE (the §15 stop set): in a plain
    // SELECT it is now trailing junk, as in PostgreSQL (which reserves the word).
    ["SELECT v FROM t returning", "42601"],
  ];
  for (const [sql, code] of cases) {
    assert.equal(errCode(() => execute(db, sql)), code, sql);
  }
  // Nothing above wrote anything.
  assert.deepStrictEqual(rows(db, "SELECT count(*) FROM t"), [["3"]]);
});

test("RETURNING subqueries observe the pre-statement snapshot", () => {
  const db = setup();
  // Uncorrelated subqueries fold once and read the PRE-statement snapshot (probed
  // against PG 18): the count excludes the two rows being inserted...
  assert.deepStrictEqual(
    rows(db, "INSERT INTO t VALUES (50, 0, 0), (51, 0, 0) RETURNING id, (SELECT count(*) FROM t)"),
    [
      ["50", "3"],
      ["51", "3"],
    ],
  );
  // ... an UPDATE's subquery sees pre-update values (sum over old v: 10+20+30) ...
  assert.deepStrictEqual(
    rows(db, "UPDATE t SET v = 0 WHERE id = 1 RETURNING (SELECT sum(v) FROM t WHERE w IS NOT NULL)"),
    [["60"]],
  );
  // ... and a DELETE's sees the row still present (5 rows live at this point).
  assert.deepStrictEqual(
    rows(db, "DELETE FROM t WHERE id = 1 RETURNING (SELECT count(*) FROM t WHERE w IS NOT NULL)"),
    [["5"]],
  );
  // A correlated subquery's outer reference reads the row being RETURNED (here the
  // deleted row: its neighbor id+1 = 3 has w = 300).
  assert.deepStrictEqual(
    rows(db, "DELETE FROM t WHERE id = 2 RETURNING (SELECT s.w FROM t AS s WHERE s.id = t.id + 1)"),
    [["300"]],
  );
});

test("RETURNING costs", () => {
  const db = setup();
  // A plain VALUES insert still costs zero; RETURNING adds row_produced per stored row
  // plus the items' metered evaluation (bare columns are leaves).
  assert.equal(cost(db, "INSERT INTO t VALUES (60, 1, 1)"), 0n);
  assert.equal(cost(db, "INSERT INTO t VALUES (61, 1, 1) RETURNING id, v"), 1n);
  // 2 x (row_produced + one operator_eval)
  assert.equal(cost(db, "INSERT INTO t VALUES (62, 1, 1), (63, 2, 2) RETURNING v + 1"), 4n);
  // UPDATE/DELETE under a PK point bound: page_read(1) + storage_row_read(1) + the
  // residual filter eval(1), plus the projection (row_produced 1, leaves 0).
  assert.equal(cost(db, "UPDATE t SET v = 9 WHERE id = 1"), 3n);
  assert.equal(cost(db, "UPDATE t SET v = 8 WHERE id = 1 RETURNING v"), 4n);
  assert.equal(cost(db, "DELETE FROM t WHERE id = 60 RETURNING v"), 4n);
});

test("RETURNING subquery costs: fold once vs correlated per returned row", () => {
  // Fresh 3-row table: an uncorrelated RETURNING subquery folds ONCE.
  // Inner `SELECT max(v) FROM t`: page_read 1 + 3 row reads + 3 accumulates +
  // 1 row_produced = 8. Two returned rows add 2 x row_produced (the folded constant is
  // a leaf): total 10.
  let db = setup();
  assert.equal(
    cost(db, "INSERT INTO t VALUES (64, 1, 1), (65, 1, 2) RETURNING (SELECT max(v) FROM t)"),
    10n,
  );
  // A correlated one re-runs per RETURNED row: outer DELETE bound = page 1 + row 1 +
  // filter 1 + row_produced 1; the subquery node charges operator_eval 1 + the inner
  // bounded count (page 1 + row 1 + filter 1 + accumulate 1 + row_produced 1 = 5).
  db = setup();
  assert.equal(
    cost(db, "DELETE FROM t WHERE id = 1 RETURNING (SELECT count(*) FROM t AS s WHERE s.id = t.id)"),
    10n,
  );
});

test("a ceiling abort during RETURNING is all-or-nothing", () => {
  const db = setup();
  // The two-row insert with RETURNING costs 4 (pinned above). A ceiling of 2 aborts
  // during the projection pass — BEFORE phase 2 — so nothing is inserted.
  db.setMaxCost(2n);
  assert.equal(
    errCode(() => execute(db, "INSERT INTO t VALUES (70, 1, 1), (71, 2, 2) RETURNING v + 1")),
    "54P01",
  );
  db.setMaxCost(0n);
  assert.deepStrictEqual(rows(db, "SELECT count(*) FROM t"), [["3"]]);
});

test("$N binds in the RETURNING list", () => {
  const db = setup();
  // A $N in the RETURNING list types from context like anywhere else (api.md §5).
  const o = executeParams(db, "INSERT INTO t VALUES (80, 3, 0) RETURNING v + $1", [intValue(5n)]);
  assert.equal(o.kind, "query");
  if (o.kind !== "query") throw new Error("unreachable");
  assert.deepStrictEqual(o.rows.map((r) => r.map(render)), [["8"]]);
  // A parameter no context types is 42P18.
  assert.equal(
    errCode(() => executeParams(db, "INSERT INTO t VALUES (81, 3, 0) RETURNING $1", [intValue(5n)])),
    "42P18",
  );
});

test("RETURNING grows the touched set", () => {
  // A compressed large value charges value_decompress only when RETURNING reads it
  // (the §32 touched-set rule). 100_000 raw bytes at page_size 8192 (C = 8180):
  // ceil(100000/8180) = 13 slabs.
  const big = `INSERT INTO big VALUES (1, 0, '${"x".repeat(100_000)}')`;
  const fresh = () =>
    dbWith(["CREATE TABLE big (id int32 PRIMARY KEY, w int32, t text)", big]);
  // RETURNING only fixed-width columns: no decompression (page 1 + row 1 + filter 1 +
  // row_produced 1).
  assert.equal(cost(fresh(), "DELETE FROM big WHERE id = 1 RETURNING id, w"), 4n);
  // RETURNING the compressed column adds its 13 slabs.
  assert.equal(cost(fresh(), "DELETE FROM big WHERE id = 1 RETURNING t"), 17n);
  // UPDATE: an ASSIGNED column's returned value is the freshly computed one — not a
  // storage read, so no decompression (and the shrunken row re-stores inline-plain: no
  // compression attempt either).
  assert.equal(cost(fresh(), "UPDATE big SET t = 'short' WHERE id = 1 RETURNING t"), 4n);
  // RETURNING an UNASSIGNED compressed column is a logical read: the rewrite's own
  // 13 value_compress attempts (the over-RECORD_MAX row re-stores) + the projection's
  // 13 value_decompress + row_produced, over the 3-unit bounded scan.
  assert.equal(cost(fresh(), "UPDATE big SET w = 1 WHERE id = 1"), 16n);
  assert.equal(cost(fresh(), "UPDATE big SET w = 1 WHERE id = 1 RETURNING t"), 30n);
});

test("RETURNING in transactions", () => {
  const db = setup();
  execute(db, "BEGIN");
  assert.deepStrictEqual(rows(db, "INSERT INTO t VALUES (95, 1, 1) RETURNING id"), [["95"]]);
  execute(db, "ROLLBACK");
  assert.deepStrictEqual(rows(db, "SELECT count(*) FROM t"), [["3"]]);
  // A write statement stays a write statement: 25006 in a READ ONLY block.
  execute(db, "BEGIN READ ONLY");
  assert.equal(errCode(() => execute(db, "DELETE FROM t WHERE id = 1 RETURNING id")), "25006");
  execute(db, "ROLLBACK");
});
