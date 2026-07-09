// Partial-index behaviors the shared corpus cannot express (a PG divergence — jed's syntactic
// implication + timestamptz hazard; on-disk byte round-trip; catalog introspection). The PG-agreeing
// behavior (23505 among qualifying rows, error codes, planner rows) lives in the corpus
// (spec/conformance/suites/ddl/partial_index.test).

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, EngineError } from "../src/lib.ts";
import { type Handle, queryOutcome } from "./util.ts";
import { memDb } from "./mem_db.ts";

function run(db: Handle, sql: string): void {
  queryOutcome(db, sql);
}

// render one row's cells to strings (text → its content, NULL → "NULL", int → decimal).
function cells(db: Handle, sql: string): string[][] {
  const o = queryOutcome(db, sql);
  if (o.kind !== "query") throw new Error("expected a query");
  return o.rows.map((r) =>
    r.map((v) => {
      if (v.kind === "text") return v.text;
      if (v.kind === "null") return "NULL";
      if (v.kind === "int") return v.int.toString();
      return String(v.kind);
    }),
  );
}

function errCode(fn: () => void): string {
  try {
    fn();
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  throw new Error("expected an EngineError");
}

// A UNIQUE partial index constrains ONLY its qualifying rows (indexes.md §9): two active rows may
// not share amt, but an inactive row may duplicate an active one. Survives reload (v27).
test("partial unique constrains only qualifying rows and persists", () => {
  const db = memDb().session();
  run(db, "CREATE TABLE pt (id i32 PRIMARY KEY, status text, amt i32)");
  run(db, "INSERT INTO pt VALUES (1, 'active', 10)");
  run(db, "CREATE UNIQUE INDEX pt_uact ON pt (amt) WHERE status = 'active'");
  // An inactive row may duplicate the active amt=10 (it is not in the index).
  run(db, "INSERT INTO pt VALUES (2, 'inactive', 10)");
  // A second active amt=10 collides (23505 names the partial index).
  assert.equal(
    errCode(() => run(db, "INSERT INTO pt VALUES (3, 'active', 10)")),
    "23505",
  );
  // Round-trip: the v27 catalog re-parses the predicate, and it still enforces + exempts.
  const loaded = Database.fromImage(db.toImage(256, 1n));
  run(loaded, "INSERT INTO pt VALUES (4, 'inactive', 10)");
  assert.equal(
    errCode(() => run(loaded, "INSERT INTO pt VALUES (5, 'active', 10)")),
    "23505",
  );
});

// The planner uses a partial index ONLY when the WHERE contains the predicate conjunct (indexes.md
// §9) — the syntactic implication gate. EXPLAIN names it when gated, not otherwise.
test("partial planner gates on the predicate conjunct", () => {
  const db = memDb().session();
  run(db, "CREATE TABLE pt (id i32 PRIMARY KEY, status text, amt i32)");
  run(db, "INSERT INTO pt VALUES (1,'active',10),(2,'inactive',10),(3,'active',30)");
  run(db, "CREATE INDEX pt_amt_active ON pt (amt) WHERE status = 'active'");
  const plan = (sql: string): string =>
    cells(db, sql)
      .map((r) => r[2])
      .join("\n");
  const gated = plan("EXPLAIN SELECT id FROM pt WHERE status = 'active' AND amt = 10");
  assert.ok(gated.includes("pt_amt_active"), `gated plan should use the partial index:\n${gated}`);
  const ungated = plan("EXPLAIN SELECT id FROM pt WHERE amt = 10");
  assert.ok(
    !ungated.includes("pt_amt_active"),
    `ungated plan must NOT use the partial index:\n${ungated}`,
  );
  // Rows are correct either way (the residual filter re-applies the full WHERE).
  assert.deepEqual(cells(db, "SELECT id FROM pt WHERE status = 'active' AND amt = 10"), [["1"]]);
});

// A timestamptz-referencing predicate is 42P17 (the session-tz hazard, a jed divergence); a
// non-boolean predicate is 42804; a partial GIN index is 0A000.
test("partial predicate rejections", () => {
  const db = memDb().session();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, ts timestamptz, a i32, arr i32[])");
  assert.equal(
    errCode(() => run(db, "CREATE INDEX ON t (a) WHERE ts IS NULL")),
    "42P17",
  );
  assert.equal(
    errCode(() => run(db, "CREATE INDEX ON t (a) WHERE a")),
    "42804",
  );
  assert.equal(
    errCode(() => run(db, "CREATE INDEX ON t USING gin (arr) WHERE a > 0")),
    "0A000",
  );
});

// jed_indexes surfaces a partial index's predicate canonical text; NULL for a non-partial index.
test("partial index introspection shows the predicate", () => {
  const db = memDb().session();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, s text, a i32)");
  run(db, "CREATE INDEX ipart ON t (a) WHERE s = 'x'");
  run(db, "CREATE INDEX ifull ON t (a)");
  const part = cells(db, "SELECT predicate FROM jed_indexes WHERE name = 'ipart'");
  assert.equal(part.length, 1);
  assert.ok(part[0]![0]!.includes("x"), `predicate text: ${part[0]![0]}`);
  assert.deepEqual(cells(db, "SELECT predicate FROM jed_indexes WHERE name = 'ifull'"), [["NULL"]]);
});
