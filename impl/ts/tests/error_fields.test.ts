// Structured error fields (spec/design/error-fields.md) — the constraintName / tableName /
// columnName / dataTypeName diagnostics on EngineError, modeled on pgx's pgconn.PgError. Out of the
// conformance corpus's reach (it matches on code/prose, never on a structured field — CLAUDE.md
// §10), so this is the host-API surface test. Mirrors impl/rust/tests/error_fields.rs and
// impl/go/error_fields_test.go.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { EngineError, PrivilegeSet, privilegeFromName } from "../src/tooling.ts";
import { type Handle, dbWith } from "./util.ts";

// efErr runs sql on db and returns the EngineError it throws (failing if none / a non-EngineError).
function efErr(db: Handle, sql: string): EngineError {
  try {
    db.execute(sql);
  } catch (e) {
    if (e instanceof EngineError) return e;
    throw e;
  }
  throw new Error(`expected an EngineError from ${sql}`);
}

// 23505 on a PRIMARY KEY reports the derived <table>_pkey constraint + the table; the rendered
// message is unchanged (fields are additive metadata, not a message change).
test("error fields: unique violation (primary key)", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)", "INSERT INTO t VALUES (1)"]);
  const e = efErr(db, "INSERT INTO t VALUES (1)");
  assert.equal(e.code(), "23505");
  assert.equal(e.constraintName, "t_pkey");
  assert.equal(e.tableName, "t");
  assert.equal(e.columnName, undefined);
  assert.match(e.message, /duplicate key value violates unique constraint: t_pkey$/);
});

// 23505 on a named UNIQUE index reports the index (= constraint) name.
test("error fields: unique violation (secondary index)", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, email text)",
    "CREATE UNIQUE INDEX t_email_key ON t (email)",
    "INSERT INTO t VALUES (1, 'a')",
  ]);
  const e = efErr(db, "INSERT INTO t VALUES (2, 'a')");
  assert.equal(e.code(), "23505");
  assert.equal(e.constraintName, "t_email_key");
  assert.equal(e.tableName, "t");
});

// 23514 reports the CHECK constraint + the relation.
test("error fields: check violation", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, n i32 CONSTRAINT n_pos CHECK (n > 0))"]);
  const e = efErr(db, "INSERT INTO t VALUES (1, -1)");
  assert.equal(e.code(), "23514");
  assert.equal(e.constraintName, "n_pos");
  assert.equal(e.tableName, "t");
});

// 23503 (child side) reports the FK constraint + the written table.
test("error fields: foreign key violation (insert)", () => {
  const db = dbWith([
    "CREATE TABLE p (id i32 PRIMARY KEY)",
    "CREATE TABLE c (id i32 PRIMARY KEY, pid i32 CONSTRAINT c_pid_fk REFERENCES p)",
  ]);
  const e = efErr(db, "INSERT INTO c VALUES (1, 99)");
  assert.equal(e.code(), "23503");
  assert.equal(e.constraintName, "c_pid_fk");
  assert.equal(e.tableName, "c");
});

// 23503 (parent side) reports the FK constraint + the modified (parent) table.
test("error fields: foreign key violation (delete)", () => {
  const db = dbWith([
    "CREATE TABLE p (id i32 PRIMARY KEY)",
    "CREATE TABLE c (id i32 PRIMARY KEY, pid i32 CONSTRAINT c_pid_fk REFERENCES p)",
    "INSERT INTO p VALUES (1)",
    "INSERT INTO c VALUES (1, 1)",
  ]);
  const e = efErr(db, "DELETE FROM p WHERE id = 1");
  assert.equal(e.code(), "23503");
  assert.equal(e.constraintName, "c_pid_fk");
  assert.equal(e.tableName, "p");
});

// 23P01 reports the EXCLUDE constraint (its backing GiST index name) + the table.
test("error fields: exclusion violation", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, r i32range, CONSTRAINT t_r_excl EXCLUDE USING gist (r WITH &&))",
    "INSERT INTO t VALUES (1, '[1,5)')",
  ]);
  const e = efErr(db, "INSERT INTO t VALUES (2, '[3,8)')");
  assert.equal(e.code(), "23P01");
  assert.equal(e.constraintName, "t_r_excl");
  assert.equal(e.tableName, "t");
});

// 23502 reports the column (unnamed constraint, as in PostgreSQL); the table is stamped at the DML
// boundary.
test("error fields: not-null violation", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, n i32 NOT NULL)"]);
  const e = efErr(db, "INSERT INTO t VALUES (1, NULL)");
  assert.equal(e.code(), "23502");
  assert.equal(e.columnName, "n");
  assert.equal(e.tableName, "t");
  assert.equal(e.constraintName, undefined);
});

// 22003 (integer overflow on column store) reports the data type + the table.
test("error fields: numeric value out of range", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, n i16)"]);
  const e = efErr(db, "INSERT INTO t VALUES (1, 99999)");
  assert.equal(e.code(), "22003");
  assert.equal(e.dataTypeName, "i16");
  assert.equal(e.tableName, "t");
});

// 22001 (varchar length) reports the type + the column.
test("error fields: string data right truncation", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, s varchar(3))"]);
  const e = efErr(db, "INSERT INTO t VALUES (1, 'abcd')");
  assert.equal(e.code(), "22001");
  assert.equal(e.dataTypeName, "varchar(3)");
  assert.equal(e.columnName, "s");
  assert.equal(e.tableName, "t");
});

// A non-constraint error leaves every structured field unset.
test("error fields: unrelated error has no fields", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)"]);
  const e = efErr(db, "SELECT nonesuch FROM t");
  assert.equal(e.constraintName, undefined);
  assert.equal(e.tableName, undefined);
  assert.equal(e.columnName, undefined);
  assert.equal(e.dataTypeName, undefined);
  assert.equal(e.detail, undefined);
});

// DetailCase is one case of the shared Detail fixture (spec/errors/detail_cases.txt).
type DetailCase = {
  name: string;
  setup: string[];
  defaultPrivileges: string | undefined;
  grants: [string, string][];
  revokes: [string, string][];
  run: string;
  code: string;
  detail: string | undefined;
};

function parseDetailCases(text: string): DetailCase[] {
  const cases: DetailCase[] = [];
  const privObj = (rest: string): [string, string] => {
    const sp = rest.indexOf(" ");
    if (sp < 0) throw new Error(`PRIV object expected: ${rest}`);
    return [rest.slice(0, sp), rest.slice(sp + 1)];
  };
  for (const line of text.split("\n")) {
    if (line.trim() === "" || line.startsWith("#")) continue;
    if (line.startsWith("case: ")) {
      cases.push({
        name: line.slice("case: ".length),
        setup: [],
        defaultPrivileges: undefined,
        grants: [],
        revokes: [],
        run: "",
        code: "",
        detail: undefined,
      });
      continue;
    }
    const c = cases[cases.length - 1];
    if (c === undefined) throw new Error(`a directive before the first case: ${line}`);
    const sep = line.indexOf(": ");
    const key = sep < 0 ? line : line.slice(0, sep);
    const v = sep < 0 ? "" : line.slice(sep + 2);
    if (key === "setup") c.setup.push(v);
    else if (key === "default_privileges") c.defaultPrivileges = v;
    else if (key === "grant") c.grants.push(privObj(v));
    else if (key === "revoke") c.revokes.push(privObj(v));
    else if (key === "run") c.run = v;
    else if (key === "code") c.code = v;
    else if (key === "detail") c.detail = v;
    else if (line !== "no-detail") throw new Error(`unknown detail_cases directive: ${line}`);
  }
  return cases;
}

function privilegeSet(list: string): PrivilegeSet {
  let set = PrivilegeSet.empty();
  if (list === "-") return set;
  for (const name of list.split(",")) {
    const p = privilegeFromName(name);
    if (p === undefined) throw new Error(`privilege name: ${name}`);
    set = set.with(p);
  }
  return set;
}

// The Detail field (spec/design/error-fields.md §9): every case of the shared, language-neutral
// fixture — the same file the Rust and Go cores run, so the text is pinned byte-identical.
test("error fields: detail shared cases", () => {
  const path = new URL("../../../spec/errors/detail_cases.txt", import.meta.url);
  const cases = parseDetailCases(readFileSync(path, "utf8"));
  assert.ok(cases.length > 0);
  const failures: string[] = [];
  for (const c of cases) {
    const db = dbWith(c.setup);
    if (c.defaultPrivileges !== undefined)
      db.setDefaultPrivileges(privilegeSet(c.defaultPrivileges));
    for (const [p, o] of c.grants) db.grant(privilegeSet(p), o);
    for (const [p, o] of c.revokes) db.revoke(privilegeSet(p), o);
    try {
      db.execute(c.run);
      failures.push(`${c.name}: expected ${c.code} but succeeded`);
    } catch (e) {
      if (!(e instanceof EngineError)) throw e;
      if (e.code() !== c.code || e.detail !== c.detail) {
        failures.push(
          `${c.name}: got ${e.code()} ${JSON.stringify(e.detail)} (${e.message}), ` +
            `want ${c.code} ${JSON.stringify(c.detail)}`,
        );
      }
    }
  }
  assert.deepEqual(failures, []);
});

// PositionCase is one case of the shared Position/Hint fixture (spec/errors/position_cases.txt).
type PositionCase = {
  name: string;
  setup: string[];
  run: string;
  script: boolean;
  code: string;
  position: number | undefined;
  hint: string | undefined;
};

function parsePositionCases(text: string): PositionCase[] {
  const cases: PositionCase[] = [];
  for (const line of text.split("\n")) {
    if (line.trim() === "" || line.startsWith("#")) continue;
    if (line.startsWith("case: ")) {
      cases.push({
        name: line.slice("case: ".length),
        setup: [],
        run: "",
        script: false,
        code: "",
        position: undefined,
        hint: undefined,
      });
      continue;
    }
    const c = cases[cases.length - 1];
    if (c === undefined) throw new Error(`a directive before the first case: ${line}`);
    const sep = line.indexOf(": ");
    const key = sep < 0 ? line : line.slice(0, sep);
    const v = sep < 0 ? "" : line.slice(sep + 2);
    if (key === "setup") c.setup.push(v);
    else if (key === "run") c.run = v;
    else if (key === "script") {
      c.run = v;
      c.script = true;
    } else if (key === "code") c.code = v;
    else if (key === "position") c.position = Number(v);
    else if (key === "hint") c.hint = v;
    else if (line !== "no-position") throw new Error(`unknown position_cases directive: ${line}`);
  }
  return cases;
}

// The Position and Hint fields (spec/design/error-fields.md §10, §11): every case of the shared,
// language-neutral fixture — the same file the Rust and Go cores run.
test("error fields: position and hint shared cases", () => {
  const path = new URL("../../../spec/errors/position_cases.txt", import.meta.url);
  const cases = parsePositionCases(readFileSync(path, "utf8"));
  assert.ok(cases.length > 0);
  const failures: string[] = [];
  for (const c of cases) {
    const db = dbWith(c.setup);
    try {
      if (c.script) db.executeScript(c.run);
      else db.execute(c.run);
      failures.push(`${c.name}: expected ${c.code} but succeeded`);
    } catch (e) {
      if (!(e instanceof EngineError)) throw e;
      if (e.code() !== c.code || e.position !== c.position || e.hint !== c.hint) {
        failures.push(
          `${c.name}: got ${e.code()} ${e.position} ${JSON.stringify(e.hint)} (${e.message}), ` +
            `want ${c.code} ${c.position} ${JSON.stringify(c.hint)}`,
        );
      }
    }
  }
  assert.deepEqual(failures, []);
});
