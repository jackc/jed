import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import type { BlockStore } from "../src/blockstore.ts";
import { Snapshot } from "../src/executor.ts";
import { loadEnginePaged, toImage } from "../src/format.ts";
import { MemoryBlockStore } from "../src/memoryblockstore.ts";
import { Pager } from "../src/pager.ts";
import { SharedPaging } from "../src/paging.ts";
import { Database } from "../src/shared.ts";
import { createDatabase, openDatabase, attachFile } from "../src/lib.ts";

class CountingStore implements BlockStore {
  memory = new MemoryBlockStore(toImage(new Snapshot(1n), 4096, 1n));
  writes = 0;
  syncs = 0;
  grows = 0;
  readAt(offset: number, len: number): Uint8Array {
    return this.memory.readAt(offset, len);
  }
  writeAt(offset: number, bytes: Uint8Array): void {
    this.writes++;
    this.memory.writeAt(offset, bytes);
  }
  size(): number {
    return this.memory.size();
  }
  setSize(size: number): void {
    this.grows++;
    this.memory.setSize(size);
  }
  sync(): void {
    this.syncs++;
  }
  close(): void {}
  image(): Uint8Array {
    return this.readAt(0, this.size()).slice();
  }
  reset(): void {
    this.writes = this.syncs = this.grows = 0;
  }
}

test("unchanged public commits preserve bytes and issue no writes or barriers", () => {
  const store = new CountingStore();
  const engine = loadEnginePaged(new SharedPaging(Pager.fromStore(store), 64));
  engine.path = "counting-device"; // drive the durable host path over an instrumented byte device
  const db = Database.fromEngine(engine);
  const s = db.session({});
  try {
    for (const sql of [
      "CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
      "INSERT INTO t VALUES (1, 1)",
      "CREATE SEQUENCE s",
    ])
      s.execute(sql);
    const cases = readFileSync(
      new URL("../../../spec/conformance/storage/unchanged_commits.tsv", import.meta.url),
      "utf8",
    );
    for (const line of cases.split("\n")) {
      if (line === "" || line.startsWith("#")) continue;
      const [mode, delta, sql] = line.split("\t");
      const before = store.image();
      const version = db.version;
      store.reset();
      if (mode !== "auto" && mode !== "script") s.begin(true);
      const run = () => {
        if (mode === "script") s.executeScript(sql);
        else for (const stmt of sql.split(";")) if (stmt.trim() !== "") s.execute(stmt);
      };
      if (mode === "failed") assert.throws(run);
      else run();
      if (mode === "tx" || mode === "failed") s.commit();
      if (mode === "rollback") s.rollback();
      assert.equal(db.version, version + BigInt(delta), line);
      if (delta === "0") {
        assert.deepEqual(store.image(), before, line);
        assert.deepEqual([store.writes, store.syncs, store.grows], [0, 0, 0], line);
      } else {
        assert.ok(store.writes > 0 && store.syncs > 0, line);
      }
    }
  } finally {
    s.close();
    db.close();
  }
});

test("attachment-only commit persists its file and leaves main unchanged", async () => {
  const { mkdtempSync, rmSync } = await import("node:fs");
  const { join } = await import("node:path");
  const { tmpdir } = await import("node:os");
  const dir = mkdtempSync(join(tmpdir(), "jed-noop-attach-"));
  const path = join(dir, "main.jed");
  const attached = join(dir, "attached.jed");
  try {
    createDatabase({ path: attached }).close();
    const db = createDatabase({ path });
    try {
      db.attach("a", attachFile(attached), false);
      const s = db.session();
      try {
        s.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
        const before = readFileSync(path);
        const version = db.version;
        s.begin(true);
        for (const sql of [
          "DELETE FROM t WHERE id = 999",
          "CREATE TABLE a.t (id i32 PRIMARY KEY)",
          "INSERT INTO a.t VALUES (1)",
        ])
          s.execute(sql);
        s.commit();
        assert.equal(db.version, version);
        assert.deepEqual(readFileSync(path), before);
        const attBefore = readFileSync(attached);
        s.execute("DELETE FROM a.t WHERE id = 999");
        assert.deepEqual(readFileSync(attached), attBefore);
      } finally {
        s.close();
      }
      db.detach("a");
    } finally {
      db.close();
    }
    const reopened = openDatabase(attached);
    try {
      assert.equal([...reopened.query("SELECT id FROM t")].length, 1);
    } finally {
      reopened.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
