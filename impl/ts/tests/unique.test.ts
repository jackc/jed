// UNIQUE constraints + unique indexes (spec/design/constraints.md §5, indexes.md §8) —
// covers what the corpus suite (ddl/unique.test) cannot: catalog introspection (the
// unique flag, fold results, name order), the v6 on-disk round-trip, transactional DDL,
// and the documented PG divergences (end-state UPDATE validation, droppable
// constraint-backed indexes). Mirrored in impl/rust/tests/unique.rs and
// impl/go/unique_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, EngineError, execute, loadDatabase, toImage } from "../src/lib.ts";

function run(db: Database, sql: string) {
  return execute(db, sql);
}

function cost(db: Database, sql: string): bigint {
  return run(db, sql).cost;
}

function ids(db: Database, sql: string): bigint[] {
  const o = run(db, sql);
  assert.equal(o.kind, "query", sql);
  return (o.kind === "query" ? o.rows : []).map((r) => {
    assert.equal(r[0]!.kind, "int");
    return r[0]!.kind === "int" ? r[0]!.int : 0n;
  });
}

function errInfo(fn: () => unknown): { code: string; message: string } {
  try {
    fn();
  } catch (e) {
    if (e instanceof EngineError) return { code: e.code(), message: e.message };
    throw e;
  }
  throw new Error("expected an error");
}

// names is each index of the table as "name" or "name!" (unique), in catalog order.
function names(db: Database, table: string): string[] {
  const t = db.table(table);
  assert.ok(t, `table ${table} missing`);
  return t.indexes.map((ix) => ix.name + (ix.unique ? "!" : ""));
}

// Constraint naming matches PostgreSQL (oracle-probed, constraints.md §5.3): the
// lowercased <table>_<cols>_key base with the smallest free suffix, walked past BOTH the
// relation namespace and the table's check names; an explicit CONSTRAINT name is the
// index name as written.
test("constraint naming matches PostgreSQL", () => {
  const db = new Database();
  run(db, "CREATE TABLE other (x int32)");
  run(db, "CREATE INDEX walk_a_key ON other (x)"); // occupies the derived base
  run(
    db,
    "CREATE TABLE Walk (a int32 UNIQUE, b int32, CONSTRAINT Named UNIQUE (b, a), " +
      "CONSTRAINT walk_b_check CHECK (b > 0), UNIQUE (b))",
  );
  assert.deepEqual(names(db, "walk"), ["Named!", "walk_a_key1!", "walk_b_key!"]);
  // A derived name walks past a CHECK name too (PG-probed: w1_a_key -> w1_a_key1).
  run(db, "CREATE TABLE w1 (a int32, CONSTRAINT w1_a_key CHECK (a > 0), UNIQUE (a))");
  assert.deepEqual(names(db, "w1"), ["w1_a_key1!"]);
});

// The dedup/fold rules match PostgreSQL (oracle-probed, constraints.md §5.2): identical
// member lists fold into one (the first explicitly-named one's name wins); a list
// identical to the primary key's folds away entirely; a differing ORDER is distinct.
test("dedup and PK fold match PostgreSQL", () => {
  const db = new Database();
  run(db, "CREATE TABLE e3 (a int32 UNIQUE UNIQUE, UNIQUE (a))");
  assert.deepEqual(names(db, "e3"), ["e3_a_key!"]);
  // An unnamed-then-named pair keeps the NAME (PG: p1 kept "named").
  run(db, "CREATE TABLE p1 (a int32 UNIQUE, CONSTRAINT named UNIQUE (a))");
  assert.deepEqual(names(db, "p1"), ["named!"]);
  // Two named duplicates keep the FIRST (PG: e7 kept "x").
  run(db, "CREATE TABLE e7 (a int32, CONSTRAINT x UNIQUE (a), CONSTRAINT y UNIQUE (a))");
  assert.deepEqual(names(db, "e7"), ["x!"]);
  // The PK absorbs an identical list — regardless of declaration order or form.
  run(db, "CREATE TABLE e5 (a int32 PRIMARY KEY UNIQUE)");
  assert.deepEqual(names(db, "e5"), []);
  run(db, "CREATE TABLE p2 (a int32 UNIQUE, PRIMARY KEY (a))");
  assert.deepEqual(names(db, "p2"), []);
  run(db, "CREATE TABLE e9 (a int32, b int32, PRIMARY KEY (a, b), UNIQUE (a, b))");
  assert.deepEqual(names(db, "e9"), []);
  // A differing member ORDER is a distinct constraint (PG: p3 kept both).
  run(db, "CREATE TABLE p3 (a int32, b int32, PRIMARY KEY (a, b), UNIQUE (b, a))");
  assert.deepEqual(names(db, "p3"), ["p3_b_a_key!"]);
});

// DDL errors match PostgreSQL (oracle-probed, constraints.md §5.1/§5.3): member
// resolution 42703/42701/0A000 (before any CHECK validates), explicit-name collisions
// 42P07 (relation namespace, including the table being created) before 42710 (the
// table's constraint names).
test("DDL errors match PostgreSQL", () => {
  const db = new Database();
  run(db, "CREATE TABLE other (x int32)");
  assert.equal(errInfo(() => run(db, "CREATE TABLE e2 (a int32, UNIQUE (nosuch))")).code, "42703");
  assert.equal(errInfo(() => run(db, "CREATE TABLE e1 (a int32, UNIQUE (a, a))")).code, "42701");
  assert.equal(errInfo(() => run(db, "CREATE TABLE e6 (a int32, s text UNIQUE)")).code, "0A000");
  // UNIQUE members resolve BEFORE any CHECK validates (PG: z1/z2), in either order.
  assert.equal(
    errInfo(() => run(db, "CREATE TABLE z1 (a int32, CHECK (nosuch1 > 0), UNIQUE (nosuch2))")).code,
    "42703",
  );
  assert.match(
    errInfo(() => run(db, "CREATE TABLE z2 (a int32, UNIQUE (nosuch2), CHECK (nosuch1 > 0))"))
      .message,
    /nosuch2/,
  );
  // An explicit constraint name collides in the RELATION namespace: an existing table,
  // the table being created (PG: p4), and a same-statement sibling (PG: e8).
  assert.equal(
    errInfo(() => run(db, "CREATE TABLE c2 (a int32, CONSTRAINT other UNIQUE (a))")).code,
    "42P07",
  );
  assert.equal(
    errInfo(() => run(db, "CREATE TABLE p4 (a int32, CONSTRAINT p4 UNIQUE (a))")).code,
    "42P07",
  );
  assert.equal(
    errInfo(() =>
      run(db, "CREATE TABLE e8 (a int32, CONSTRAINT x UNIQUE (a), b int32, CONSTRAINT x UNIQUE (b))"),
    ).code,
    "42P07",
  );
  // ... and with a CHECK constraint's name it is 42710, in either declaration order
  // (PG: z4/z5 — both report when the unique constraint is created).
  assert.equal(
    errInfo(() =>
      run(db, "CREATE TABLE z4 (a int32, CONSTRAINT zc CHECK (a > 0), CONSTRAINT zc UNIQUE (a))"),
    ).code,
    "42710",
  );
  assert.equal(
    errInfo(() =>
      run(db, "CREATE TABLE z5 (a int32, CONSTRAINT zc UNIQUE (a), CONSTRAINT zc CHECK (a > 0))"),
    ).code,
    "42710",
  );
  // CREATE UNIQUE <not-index> is a syntax error.
  assert.equal(errInfo(() => run(db, "CREATE UNIQUE TABLE t (a int32)")).code, "42601");
});

// INSERT enforcement (indexes.md §8): a duplicate against the store or within the batch
// traps 23505 naming the index; NULLS DISTINCT exempts any tuple with a NULL component;
// the violation precedence is CHECK before PK before UNIQUE, and among unique indexes
// the catalog (name) order.
test("INSERT enforcement", () => {
  const db = new Database();
  run(
    db,
    "CREATE TABLE t (id int32 PRIMARY KEY, v int32 UNIQUE, w int32, " +
      "CONSTRAINT wv UNIQUE (w, v), CHECK (id < 100))",
  );
  run(db, "INSERT INTO t VALUES (1, 10, 100), (2, NULL, 100)");
  // A stored duplicate; the message names the violated index.
  const dup = errInfo(() => run(db, "INSERT INTO t VALUES (3, 10, 200)"));
  assert.equal(dup.code, "23505");
  assert.match(dup.message, /t_v_key/);
  // An in-batch duplicate (two-phase: nothing stored).
  assert.equal(
    errInfo(() => run(db, "INSERT INTO t VALUES (3, 30, 1), (4, 30, 2)")).code,
    "23505",
  );
  assert.deepEqual(ids(db, "SELECT id FROM t ORDER BY id"), [1n, 2n]);
  // NULLS DISTINCT: any number of NULLs coexist, and a NULL component exempts the
  // multi-column tuple — (100, NULL) twice is fine even though w matches.
  run(db, "INSERT INTO t VALUES (5, NULL, 100)");
  run(db, "INSERT INTO t VALUES (6, NULL, 300), (7, NULL, 300)");
  // A fully non-NULL composite duplicate traps, naming the composite index (its own
  // table — beside t_v_key the component dup would always be reported first).
  run(db, "CREATE TABLE c (id int32 PRIMARY KEY, w int32, v int32, CONSTRAINT wv2 UNIQUE (w, v))");
  run(db, "INSERT INTO c VALUES (1, 40, 400)");
  assert.match(errInfo(() => run(db, "INSERT INTO c VALUES (2, 40, 400)")).message, /wv2/);
  // A distinct pair sharing one component is a different tuple — allowed.
  run(db, "INSERT INTO c VALUES (2, 40, 401)");
  run(db, "INSERT INTO t VALUES (8, 40, 400)");
  // INSERT ... SELECT takes the same path.
  assert.equal(
    errInfo(() => run(db, "INSERT INTO t SELECT id + 20, v, w FROM t WHERE id = 8")).code,
    "23505",
  );
  // Precedence: the PK's 23505 wins over UNIQUE (PG-probed), naming <table>_pkey.
  assert.match(errInfo(() => run(db, "INSERT INTO t VALUES (1, 10, 999)")).message, /t_pkey/);
  // ... and CHECK (23514) fires before either (PG-probed).
  assert.equal(errInfo(() => run(db, "INSERT INTO t VALUES (101, 10, 999)")).code, "23514");
  // Two violated unique indexes report in catalog (name) order: t_v_key < wv.
  assert.match(errInfo(() => run(db, "INSERT INTO t VALUES (10, 40, 400)")).message, /t_v_key/);
});

// UPDATE validates uniqueness against the statement's END STATE (indexes.md §8 — the
// documented PG divergence): self-resolving rewrites succeed; genuine conflicts with
// untouched rows and in-batch collisions trap 23505; nothing is written on failure.
test("UPDATE enforcement validates the end state", () => {
  const db = new Database();
  run(db, "CREATE TABLE m (id int32 PRIMARY KEY, v int32 UNIQUE)");
  run(db, "INSERT INTO m VALUES (1, 1), (2, 2), (3, 30)");
  // PG fails both of these on the transient per-row collision; jed's end state is unique.
  run(db, "UPDATE m SET v = v + 1 WHERE id < 3"); // 1,2 -> 2,3
  assert.deepEqual(ids(db, "SELECT v FROM m ORDER BY id"), [2n, 3n, 30n]);
  run(db, "UPDATE m SET v = 5 - v WHERE id < 3"); // swap: 2,3 -> 3,2
  assert.deepEqual(ids(db, "SELECT v FROM m ORDER BY id"), [3n, 2n, 30n]);
  // A no-op rewrite of the same value is fine (its own old entry never conflicts).
  run(db, "UPDATE m SET v = v WHERE id = 1");
  // A genuine conflict with an untouched row.
  assert.equal(errInfo(() => run(db, "UPDATE m SET v = 30 WHERE id = 1")).code, "23505");
  // An in-batch collision: two rewritten rows landing on one value.
  assert.equal(errInfo(() => run(db, "UPDATE m SET v = 7 WHERE id < 3")).code, "23505");
  // All-or-nothing: the failed statements wrote nothing.
  assert.deepEqual(ids(db, "SELECT v FROM m ORDER BY id"), [3n, 2n, 30n]);
  // NULL is exempt on UPDATE too: several rows may go NULL at once.
  run(db, "UPDATE m SET v = NULL WHERE id < 3");
  assert.deepEqual(ids(db, "SELECT id FROM m WHERE v IS NULL ORDER BY id"), [1n, 2n]);
});

// CREATE UNIQUE INDEX verifies existing rows before registering (indexes.md §2/§8): a
// duplicate traps 23505 and creates nothing (the name stays free); NULLs are exempt;
// thereafter it enforces like a constraint-backed index. The auto-name keeps _idx.
test("CREATE UNIQUE INDEX build verification", () => {
  const db = new Database();
  run(db, "CREATE TABLE d (id int32 PRIMARY KEY, a int32, n int32)");
  run(db, "INSERT INTO d VALUES (1, 7, NULL), (2, 7, NULL), (3, 8, 5)");
  // Build over duplicates fails and registers nothing.
  const e = errInfo(() => run(db, "CREATE UNIQUE INDEX dup ON d (a)"));
  assert.equal(e.code, "23505");
  assert.match(e.message, /dup/);
  assert.deepEqual(names(db, "d"), []);
  // The name is free again (nothing was created).
  run(db, "CREATE TABLE dup (x int32)");
  run(db, "DROP TABLE dup");
  // NULLs are exempt at build time (two NULLs in n).
  run(db, "CREATE UNIQUE INDEX ON d (n)"); // d_n_idx — the _idx auto-name
  assert.deepEqual(names(db, "d"), ["d_n_idx!"]);
  // ... and it enforces thereafter.
  assert.equal(errInfo(() => run(db, "INSERT INTO d VALUES (4, 9, 5)")).code, "23505");
  run(db, "INSERT INTO d VALUES (4, 9, NULL)");
});

// DROP INDEX of a constraint-backed unique index is allowed and drops the constraint
// (the documented PG divergence — indexes.md §7: jed has no ALTER TABLE, so the index
// name is the constraint's only handle).
test("DROP INDEX drops the constraint", () => {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, v int32 UNIQUE)");
  run(db, "INSERT INTO t VALUES (1, 10)");
  assert.equal(errInfo(() => run(db, "INSERT INTO t VALUES (2, 10)")).code, "23505");
  run(db, "DROP INDEX t_v_key");
  run(db, "INSERT INTO t VALUES (2, 10)"); // no longer enforced
  assert.deepEqual(names(db, "t"), []);
});

// Uniqueness validation is unmetered (cost.md §3): an INSERT into a uniquely-indexed
// table still costs 0, and a CREATE UNIQUE INDEX build charges exactly the plain build's
// scan. The planner treats a unique index like any other (the bounded-scan cost).
test("costs are unchanged by unique", () => {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, v int32, w int32)");
  for (let i = 1; i <= 20; i++) {
    run(db, `INSERT INTO t VALUES (${i}, ${i % 5}, ${i})`);
  }
  // The unique build charges the same page_read(1) + 20 rows = 21 as a plain build.
  assert.equal(cost(db, "CREATE UNIQUE INDEX t_w_idx ON t (w)"), 21n);
  // INSERT ... VALUES stays zero-cost — the probe is unmetered.
  assert.equal(cost(db, "INSERT INTO t VALUES (21, 9, 21)"), 0n);
  // The unique index bounds a scan exactly like a plain one: 1 index node + 1 point
  // lookup + 1 row + 1 filter eval + 1 produced = 5.
  assert.equal(cost(db, "SELECT id FROM t WHERE w = 7"), 5n);
});

// The v6 round-trip: the unique flag survives serialize -> load (and the reloaded
// database still enforces), and the image is byte-stable across a second serialize.
test("round-trip preserves unique", () => {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, v int32 UNIQUE, w int32)");
  run(db, "CREATE INDEX plain ON t (w)");
  run(db, "INSERT INTO t VALUES (1, 10, 100), (2, NULL, 100)");
  const image = toImage(db, 8192, 1n);
  const loaded = loadDatabase(image);
  assert.deepEqual(names(loaded, "t"), ["plain", "t_v_key!"]);
  assert.equal(errInfo(() => run(loaded, "INSERT INTO t VALUES (3, 10, 1)")).code, "23505");
  run(loaded, "INSERT INTO t VALUES (3, NULL, 1)");
  assert.deepEqual(toImage(db, 8192, 1n), image, "byte-stable");
});

// Transactional DDL: a UNIQUE created inside a rolled-back block leaves no trace — no
// definition, no store, no enforcement (the §3 snapshot model).
test("transactional DDL rolls back", () => {
  const db = new Database();
  run(db, "CREATE TABLE t (id int32 PRIMARY KEY, v int32)");
  run(db, "INSERT INTO t VALUES (1, 10)");
  run(db, "BEGIN");
  run(db, "CREATE UNIQUE INDEX u ON t (v)");
  assert.equal(errInfo(() => run(db, "INSERT INTO t VALUES (2, 10)")).code, "23505");
  run(db, "ROLLBACK");
  assert.deepEqual(names(db, "t"), []);
  run(db, "INSERT INTO t VALUES (2, 10)"); // not enforced after rollback
});
