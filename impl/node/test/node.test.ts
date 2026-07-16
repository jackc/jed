import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { Database } from "../index.ts";

test("wraps create, prepared execution, query, and reopen", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-node-"));
  const path = join(dir, "test.jed");
  try {
    const db = Database.create(path);
    db.execute("CREATE TABLE t (id i64 PRIMARY KEY, name text)");
    const insert = db.prepare("INSERT INTO t VALUES ($1, $2)");
    insert.execute([1n, "one"]);
    insert.execute([2n, "two"]);
    insert.close();
    const lookup = db.prepare("SELECT id, name FROM t WHERE id = $1");
    assert.deepEqual(lookup.query([2n]), [[2n, "two"]]);
    lookup.close();
    db.close();

    const reopened = Database.open(path);
    assert.deepEqual(reopened.query("SELECT id, name FROM t ORDER BY id"), [
      [1n, "one"],
      [2n, "two"],
    ]);
    reopened.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
