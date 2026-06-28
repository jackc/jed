// S3 session privileges — the host-API surface (spec/design/session.md §5.3). The SQL-observable
// 42501 behavior (every table/function/DDL gate) is corpus-tested across all three cores
// (suites/session/privileges.test); these per-core tests cover what the single-statement corpus
// cannot CALL: configuring the envelope through the TS host API directly, the value-level
// Privilege/PrivilegeSet surface, the per-session independence of an additional session, and the
// introspection accessors (CLAUDE.md §10). Mirrors impl/rust/tests/privileges.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Engine, EngineError, execute, PrivilegeSet } from "../src/lib.ts";

function code(fn: () => unknown): string {
  try {
    fn();
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  throw new Error("expected an error, got none");
}

test("default session is fully permissive", () => {
  const db = new Engine();
  assert.equal(db.allowDdl(), true);
  assert.equal(db.privileges().isPermissive(), true);
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  execute(db, "INSERT INTO t VALUES (1, 10)");
  execute(db, "UPDATE t SET v = 20 WHERE id = 1");
  execute(db, "DELETE FROM t WHERE id = 1");
});

test("setDefaultPrivileges makes a read-only session", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  execute(db, "INSERT INTO t VALUES (1, 10)");
  db.setDefaultPrivileges(PrivilegeSet.empty().with("select"));
  execute(db, "SELECT v FROM t WHERE id = 1");
  assert.equal(
    code(() => execute(db, "INSERT INTO t VALUES (2, 20)")),
    "42501",
  );
  assert.equal(
    code(() => execute(db, "UPDATE t SET v = 0 WHERE id = 1")),
    "42501",
  );
  assert.equal(
    code(() => execute(db, "DELETE FROM t WHERE id = 1")),
    "42501",
  );
});

test("grant adds and revoke wins", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");

  db.setDefaultPrivileges(PrivilegeSet.empty());
  db.grant(PrivilegeSet.empty().with("insert"), "t");
  execute(db, "INSERT INTO t VALUES (1, 10)"); // bare INSERT needs only INSERT

  // Revoking what was granted denies it (deny wins regardless of the grant).
  db.revoke(PrivilegeSet.empty().with("insert"), "t");
  assert.equal(
    code(() => execute(db, "INSERT INTO t VALUES (2, 20)")),
    "42501",
  );
  assert.equal(db.privileges().allowsTable("t", "insert"), false);
});

test("allow_ddl gate is independent of table privileges", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  db.setAllowDdl(false);
  assert.equal(
    code(() => execute(db, "CREATE TABLE u (id i32 PRIMARY KEY)")),
    "42501",
  );
  assert.equal(
    code(() => execute(db, "DROP TABLE t")),
    "42501",
  );
  execute(db, "INSERT INTO t VALUES (1, 10)"); // DML untouched
});

test("function EXECUTE is revocable", () => {
  const db = new Engine();
  assert.equal(db.privileges().allowsFunction("abs"), true);
  execute(db, "SELECT abs(-5)");
  db.revoke(PrivilegeSet.empty().with("execute"), "abs");
  assert.equal(db.privileges().allowsFunction("abs"), false);
  assert.equal(
    code(() => execute(db, "SELECT abs(-5)")),
    "42501",
  );
  execute(db, "SELECT 1 + 2"); // the + operator is not a named function — never gated
});

test("an additional session carries its own envelope", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");

  const restricted = db.newSession({ defaultPrivileges: PrivilegeSet.empty().with("select") });
  restricted.execute(db, "SELECT * FROM t"); // read allowed
  assert.equal(
    code(() => restricted.execute(db, "INSERT INTO t VALUES (1, 10)")),
    "42501",
  );

  // The default session is unaffected — it still writes.
  execute(db, "INSERT INTO t VALUES (1, 10)");

  // A grant on the additional session lifts the restriction for it alone.
  restricted.grant(PrivilegeSet.empty().with("insert"), "t");
  restricted.execute(db, "INSERT INTO t VALUES (2, 20)");
});

test("a missing object is 42P01, not authorization", () => {
  const db = new Engine();
  db.setDefaultPrivileges(PrivilegeSet.empty());
  assert.equal(
    code(() => execute(db, "SELECT * FROM does_not_exist")),
    "42P01",
  );
});
