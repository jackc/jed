// Host-only loading paths must not stage already committed GiST entries (§7).
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { attachFile, createDatabase, Database, EngineError, intValue } from "../src/lib.ts";

for (const mode of ["deserialize", "attach"]) {
  test(`GiST ${mode} begins with no pending writes`, () => {
    const dir = mkdtempSync(join(tmpdir(), "jed-gist-load-memory-"));
    const path = join(dir, "source.jed");
    const source = createDatabase({ path, skipFsync: true, locking: "none" });
    let db: Database | undefined;
    try {
      const initial = source.session();
      try {
        for (const sql of [
          "CREATE TABLE t (id i32 PRIMARY KEY, r i32range)",
          "CREATE INDEX t_r ON t USING gist (r)",
          "INSERT INTO t VALUES (1, '[1,5)')",
        ])
          initial.execute(sql);
      } finally {
        initial.close();
      }
      const image = source.serialize("main");
      source.close();
      let name = "t";
      if (mode === "deserialize") db = Database.deserialize(image);
      else {
        db = createDatabase();
        db.attach("aux", attachFile(path, { locking: "none" }), false);
        name = "aux.t";
      }
      const s = db.session({ maxQueryMemoryBytes: 1n });
      try {
        // A point miss retains no input and stages nothing, including after load.
        s.execute(`DELETE FROM ${name} WHERE id = 99`);
        const insert = `INSERT INTO ${name} VALUES (2, '[10,20)')`;
        assert.throws(
          () => s.execute(insert),
          (e: unknown) => e instanceof EngineError && e.code() === "54P05",
        );
        s.setMaxQueryMemoryBytes(4096n);
        s.execute(insert);
        s.setMaxQueryMemoryBytes(1n);
        s.execute(`DELETE FROM ${name} WHERE id = 99`);
        s.setMaxQueryMemoryBytes(4096n);
        const rows = s.query(`SELECT count(*) FROM ${name}`);
        try {
          assert.deepEqual([...rows], [[intValue(2n)]]);
        } finally {
          rows.close();
        }
      } finally {
        s.close();
      }
    } finally {
      db?.close();
      source.close();
      rmSync(dir, { recursive: true, force: true });
    }
  });
}
