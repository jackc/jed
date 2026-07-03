// S5 session variables — the host-API surface (spec/design/session.md §6.1). The SQL-observable
// current_setting behavior (a set variable read back, the 42704-on-unset, missing_ok → NULL, the
// per-record reset) is corpus-tested across all three cores (suites/session/variables.test); these
// per-core tests cover what the directive-driven corpus cannot CALL or OBSERVE: the host setters and
// getter (setVar/resetVar/var), the 42704 rejection of a non-dotted name, case folding at the host
// API, NULL propagation through a text-typed NULL value, that variables are SESSION state not snapshot
// state (they do not roll back with a transaction), an additional session's independent variables, and
// resetVars (PG RESET ALL). Mirrors impl/rust/tests/variables.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { EngineError } from "../src/tooling.ts";
import type { Handle } from "./util.ts";
import type { Value } from "../src/value.ts";
import { memDb } from "./mem_db.ts";

function code(fn: () => unknown): string {
  try {
    fn();
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  throw new Error("expected an error, got none");
}

// scalar runs a single-row, single-column query and returns the lone value.
function scalar(db: Handle, sql: string): Value {
  const o = db.execute(sql);
  if (o.kind !== "query") throw new Error("expected a query result");
  assert.equal(o.rows.length, 1, sql);
  assert.equal(o.rows[0]!.length, 1, sql);
  return o.rows[0]![0]!;
}

function assertText(v: Value, want: string): void {
  assert.equal(v.kind, "text", `want text, got ${v.kind}`);
  assert.equal((v as { kind: "text"; text: string }).text, want);
}

test("host set and read round trip", () => {
  // setVar stores; var reads it back through the host API; current_setting reads it in SQL.
  const db = memDb().session();
  assert.equal(db.var("myapp.tenant"), undefined); // unset
  db.setVar("myapp.tenant", "acme");
  assert.equal(db.var("myapp.tenant"), "acme");
  assertText(scalar(db, "SELECT current_setting('myapp.tenant')"), "acme");
});

test("set and reset reject a non-dotted name", () => {
  // A variable must be namespaced (dotted) — a non-dotted name is a built-in setting name, and v1
  // exposes none through this map (the time_zone built-in is its own slice), so it is 42704.
  const db = memDb().session();
  assert.equal(
    code(() => db.setVar("bogus", "x")),
    "42704",
  );
  assert.equal(
    code(() => db.resetVar("bogus")),
    "42704",
  );
  // The host getter never throws — a non-dotted (or any unset) name simply reads as undefined.
  assert.equal(db.var("bogus"), undefined);
});

test("reset removes and is idempotent", () => {
  const db = memDb().session();
  db.setVar("myapp.k", "v");
  db.resetVar("myapp.k");
  assert.equal(db.var("myapp.k"), undefined);
  // current_setting on the now-unset name is 42704 again.
  assert.equal(
    code(() => db.execute("SELECT current_setting('myapp.k')")),
    "42704",
  );
  // Resetting an unset variable is a no-op (PG RESET of an unset custom variable).
  db.resetVar("myapp.k");
});

test("names are case-insensitive but values are verbatim", () => {
  // The NAME folds to lowercase (PG GUC names are case-insensitive); the VALUE is preserved exactly.
  const db = memDb().session();
  db.setVar("myApp.Tenant", "AcmeCorp");
  assert.equal(db.var("myapp.tenant"), "AcmeCorp");
  assert.equal(db.var("MYAPP.TENANT"), "AcmeCorp");
  assertText(scalar(db, "SELECT current_setting('MyApp.TENANT')"), "AcmeCorp");
});

test("missing_ok turns the unset error into NULL", () => {
  const db = memDb().session();
  assert.equal(
    code(() => db.execute("SELECT current_setting('myapp.unset')")),
    "42704",
  );
  assert.equal(scalar(db, "SELECT current_setting('myapp.unset', true)").kind, "null");
  // false behaves like the one-arg form.
  assert.equal(
    code(() => db.execute("SELECT current_setting('myapp.unset', false)")),
    "42704",
  );
});

test("a NULL name propagates to NULL", () => {
  // null = "propagates": a NULL name short-circuits to NULL before the lookup. A text column holding
  // a NULL is the typed-NULL the corpus cannot write (jed defers text casts, so no NULL::text yet).
  const db = memDb().session();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, n text)");
  db.execute("INSERT INTO t VALUES (1, NULL)");
  db.setVar("myapp.x", "set");
  assert.equal(scalar(db, "SELECT current_setting(n) FROM t WHERE id = 1").kind, "null");
});

test("variables are session state, not snapshot state", () => {
  // Variables are SESSION state, not snapshot state (§6.1): a ROLLBACK undoes DATA but never a
  // session variable (PG SET SESSION). Set one outside, one inside a block, roll back — both survive.
  const db = memDb().session();
  db.setVar("myapp.outer", "a");
  db.execute("BEGIN");
  db.setVar("myapp.inner", "b");
  db.execute("ROLLBACK");
  assert.equal(db.var("myapp.outer"), "a");
  assert.equal(db.var("myapp.inner"), "b");
  assertText(scalar(db, "SELECT current_setting('myapp.inner')"), "b");
});

test("an additional session has independent variables", () => {
  // db.session(opts) mints an independent session over a shared core (§2.1/§2.4): its variable map is
  // its own — a variable set on it is invisible to another session and vice versa.
  const db = memDb();
  const a = db.session({});
  a.setVar("myapp.who", "a");

  const other = db.session({});
  other.setVar("myapp.who", "other");

  const o = other.execute("SELECT current_setting('myapp.who')");
  if (o.kind !== "query") throw new Error("expected a query result");
  assertText(o.rows[0]![0]!, "other");
  assert.equal(a.var("myapp.who"), "a");
  assert.equal(other.var("myapp.who"), "other");

  // A variable only on one session is not visible to the other at all.
  other.setVar("myapp.only", "x");
  assert.equal(a.var("myapp.only"), undefined);
});

test("resetVars clears every variable", () => {
  // resetVars is PG RESET ALL for the variable map.
  const db = memDb().session();
  db.setVar("myapp.a", "1");
  db.setVar("myapp.b", "2");
  db.resetVars();
  assert.equal(db.var("myapp.a"), undefined);
  assert.equal(db.var("myapp.b"), undefined);
  assert.equal(
    code(() => db.execute("SELECT current_setting('myapp.a')")),
    "42704",
  );
});
