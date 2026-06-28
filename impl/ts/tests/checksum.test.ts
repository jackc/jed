// Per-page checksum (format_version 7): every body page — catalog, B-tree leaf, B-tree interior,
// and overflow — carries a CRC-32/IEEE over its own bytes (spec/fileformat/format.md *Page header*;
// spec/design/storage.md §6). This pins the durability guarantee that distinguishes reliability item
// #3 from the meta-only checksum: a silently corrupted LIVE page is detected as XX001 the instant it
// is read — at open for a catalog/interior/overflow page (the loader and the free-list reachability
// walk), at fault for a leaf — and is NEVER served as wrong rows. A corrupted DEAD page (free space
// an earlier incremental commit abandoned, P6.2) is harmless: not reachable from the committed
// snapshot, so the file still reads back exactly. The invariant asserted is the strong one:
// corrupting any body page yields either XX001 or the byte-identical correct result — corruption is
// caught or inert, never silent. Mirrors impl/rust/tests/checksum.rs and impl/go/checksum_test.go.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { close, create, EngineError, execute, open, render } from "../src/tooling.ts";
import { fillerText } from "./util.ts";

const PAGE_SIZE = 256;

// scanChecksum opens path and returns the rendered "SELECT id, body" rows; it throws if any page
// read fails (open of a corrupt catalog/interior/overflow page, or a fault of a corrupt leaf).
function scanChecksum(path: string): string[][] {
  const db = open(path);
  try {
    const o = execute(db, "SELECT id, body FROM t ORDER BY id");
    if (o.kind !== "query") throw new Error("expected a query");
    return o.rows.map((r) => r.map((v) => render(v)));
  } finally {
    close(db);
  }
}

test("corrupting any body page is caught or inert, never silent", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-checksum-"));
  try {
    const path = join(dir, "seed.jed");
    const cpath = join(dir, "corrupt.jed");

    // Seed a tree spanning every body-page kind at page_size 256: a multi-leaf B-tree (interior
    // root) of ~30 rows, with row 1 a 600-char incompressible body that spills out-of-line.
    const db = create(path, { pageSize: PAGE_SIZE });
    execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, body text)");
    let sql = `INSERT INTO t VALUES (1, '${fillerText(600)}')`;
    for (let id = 2; id <= 30; id++) sql += `, (${id}, 'row${id}')`;
    execute(db, sql);
    close(db);

    const want = scanChecksum(path);
    assert.equal(want.length, 30, "30 rows seeded");

    const clean = readFileSync(path);
    const pages = Math.floor(clean.length / PAGE_SIZE);
    assert.ok(pages >= 6, `the seed should span several pages, got ${pages}`);

    // Corrupt one payload byte of each body page in turn (pages 0/1 are the meta slots, checksummed
    // separately — incremental.test.ts / reclamation.test.ts). The flip is NOT CRC-repaired, so a
    // live page fails its per-page checksum; a dead page is never read and the snapshot is unaffected.
    let detected = 0;
    for (let i = 2; i < pages; i++) {
      const bytes = Uint8Array.from(clean);
      bytes[i * PAGE_SIZE + 16] ^= 0xff; // first payload byte (offset PAGE_HEADER = 16)
      writeFileSync(cpath, bytes);
      try {
        const rows = scanChecksum(cpath);
        assert.deepEqual(rows, want, `corrupting dead page ${i} must not change results`);
      } catch (e) {
        if (!(e instanceof EngineError)) throw e;
        assert.equal(e.code(), "XX001", `corrupting live page ${i} must be data_corrupted`);
        detected++;
      }
    }

    // The live pages — catalog, the interior root, several leaves, the overflow chain — are all
    // protected; a floor of 4 guarantees detection fired across page kinds, not just one.
    assert.ok(detected >= 4, `expected live pages across kinds to be detected, got ${detected}`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
