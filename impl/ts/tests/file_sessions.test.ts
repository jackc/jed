// Slice 7c — file-backed sessions + the default-session bridge (spec/design/session.md §2.4/§10).
// These per-core tests cover what the corpus cannot express (host-API surface + on-disk durability):
// that createDatabase/openDatabase return the shared core with a stateful default session whose
// autocommit writes persist durably and survive a reopen; that a read-only open rejects writes
// (25006); and that file-backed read sessions stay snapshot-isolated as the default session commits
// between their calls (TS gives isolation, not CPU parallelism — CLAUDE.md §2). The logical
// transaction/visibility semantics stay in the shared concurrency corpus (suites/concurrency/).

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import {
  createDatabase,
  type Database,
  EngineError,
  openDatabase,
  type Session,
} from "../src/lib.ts";

function tmpDir(): string {
  return mkdtempSync(join(tmpdir(), "jed-7c-"));
}

function countDB(db: Database): bigint {
  const rows = [...db.query("SELECT count(*) FROM t")];
  const v = rows[0][0];
  return v.kind === "int" ? v.int : -1n;
}

function countSession(s: Session): bigint {
  const rows = [...s.query("SELECT count(*) FROM t")];
  const v = rows[0][0];
  return v.kind === "int" ? v.int : -1n;
}

test("create + default session persists and reopens", () => {
  const dir = tmpDir();
  const path = join(dir, "roundtrip.jed");
  try {
    {
      const db = createDatabase({ path, skipFsync: true });
      assert.equal(db.version, 1n); // the initial empty image is committed as version 1
      db.execute("CREATE TABLE t (id i64 PRIMARY KEY)");
      assert.equal(db.version, 2n); // the autocommit CREATE published version 2
      for (let i = 1; i <= 5; i++) db.execute(`INSERT INTO t VALUES (${i})`);
      assert.equal(countDB(db), 5n);
      db.close();
    }
    const db = openDatabase(path, { skipFsync: true });
    try {
      assert.equal(countDB(db), 5n);
      assert.equal(db.version, 7n); // 1 (create) + 1 (CREATE TABLE) + 5 (inserts)
    } finally {
      db.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("explicit transaction on a session persists then rolls back", () => {
  const dir = tmpDir();
  const path = join(dir, "explicit_tx.jed");
  try {
    {
      const db = createDatabase({ path, skipFsync: true });
      // Explicit transactions live on a Session (the persistent default-session bridge was removed
      // from Database): mint one over the file-backed core and drive begin/commit/rollback on it.
      const s = db.session({});
      s.execute("CREATE TABLE t (id i64 PRIMARY KEY)");
      // A committed explicit block is durable.
      s.begin(true);
      s.execute("INSERT INTO t VALUES (1)");
      s.execute("INSERT INTO t VALUES (2)");
      s.commit();
      assert.equal(countSession(s), 2n);
      // A rolled-back block leaves nothing.
      s.begin(true);
      s.execute("INSERT INTO t VALUES (3)");
      s.rollback();
      assert.equal(countSession(s), 2n);
      s.close();
      db.close();
    }
    const db = openDatabase(path, { skipFsync: true });
    try {
      assert.equal(countDB(db), 2n); // only the committed block survived the reopen
    } finally {
      db.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("executeScript on a file-backed default session is all-or-nothing", () => {
  const dir = tmpDir();
  const path = join(dir, "script.jed");
  try {
    {
      const db = createDatabase({ path, skipFsync: true });
      const summary = db.executeScript(
        "CREATE TABLE t (id i64 PRIMARY KEY); INSERT INTO t VALUES (1); INSERT INTO t VALUES (2);",
      );
      assert.equal(summary.statementsRun, 3);
      assert.equal(countDB(db), 2n);
      db.close();
    }
    const db = openDatabase(path, { skipFsync: true });
    try {
      assert.equal(countDB(db), 2n);
    } finally {
      db.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a read-only open rejects writes (25006)", () => {
  const dir = tmpDir();
  const path = join(dir, "read_only.jed");
  try {
    {
      const db = createDatabase({ path, skipFsync: true });
      db.execute("CREATE TABLE t (id i64 PRIMARY KEY)");
      db.execute("INSERT INTO t VALUES (1)");
      db.close();
    }
    const db = openDatabase(path, { readOnly: true, skipFsync: true });
    try {
      assert.equal(countDB(db), 1n);
      assert.throws(
        () => db.execute("INSERT INTO t VALUES (2)"),
        (e: unknown) => e instanceof EngineError && e.code() === "25006",
      );
      // A read/write session minted from a read-only core also rejects writes.
      const w = db.writeSession();
      try {
        assert.throws(
          () => w.execute("INSERT INTO t VALUES (3)"),
          (e: unknown) => e instanceof EngineError && e.code() === "25006",
        );
      } finally {
        w.close();
      }
    } finally {
      db.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a file-backed read session stays isolated as the default session commits", () => {
  // The TS analog of the threaded cores' concurrent-reader test: a pinned read session faults pages
  // from a file-backed snapshot and observes one stable version even as the default session commits
  // between its calls. A fresh reader sees the new state; the file is durable on reopen.
  const dir = tmpDir();
  const path = join(dir, "isolation.jed");
  try {
    const db = createDatabase({ path, pageSize: 256, skipFsync: true }); // small pages so the table spans leaves
    db.execute("CREATE TABLE t (id i64 PRIMARY KEY)");
    db.execute("INSERT INTO t VALUES (1)");

    const pinned = db.readSession(); // pins the one-row version
    try {
      for (let i = 2; i <= 20; i++) db.execute(`INSERT INTO t VALUES (${i})`);
      assert.equal(countSession(pinned), 1n); // snapshot isolation: pinned reader unchanged
      const fresh = db.readSession();
      try {
        assert.equal(countSession(fresh), 20n); // a fresh reader sees every commit
      } finally {
        fresh.close();
      }
    } finally {
      pinned.close();
    }
    db.close();

    const reopened = openDatabase(path, { skipFsync: true });
    try {
      assert.equal(countDB(reopened), 20n); // durable on disk
    } finally {
      reopened.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
