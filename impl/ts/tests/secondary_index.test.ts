// Secondary indexes (spec/design/indexes.md) — covers what the corpus suite
// (ddl/create_index.test, query/index_scan.test) cannot: catalog introspection (index
// definitions, name order), the v5 on-disk round-trip with index trees, the file-backed
// paged-open + incremental-commit path, and transactional DDL. Mirrored in
// impl/rust/tests/secondary_index.rs and impl/go/secondary_index_test.go.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import {
  Database,
  EngineError,
  type Session,
  createDatabase,
  openDatabase,
} from "../src/tooling.ts";
import { type Handle, queryOutcome } from "./util.ts";
import { indexColumnOrdinals, pkIndices } from "../src/catalog.ts";
import { memDb } from "./mem_db.ts";

function run(db: Handle, sql: string) {
  return queryOutcome(db, sql);
}

function cost(db: Handle, sql: string): bigint {
  return run(db, sql).cost;
}

function ids(db: Handle, sql: string): bigint[] {
  const o = run(db, sql);
  assert.equal(o.kind, "query", sql);
  return (o.kind === "query" ? o.rows : []).map((r) => {
    assert.equal(r[0]!.kind, "int");
    return r[0]!.kind === "int" ? r[0]!.int : 0n;
  });
}

function errCode(fn: () => unknown): string {
  try {
    fn();
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  throw new Error("expected an error");
}

// The 20-row fixture the planner/cost tests run against: v = i % 5 gives 4 rows per
// value, so an equality admits 4 of 20.
function db20(): Session {
  const db = memDb().session();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32, w i32)");
  for (let i = 1; i <= 20; i++) {
    run(db, `INSERT INTO t VALUES (${i}, ${i % 5}, ${i})`);
  }
  return db;
}

test("auto-naming matches PostgreSQL", () => {
  // Lowercased <table>_<cols>_idx + the smallest free suffix (oracle-probed, indexes.md
  // §2); duplicates in the column list are allowed and named through; an explicit name
  // round-trips as written. The catalog holds indexes in ascending lowercased-name order.
  const db = memDb().session();
  run(db, "CREATE TABLE T (A i32 PRIMARY KEY, B i32)");
  run(db, "CREATE INDEX ON T (B)"); // t_b_idx
  run(db, "CREATE INDEX ON T (B)"); // t_b_idx1
  run(db, "CREATE INDEX ON T (B)"); // t_b_idx2
  run(db, "CREATE INDEX ON T (A, B)"); // t_a_b_idx
  run(db, "CREATE INDEX ON T (B, B)"); // t_b_b_idx (duplicate column allowed — PG)
  run(db, "CREATE INDEX Mine ON T (B)");
  const t = db.table("t")!;
  assert.deepEqual(
    t.indexes.map((i) => i.name),
    ["Mine", "t_a_b_idx", "t_b_b_idx", "t_b_idx", "t_b_idx1", "t_b_idx2"],
  );
  assert.deepEqual(indexColumnOrdinals(t.indexes[1]!), [0, 1]);
  assert.deepEqual(indexColumnOrdinals(t.indexes[2]!), [1, 1]);
  assert.deepEqual(pkIndices(t), [0]);
});

test("DDL errors match PostgreSQL", () => {
  // Validation order is table → columns (list order) → name collision (oracle-probed,
  // indexes.md §2); the relation namespace is shared with tables; DROP mismatches are
  // 42704/42809.
  const db = memDb().session();
  run(db, "CREATE TABLE t (a i32 PRIMARY KEY, s f64)");
  assert.equal(
    errCode(() => run(db, "CREATE INDEX i ON nosuch (nope)")),
    "42P01",
  );
  run(db, "CREATE INDEX taken ON t (a)");
  assert.equal(
    errCode(() => run(db, "CREATE INDEX taken ON t (nope)")),
    "42703",
  );
  // f64 IS now a valid index column (the float-order-preserving key, encoding.md §2.8 — every
  // scalar is keyable; text/bytea covered in ddl/create_index.test).
  run(db, "CREATE INDEX i ON t (s)");
  assert.equal(
    errCode(() => run(db, "CREATE INDEX taken ON t (a)")),
    "42P07",
  );
  assert.equal(
    errCode(() => run(db, "CREATE INDEX t ON t (a)")),
    "42P07",
  );
  assert.equal(
    errCode(() => run(db, "CREATE TABLE taken (x i32)")),
    "42P07",
  );
  assert.equal(
    errCode(() => run(db, "DROP INDEX nosuch")),
    "42704",
  );
  assert.equal(
    errCode(() => run(db, "DROP INDEX t")),
    "42809",
  );
  assert.equal(
    errCode(() => run(db, "DROP TABLE taken")),
    "42809",
  );
  run(db, "DROP INDEX taken");
  assert.equal(
    errCode(() => run(db, "DROP INDEX taken")),
    "42704",
  );
  run(db, "CREATE INDEX taken ON t (a)");
  run(db, "DROP TABLE t");
  run(db, "CREATE TABLE taken (x i32)"); // DROP TABLE freed its index names
  // The lookahead keeps every word non-reserved (grammar.md §30): the unnamed form over
  // a table named `on`, and an index explicitly named `on`.
  run(db, "CREATE TABLE on (x i32)");
  run(db, "CREATE INDEX ON on (x)");
  assert.equal(db.table("on")!.indexes[0]!.name, "on_x_idx");
  run(db, "DROP TABLE on"); // free the name `on` in the relation namespace
  run(db, "CREATE TABLE q (x i32)");
  run(db, "CREATE INDEX on ON q (x)");
  assert.equal(db.table("q")!.indexes[0]!.name, "on");
  run(db, "DROP INDEX on");
});

test("planner costs are pinned", () => {
  // The planner picks the index for a first-column equality and the cost drops to the
  // index-bounded form (cost.md §3 "index-bounded scan"); a provably-empty bound reads
  // nothing; the PK bound wins over an index; the lowest-named index breaks ties.
  const db = db20();
  assert.equal(cost(db, "SELECT id FROM t WHERE v = 3"), 45n); // full scan
  assert.equal(cost(db, "CREATE INDEX t_v_idx ON t (v)"), 21n);
  assert.equal(cost(db, "SELECT id FROM t WHERE v = 3"), 17n); // index-bounded
  assert.deepEqual(ids(db, "SELECT id FROM t WHERE v = 3 ORDER BY id"), [3n, 8n, 13n, 18n]);
  assert.equal(cost(db, "SELECT id FROM t WHERE v = NULL"), 0n); // 3VL-empty
  assert.equal(cost(db, "SELECT id FROM t WHERE v = 1 AND v = 2"), 0n); // contradiction
  assert.equal(cost(db, "SELECT id FROM t WHERE id = 7 AND v = 2"), 6n); // PK bound wins
  run(db, "CREATE INDEX two ON t (w, v)");
  assert.equal(cost(db, "SELECT id FROM t WHERE v = 3"), 17n); // t_v_idx still serves v
  run(db, "DROP INDEX t_v_idx");
  assert.equal(cost(db, "SELECT id FROM t WHERE v = 3"), 45n); // non-first column: full
  // First-column equality on the composite index works (the entry's tail component is
  // skipped to reach the row key); the lowest lowercased name wins a tie.
  assert.equal(cost(db, "SELECT id FROM t WHERE w = 7"), 5n);
  run(db, "CREATE INDEX a_first ON t (w)");
  assert.equal(cost(db, "SELECT id FROM t WHERE w = 7"), 5n);
  run(db, "DROP INDEX a_first");
  run(db, "DROP INDEX two");
  assert.equal(cost(db, "SELECT id FROM t WHERE w = 7"), 42n); // full scan again
});

test("round-trips through the on-disk image", () => {
  // The v5 image round-trips: index trees (including a NULL entry), the out-of-order PK
  // list, and a second-generation serialize are byte-stable; a reloaded database still
  // uses (and maintains) its indexes.
  const db = db20();
  run(db, "CREATE INDEX t_v_idx ON t (v)");
  run(db, "INSERT INTO t VALUES (100, NULL, 0)");
  const img = db.toImage(8192, 1n);
  const loaded = Database.fromImage(img);
  assert.deepEqual(loaded.toImage(8192, 1n), img, "byte-stable reload");
  const t = loaded.table("t")!;
  assert.equal(t.indexes.length, 1);
  assert.equal(t.indexes[0]!.name, "t_v_idx");
  assert.deepEqual(indexColumnOrdinals(t.indexes[0]!), [1]);
  assert.equal(cost(loaded, "SELECT id FROM t WHERE v = 3"), 17n);
  run(loaded, "UPDATE t SET v = 3 WHERE id = 100");
  assert.deepEqual(ids(loaded, "SELECT id FROM t WHERE v = 3 ORDER BY id"), [
    3n,
    8n,
    13n,
    18n,
    100n,
  ]);
});

test("index DDL is transactional", () => {
  // A CREATE INDEX inside a rolled-back block vanishes (definition and store), and one
  // inside a committed block persists (transactions.md §4.5).
  const db = db20();
  run(db, "BEGIN");
  run(db, "CREATE INDEX t_v_idx ON t (v)");
  assert.equal(cost(db, "SELECT id FROM t WHERE v = 3"), 17n);
  run(db, "ROLLBACK");
  assert.equal(db.table("t")!.indexes.length, 0);
  assert.equal(cost(db, "SELECT id FROM t WHERE v = 3"), 45n);
  run(db, "BEGIN");
  run(db, "CREATE INDEX t_v_idx ON t (v)");
  run(db, "COMMIT");
  assert.equal(cost(db, "SELECT id FROM t WHERE v = 3"), 17n);
});

test("file-backed paged reopen uses the index", () => {
  // An index survives the incremental commit + demand-paged reopen (format.md
  // "Allocation & incremental commit"; pager.md), keeps the same pinned scan cost
  // (page_read is logical — buffer-pool-invisible), and stays maintainable.
  const dir = mkdtempSync(join(tmpdir(), "jed-"));
  const path = join(dir, "secondary_index_paged.jed");
  const db = createDatabase({ path, pageSize: 256, skipFsync: true });
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32, w i32)");
  for (let i = 1; i <= 20; i++) {
    run(db, `INSERT INTO t VALUES (${i}, ${i % 5}, ${i})`);
  }
  run(db, "CREATE INDEX t_v_idx ON t (v)");
  const inMemoryCost = cost(db, "SELECT id FROM t WHERE v = 3");
  db.close();

  const reopened = openDatabase(path, { skipFsync: true });
  assert.equal(cost(reopened, "SELECT id FROM t WHERE v = 3"), inMemoryCost);
  assert.deepEqual(ids(reopened, "SELECT id FROM t WHERE v = 3 ORDER BY id"), [3n, 8n, 13n, 18n]);
  run(reopened, "UPDATE t SET v = 3 WHERE id = 4");
  run(reopened, "DELETE FROM t WHERE id = 13");
  reopened.close();
  const again = openDatabase(path, { skipFsync: true });
  assert.deepEqual(ids(again, "SELECT id FROM t WHERE v = 3 ORDER BY id"), [3n, 4n, 8n, 18n]);
  again.close();
  rmSync(dir, { recursive: true, force: true });
});

test("CREATE INDEX honors the cost ceiling", () => {
  // A ceiling below the build cost (21) aborts deterministically with 54P01 and
  // registers nothing (CLAUDE.md §13).
  const db = db20();
  db.setMaxCost(10n);
  assert.equal(
    errCode(() => run(db, "CREATE INDEX t_v_idx ON t (v)")),
    "54P01",
  );
  db.setMaxCost(0n);
  assert.equal(db.table("t")!.indexes.length, 0);
  run(db, "CREATE INDEX t_v_idx ON t (v)");
});
