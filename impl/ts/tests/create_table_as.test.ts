// CREATE TABLE … AS (spec/design/create-table-as.md): the SQL-visible behavior is the corpus's
// (ddl/create_table_as*.test); this covers the host surface it cannot drive — the query's bound
// parameters (§1), the affected-row count, and the column-definition form's refusal of parameters.
// Mirrors the Rust create_table_as_binds_parameters test and the Go create_table_as_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import { EngineError, intValue, queryOutcome, type Session } from "../src/tooling.ts";
import { render, textValue } from "../src/value.ts";
import { memDb } from "./mem_db.ts";

function codeOf(fn: () => unknown): string {
  try {
    fn();
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  throw new Error("expected an error");
}

function rows(sess: Session, sql: string): string[][] {
  const o = queryOutcome(sess, sql);
  if (o.kind !== "query") throw new Error("expected a query");
  return o.rows.map((r) => r.map(render));
}

test("CREATE TABLE … AS binds parameters", () => {
  const db = memDb();
  const sess = db.session();
  sess.execute("CREATE TABLE src (id i32 PRIMARY KEY, name text)");
  sess.execute("INSERT INTO src VALUES (1, 'a'), (2, 'b'), (3, 'c')");
  const out = queryOutcome(
    sess,
    "CREATE TABLE t AS SELECT id, name, $2::text AS tag FROM src WHERE id >= $1",
    [intValue(2n), textValue("x")],
  );
  assert.equal(out.kind, "statement");
  if (out.kind === "statement") assert.equal(out.rowsAffected, 2);
  assert.deepEqual(rows(sess, "SELECT id, name, tag FROM t ORDER BY id"), [
    ["2", "b", "x"],
    ["3", "c", "x"],
  ]);
  // A missing parameter is the query's own count mismatch, and nothing is created.
  assert.equal(
    codeOf(() => queryOutcome(sess, "CREATE TABLE u AS SELECT $1::i32 AS a")),
    "42601",
  );
  assert.equal(
    codeOf(() => queryOutcome(sess, "SELECT a FROM u")),
    "42P01",
  );
  // WITH NO DATA binds the parameters but runs nothing.
  const nodata = queryOutcome(sess, "CREATE TABLE v AS SELECT $1::i32 AS a WITH NO DATA", [
    intValue(1n),
  ]);
  assert.equal(nodata.kind, "statement");
  if (nodata.kind === "statement") assert.equal(nodata.rowsAffected, null);
  assert.deepEqual(rows(sess, "SELECT a FROM v"), []);
  // The column-definition form is plain DDL: it takes no parameters.
  assert.equal(
    codeOf(() => queryOutcome(sess, "CREATE TABLE w (a i32)", [intValue(1n)])),
    "42601",
  );
  db.close();
});
