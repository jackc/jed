// Tests for the jed-migrate TS package, driven through the public API against the shared
// ../../testdata/ corpus (design.md §10). Runs on bare Node via type-stripping: `node --test`.

import assert from "node:assert/strict";
import { rmSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";
import { createDatabase, type Database } from "../../../impl/ts/src/lib.ts";
import {
  BadVersionError,
  IrreversibleMigrationError,
  LoadError,
  loadMigrations,
  loadMigrationsFromEntries,
  type Migration,
  MigrationError,
  Migrator,
  newMigration,
  resolveTargets,
} from "../src/index.ts";

const TESTDATA = join(import.meta.dirname, "../../testdata");
const testdata = (sub: string): string => join(TESTDATA, sub);

function memDb(): Database {
  return createDatabase({});
}

function count(db: Database, sql: string): number {
  const row = db.get(sql);
  if (row === undefined) throw new Error("no row");
  return Number(Object.values(row)[0]);
}

// ───────────────────────────── loading ─────────────────────────────

test("loads the blog set", () => {
  const migrations = loadMigrations(testdata("blog"));
  assert.equal(migrations.length, 3);
  assert.deepEqual(
    migrations.map((m) => m.name),
    ["001_create_users", "002_add_posts", "003_add_email_index"],
  );
  migrations.forEach((m, i) => {
    assert.equal(m.sequence, i + 1);
    assert.notEqual(m.down, null, `${m.name} should be reversible`);
  });
  assert.ok(migrations[0].up.includes("insert into users"));
  assert.ok(!(migrations[0].down as string).includes("insert into"));
});

test("loads the irreversible set", () => {
  const migrations = loadMigrations(testdata("irreversible"));
  assert.equal(migrations.length, 2);
  assert.notEqual(migrations[0].down, null);
  assert.equal(migrations[1].down, null); // 002 has no separator
});

test("ignores non-migration files", () => {
  const migrations = loadMigrations(testdata("ignored"));
  assert.equal(migrations.length, 1);
  assert.equal(migrations[0].name, "001_only");
});

test("refuses malformed sets", () => {
  for (const sub of ["gap", "duplicate", "missing_one", "empty_up"]) {
    assert.throws(() => loadMigrations(testdata(`malformed/${sub}`)), LoadError, sub);
  }
});

test("embedded source loads identically to the directory", () => {
  const fromDir = loadMigrations(testdata("blog"));
  // A record keyed by full path — the shape of a bundler glob; basenames are matched.
  const embedded = loadMigrationsFromEntries({
    "migrations/001_create_users.sql":
      fromDir[0].up + "\n\n---- create above / drop below ----\n\n" + fromDir[0].down,
    "migrations/002_add_posts.sql":
      fromDir[1].up + "\n\n---- create above / drop below ----\n\n" + fromDir[1].down,
    "migrations/003_add_email_index.sql":
      fromDir[2].up + "\n\n---- create above / drop below ----\n\n" + fromDir[2].down,
    "migrations/README.md": "ignored",
  });
  assert.equal(embedded.length, 3);
  assert.deepEqual(
    embedded.map((m) => m.name),
    ["001_create_users", "002_add_posts", "003_add_email_index"],
  );
});

// ───────────────────────────── the migrate walk ─────────────────────────────

test("migrate up then down round-trips", () => {
  const db = memDb();
  const m = new Migrator(db, loadMigrations(testdata("blog")));
  try {
    assert.equal(m.currentVersion(), 0);
    assert.deepEqual(db.tableNames(), ["schema_version"]);

    m.migrate();
    assert.equal(m.currentVersion(), 3);
    assert.deepEqual(db.tableNames(), ["posts", "schema_version", "users"]);

    m.migrateTo(0);
    assert.equal(m.currentVersion(), 0);
    assert.deepEqual(db.tableNames(), ["schema_version"]);

    // Back up again — proves the down halves truly reversed the schema.
    m.migrate();
    assert.deepEqual(db.tableNames(), ["posts", "schema_version", "users"]);
  } finally {
    m.close();
  }
});

test("stepwise migration runs the multi-statement up half", () => {
  const db = memDb();
  const m = new Migrator(db, loadMigrations(testdata("blog")));
  try {
    for (let target = 1; target <= 3; target++) {
      m.migrateTo(target);
      assert.equal(m.currentVersion(), target);
    }
    assert.equal(count(db, "select count(*) as n from users"), 2);
  } finally {
    m.close();
  }
});

test("fast path is a no-op", () => {
  const db = memDb();
  const m = new Migrator(db, loadMigrations(testdata("blog")));
  try {
    m.migrate();
    m.migrateTo(3);
    m.migrate();
    assert.equal(m.currentVersion(), 3);
  } finally {
    m.close();
  }
});

test("a bad target is rejected", () => {
  const db = memDb();
  const m = new Migrator(db, loadMigrations(testdata("blog")));
  try {
    assert.throws(() => m.migrateTo(4), BadVersionError);
    assert.throws(() => m.migrateTo(-1), BadVersionError);
  } finally {
    m.close();
  }
});

test("an irreversible down fails and leaves the version unmoved", () => {
  const db = memDb();
  const m = new Migrator(db, loadMigrations(testdata("irreversible")));
  try {
    m.migrate();
    assert.equal(m.currentVersion(), 2);
    assert.throws(() => m.migrateTo(0), IrreversibleMigrationError);
    assert.equal(m.currentVersion(), 2);
  } finally {
    m.close();
  }
});

test("a migration error carries context and rolls back", () => {
  const db = memDb();
  const migrations: Migration[] = [
    {
      sequence: 1,
      name: "001_bad",
      up: "create table ok (id bigint primary key);\ninsert into nope (id) values (1);",
      down: null,
    },
  ];
  const m = new Migrator(db, migrations);
  try {
    let caught: unknown;
    try {
      m.migrate();
    } catch (e) {
      caught = e;
    }
    assert.ok(caught instanceof MigrationError, `expected MigrationError, got ${caught}`);
    assert.equal((caught as MigrationError).migration, "001_bad");
    assert.equal((caught as MigrationError).direction, "up");
    assert.notEqual((caught as MigrationError).statement, "");
    assert.equal((caught as MigrationError).sqlState(), "42P01"); // undefined table
    assert.equal(m.currentVersion(), 0); // rolled back
    assert.ok(!db.tableNames().includes("ok"));
  } finally {
    m.close();
  }
});

test("in-script transaction control is rejected", () => {
  const db = memDb();
  const m = new Migrator(db, loadMigrations(testdata("tx_control")));
  try {
    let caught: unknown;
    try {
      m.migrate();
    } catch (e) {
      caught = e;
    }
    assert.ok(caught instanceof MigrationError);
    assert.equal((caught as MigrationError).sqlState(), "0A000");
    assert.equal(m.currentVersion(), 0);
  } finally {
    m.close();
  }
});

test("status reports progress", () => {
  const db = memDb();
  const m = new Migrator(db, loadMigrations(testdata("blog")));
  try {
    assert.deepEqual(m.status(), { current: 0, target: 3, pending: 3 });
    m.migrateTo(2);
    assert.deepEqual(m.status(), { current: 2, target: 3, pending: 1 });
  } finally {
    m.close();
  }
});

test("custom version table", () => {
  const db = memDb();
  const m = new Migrator(db, loadMigrations(testdata("blog")), { versionTable: "migration_state" });
  try {
    m.migrateTo(1);
    const names = db.tableNames();
    assert.ok(names.includes("migration_state"));
    assert.ok(!names.includes("schema_version"));
  } finally {
    m.close();
  }
});

test("an invalid version table name is rejected", () => {
  const db = memDb();
  assert.throws(
    () =>
      new Migrator(db, loadMigrations(testdata("blog")), {
        versionTable: "bad name; drop table x",
      }),
    LoadError,
  );
});

// ───────────────────────────── target grammar ─────────────────────────────

test("resolves the target grammar", () => {
  const N = 5;
  assert.deepEqual(resolveTargets("", 0, N), [5]);
  assert.deepEqual(resolveTargets("last", 2, N), [5]);
  assert.deepEqual(resolveTargets("3", 0, N), [3]);
  assert.deepEqual(resolveTargets("0", 5, N), [0]);
  assert.deepEqual(resolveTargets("+2", 1, N), [3]);
  assert.deepEqual(resolveTargets("-2", 5, N), [3]);
  assert.deepEqual(resolveTargets("-+1", 5, N), [4, 5]);
  assert.deepEqual(resolveTargets("-+3", 5, N), [2, 5]);
  for (const bad of ["6", "+9", "-+9", "banana", "+"]) {
    assert.throws(() => resolveTargets(bad, 5, N), bad);
  }
  assert.throws(() => resolveTargets("-1", 0, N));
});

// ───────────────────────────── scaffolding ─────────────────────────────

test("newMigration scaffolds the next sequence", () => {
  const dir = join(import.meta.dirname, `../.tmp-new-${process.pid}`);
  rmSync(dir, { recursive: true, force: true });
  try {
    const p1 = newMigration(dir, "create_users");
    assert.ok(p1.endsWith("001_create_users.sql"));
    const p2 = newMigration(dir, "add_posts");
    assert.ok(p2.endsWith("002_add_posts.sql"));
    // The comment-only stubs have empty up halves, so loading refuses the set.
    assert.throws(() => loadMigrations(dir), LoadError);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
