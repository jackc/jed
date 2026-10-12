// Views through the host API (spec/design/views.md). The corpus pins view semantics through the
// materialized execute path; these per-core tests pin only what it cannot express (CLAUDE.md §10): a
// prepared statement over a view never serves a stale plan, the lazy query() lanes classify a read of
// a nextval view as a write, function EXECUTE is checked against the reader through a view body,
// views survive a file commit + reopen + compaction, and the dependency-order tooling accessor.
// Mirrors impl/rust/tests/views.rs.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import {
  createDatabase,
  EngineError,
  openDatabase,
  PrivilegeSet,
  type Session,
  type Value,
} from "../src/lib.ts";
import { memDb } from "./mem_db.ts";

function rows(s: Session, sql: string): Value[][] {
  return [...s.query(sql)];
}

function code(fn: () => unknown): string {
  try {
    fn();
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  throw new Error("expected an error, got none");
}

const int = (n: bigint): Value => ({ kind: "int", int: n });
const text = (s: string): Value => ({ kind: "text", text: s });

// A prepared statement over a view re-plans after the view is dropped and recreated with another
// definition, and after its underlying table changes (views.md §4: a view is never cached as a stale
// plan — the view expands at every planning).
test("prepared view query never serves a stale plan", () => {
  const s = memDb().session();
  s.execute("CREATE TABLE t (a i32 PRIMARY KEY, b i32)");
  s.execute("INSERT INTO t VALUES (1, 10), (2, 20)");
  s.execute("CREATE VIEW v AS SELECT a, b FROM t WHERE a = 1");
  const stmt = s.prepare("SELECT b FROM v");
  assert.deepStrictEqual(stmt.all(), [{ b: 10n }]);
  assert.deepStrictEqual(stmt.all(), [{ b: 10n }]);

  s.execute("DROP VIEW v");
  s.execute("CREATE VIEW v AS SELECT a, b FROM t WHERE a = 2");
  assert.deepStrictEqual(stmt.all(), [{ b: 20n }]);

  s.execute("UPDATE t SET b = 21 WHERE a = 2");
  assert.deepStrictEqual(stmt.all(), [{ b: 21n }]);

  s.execute("ALTER TABLE t RENAME COLUMN b TO bb");
  assert.deepStrictEqual(stmt.all(), [{ b: 21n }]);

  s.execute("DROP VIEW v");
  assert.equal(
    code(() => stmt.all()),
    "42P01",
  );
});

// The lazy query() lanes classify a read of a view whose body calls nextval as a write (views.md
// §4.1, sequences.md §4): inside a READ ONLY block it is 25006, and in autocommit it advances the
// sequence exactly once per read.
test("query lane sees a nextval view as a write", () => {
  const s = memDb().session();
  s.execute("CREATE SEQUENCE s");
  s.execute("CREATE VIEW vs AS SELECT nextval('s') AS n");
  assert.deepStrictEqual(rows(s, "SELECT n FROM vs"), [[int(1n)]]);
  s.execute("CREATE VIEW vvs AS SELECT n FROM vs");
  assert.deepStrictEqual(rows(s, "SELECT n FROM vvs"), [[int(2n)]]);

  s.execute("BEGIN READ ONLY");
  assert.equal(
    code(() => rows(s, "SELECT n FROM vvs")),
    "25006",
  );
  s.execute("ROLLBACK");
  // A CTE of the same name shadows the view, so this read is not a write.
  s.execute("BEGIN READ ONLY");
  assert.deepStrictEqual(rows(s, "WITH vs AS (SELECT 7 AS n) SELECT n FROM vs"), [[int(7n)]]);
  s.execute("ROLLBACK");
});

// Reading a view needs SELECT on the view only, but the functions its body calls need the reader's
// EXECUTE, as in a derived table (views.md §9).
test("view body functions need the reader's EXECUTE", () => {
  const s = memDb().session();
  s.execute("CREATE TABLE t (a i32 PRIMARY KEY)");
  s.execute("INSERT INTO t VALUES (-5)");
  s.execute("CREATE VIEW v AS SELECT abs(a) AS x FROM t");
  s.execute("CREATE VIEW w AS SELECT x FROM v");
  assert.deepStrictEqual(rows(s, "SELECT x FROM w"), [[int(5n)]]);
  s.revoke(PrivilegeSet.empty().with("execute"), "abs");
  assert.equal(
    code(() => rows(s, "SELECT x FROM w")),
    "42501",
  );
  assert.equal(
    code(() => rows(s, "SELECT x FROM v")),
    "42501",
  );
});

// Views persist through a file commit and reopen, keep their stored text, and still read; a rename
// rewrite made in one session is what the next open sees (views.md §3/§7). Compaction streams the
// from-scratch image, view entries included (api.md §2.6).
test("views survive commit, reopen, and compaction", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-views-"));
  try {
    const path = join(dir, "views.jed");
    {
      const db = createDatabase({ path, skipFsync: true });
      const s = db.session();
      s.execute("CREATE TABLE t (a i32 PRIMARY KEY, b text)");
      s.execute("INSERT INTO t VALUES (1, 'x'), (2, 'y')");
      s.execute("CREATE VIEW v AS SELECT * FROM t WHERE a > 1");
      s.execute("CREATE VIEW w (k) AS SELECT a FROM v");
      s.execute("ALTER TABLE t RENAME TO t2");
      s.close();
      db.close();
    }
    let db = openDatabase(path, { skipFsync: true });
    let s = db.session();
    assert.deepStrictEqual(rows(s, "SELECT name, definition FROM jed_views ORDER BY name"), [
      [text("v"), text('select "t" . "a" , "t" . "b" from "t2" as "t" where a > 1')],
      [text("w"), text("select a from v")],
    ]);
    assert.deepStrictEqual(rows(s, "SELECT k FROM w"), [[int(2n)]]);
    assert.equal(
      code(() => s.execute("DROP TABLE t2")),
      "2BP01",
    );
    s.close();
    db.close();
    // Compact on a fresh open (no reader pinned), then reopen the compacted file.
    db = openDatabase(path, { skipFsync: true });
    db.compact("main");
    db.close();
    db = openDatabase(path, { skipFsync: true });
    s = db.session();
    assert.deepStrictEqual(rows(s, "SELECT k FROM w"), [[int(2n)]]);
    s.close();
    db.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// The dependency-order tooling accessor lists views in dependency order, ties in name byte order
// (views.md §11).
test("views in dependency order", () => {
  const s = memDb().session();
  s.execute("CREATE TABLE t (a i32 PRIMARY KEY)");
  s.execute("CREATE VIEW z AS SELECT a FROM t");
  s.execute("CREATE VIEW b AS SELECT a FROM z");
  s.execute("CREATE VIEW a AS SELECT a FROM b");
  s.execute("CREATE VIEW m AS SELECT a FROM t");
  assert.deepStrictEqual(
    s.viewsInDependencyOrder().map((v) => v.name),
    ["m", "z", "b", "a"],
  );
});
