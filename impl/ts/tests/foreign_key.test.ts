// FOREIGN KEY constraints — `[CONSTRAINT name] FOREIGN KEY (cols) REFERENCES …` and the
// column-level `REFERENCES` (spec/design/constraints.md §6, grammar.md §43). Covers what the
// oracle corpus (ddl/foreign_key.test) cannot: the jed-specific divergences from PostgreSQL
// (strict same-type pairing, the deferred referential actions, the end-state parent UPDATE), and
// catalog introspection (constraint names). The agreeing behavior — the 23503 enforcement at every
// write site, MATCH SIMPLE, the batch end state, 42830/2BP01 — is the corpus's job. Mirrors
// impl/rust/tests/foreign_key.rs and impl/go/foreign_key_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import { type Handle, dbWith, errCode } from "./util.ts";

function fkNames(db: Handle, table: string): string[] {
  return db.table(table)!.fks.map((f) => f.name);
}

// Auto-naming follows PostgreSQL's <table>_<localcols>_fkey; an explicit CONSTRAINT name is used as
// written; the catalog holds FKs in ascending lowercased-name order.
test("FK naming and catalog order", () => {
  const db = dbWith([
    "CREATE TABLE p (a i32, b i32, code i32 UNIQUE, PRIMARY KEY (a, b))",
    "CREATE TABLE c (id i32 PRIMARY KEY, pa i32, pb i32, pcode i32, " +
      "CONSTRAINT c_code_fk FOREIGN KEY (pcode) REFERENCES p (code), " +
      "FOREIGN KEY (pa, pb) REFERENCES p (a, b))",
  ]);
  assert.deepEqual(fkNames(db, "c"), ["c_code_fk", "c_pa_pb_fkey"]);

  const db2 = dbWith([
    "CREATE TABLE q (id i32 PRIMARY KEY)",
    "CREATE TABLE r (id i32 PRIMARY KEY, x i32 REFERENCES q, FOREIGN KEY (x) REFERENCES q (id))",
  ]);
  assert.deepEqual(fkNames(db2, "r"), ["r_x_fkey", "r_x_fkey1"]);
});

// jed is STRICTER than PostgreSQL on type pairing: corresponding columns must be the SAME scalar
// type (42804), where PG allows any comparable pair (e.g. i32 ↔ i64) — constraints.md §6.7.
test("FK strict same-type pairing (42804)", () => {
  const db = dbWith(["CREATE TABLE p (id i32 PRIMARY KEY)"]);
  assert.equal(
    errCode(() => db.execute("CREATE TABLE c1 (x i64 REFERENCES p)")),
    "42804",
  );
  assert.equal(
    errCode(() => db.execute("CREATE TABLE c2 (x text REFERENCES p)")),
    "42804",
  );
  db.execute("CREATE TABLE c3 (x i32 REFERENCES p)"); // same type — accepted
});

// CASCADE / SET NULL / SET DEFAULT parse but are rejected at CREATE TABLE (0A000); NO ACTION and
// RESTRICT are accepted (constraints.md §6.6).
test("FK referential actions narrowed (0A000)", () => {
  const db = dbWith(["CREATE TABLE p (id i32 PRIMARY KEY)"]);
  assert.equal(
    errCode(() => db.execute("CREATE TABLE c1 (x i32 REFERENCES p ON DELETE CASCADE)")),
    "0A000",
  );
  assert.equal(
    errCode(() => db.execute("CREATE TABLE c2 (x i32 REFERENCES p ON UPDATE SET NULL)")),
    "0A000",
  );
  assert.equal(
    errCode(() => db.execute("CREATE TABLE c3 (x i32 REFERENCES p ON DELETE SET DEFAULT)")),
    "0A000",
  );
  db.execute("CREATE TABLE c4 (x i32 REFERENCES p ON DELETE NO ACTION ON UPDATE RESTRICT)");
});

// jed validates the parent side against the statement's END STATE: a swap of two referenced UNIQUE
// values keeps every referenced tuple present, so the UPDATE succeeds where PG fails on the
// transient — a documented divergence (constraints.md §6.7).
test("FK parent UPDATE end-state swap allowed", () => {
  const db = dbWith([
    "CREATE TABLE p (id i32 PRIMARY KEY, code i32 UNIQUE)",
    "INSERT INTO p VALUES (1, 100), (2, 200)",
    "CREATE TABLE c (id i32 PRIMARY KEY, pc i32 REFERENCES p (code))",
    "INSERT INTO c VALUES (10, 100), (11, 200)",
  ]);
  db.execute("UPDATE p SET code = CASE code WHEN 100 THEN 200 ELSE 100 END"); // swap — end state valid
  assert.equal(
    errCode(() => db.execute("UPDATE p SET code = 999 WHERE id = 1")),
    "23503",
  );
});
