// Browser/OPFS host — file-host byte parity (spec/design/hosts.md §5, the slice's "done" criterion).
// OpfsBlockStore is exercised in Node with a FAKE FileSystemSyncAccessHandle (a growable byte buffer),
// so the byte contract is verified WITHOUT a browser. The OPFS host adds no SQL semantics — it must read
// and write the SAME bytes as the Node `fs` host, which the cross-core goldens already pin (CLAUDE.md
// §8). We assert both directions: a database written by the Node host opens identically through OPFS,
// and a database written through OPFS is byte-identical to the Node host's output. The fake handle
// implements EXACTLY the SyncAccessHandle surface OpfsBlockStore needs, so the same OpfsBlockStore code
// the browser will run is what these tests verify.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { close, create, EngineError, execute, open, render } from "../src/lib.ts";
import type { Value } from "../src/lib.ts";
import { createOpfsWithHandle, closeOpfs, openOpfsWithHandle } from "../src/opfs.ts";
import { OpfsBlockStore, type SyncAccessHandle } from "../src/opfsblockstore.ts";
import { bytesEqual } from "./util.ts";
import { specPath } from "./tomlmini.ts";

// FakeSyncAccessHandle is an in-memory stand-in for a FileSystemSyncAccessHandle (hosts.md §5): a
// growable byte buffer with positioned read/write, truncate, getSize, and flush (a no-op — nothing to
// make durable in RAM). It mirrors OPFS read semantics: read fills the buffer prefix and returns the
// byte count, leaving the rest of the caller's buffer untouched (a short read past EOF returns fewer).
class FakeSyncAccessHandle implements SyncAccessHandle {
  private buf: Uint8Array;
  private len: number;

  constructor(initial?: Uint8Array) {
    this.buf = initial ? initial.slice() : new Uint8Array(0);
    this.len = this.buf.length;
  }

  private ensure(n: number): void {
    if (n <= this.buf.length) return;
    let cap = Math.max(this.buf.length, 1);
    while (cap < n) cap *= 2;
    const next = new Uint8Array(cap);
    next.set(this.buf.subarray(0, this.len));
    this.buf = next;
  }

  read(buffer: Uint8Array, opts: { at: number }): number {
    const at = opts.at;
    if (at >= this.len) return 0;
    const n = Math.min(buffer.length, this.len - at);
    buffer.set(this.buf.subarray(at, at + n), 0);
    return n;
  }

  write(buffer: Uint8Array, opts: { at: number }): number {
    const at = opts.at;
    this.ensure(at + buffer.length);
    this.buf.set(buffer, at);
    if (at + buffer.length > this.len) this.len = at + buffer.length;
    return buffer.length;
  }

  truncate(newSize: number): void {
    this.ensure(newSize);
    if (newSize < this.len) this.buf.fill(0, newSize, this.len);
    this.len = newSize;
  }

  getSize(): number {
    return this.len;
  }

  flush(): void {
    // RAM — nothing to make durable.
  }

  close(): void {
    // No OS resource to release.
  }

  // bytes returns the current logical file contents (for byte-parity assertions).
  bytes(): Uint8Array {
    return this.buf.subarray(0, this.len);
  }
}

function intOf(v: Value): bigint {
  if (v.kind !== "int") throw new Error("expected an int value");
  return v.int;
}

// A representative workload: DDL + inserts + an UPDATE and a DELETE (incremental commits), big enough at
// page_size 256 to span several pages and cross the preallocation threshold (so file growth is exercised
// identically on both hosts).
const WORKLOAD: string[] = [
  "CREATE TABLE t (k i32 PRIMARY KEY, v text, n i64)",
  "BEGIN",
  ...Array.from({ length: 80 }, (_, k) => `INSERT INTO t VALUES (${k}, 'row-${k}', ${k * 1000})`),
  "COMMIT",
  "UPDATE t SET v = 'updated' WHERE k = 40",
  "DELETE FROM t WHERE k = 7",
  "INSERT INTO t VALUES (80, 'last', 80000)",
];

// runFsHost runs stmts on a Node `fs`-backed database at path and returns its final on-disk bytes.
function runFsHost(path: string, pageSize: number, stmts: string[]): Uint8Array {
  const db = create(path, { pageSize });
  for (const s of stmts) execute(db, s);
  close(db);
  return new Uint8Array(readFileSync(path));
}

// runOpfsHost runs the SAME stmts through OpfsBlockStore over a fake handle and returns its final bytes.
function runOpfsHost(pageSize: number, stmts: string[]): Uint8Array {
  const handle = new FakeSyncAccessHandle();
  const db = createOpfsWithHandle(handle, { pageSize });
  for (const s of stmts) execute(db, s);
  closeOpfs(db);
  return handle.bytes();
}

test("OPFS host: write parity with the Node fs host (create + incremental commits)", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-opfs-write-"));
  try {
    const fsBytes = runFsHost(join(dir, "fs.jed"), 256, WORKLOAD);
    const opfsBytes = runOpfsHost(256, WORKLOAD);
    assert.equal(
      opfsBytes.length,
      fsBytes.length,
      `OPFS image (${opfsBytes.length}B) and Node fs image (${fsBytes.length}B) differ in length`,
    );
    assert.ok(
      bytesEqual(opfsBytes, fsBytes),
      "OPFS bytes must be byte-identical to the Node fs host's",
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("OPFS host: write parity holds across page sizes", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-opfs-ps-"));
  try {
    for (const ps of [256, 512, 4096]) {
      const fsBytes = runFsHost(join(dir, `fs-${ps}.jed`), ps, WORKLOAD);
      const opfsBytes = runOpfsHost(ps, WORKLOAD);
      assert.ok(bytesEqual(opfsBytes, fsBytes), `byte parity must hold at page_size ${ps}`);
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("OPFS host: reads a file written by the Node fs host (identical rows)", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-opfs-read-"));
  try {
    const path = join(dir, "fs.jed");
    const db = create(path, { pageSize: 512 });
    for (const s of WORKLOAD) execute(db, s);
    const fsRows = db.rowsInKeyOrder("t").map((r) => r.map(render));
    close(db);

    // Open the Node-written file through the OPFS host (fake handle seeded with the file bytes), with a
    // tiny cache so demand paging faults leaves through OpfsBlockStore.readAt.
    const handle = new FakeSyncAccessHandle(new Uint8Array(readFileSync(path)));
    const odb = openOpfsWithHandle(handle, { cacheBytes: 4 * 512 });
    const opfsRows = odb.rowsInKeyOrder("t").map((r) => r.map(render));
    closeOpfs(odb);

    assert.deepEqual(opfsRows, fsRows, "OPFS must read the Node-written file into identical rows");
    assert.ok(opfsRows.length > 0);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("OPFS host: a file written through OPFS opens via the Node fs host (round-trip)", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-opfs-rt-"));
  try {
    // Write through OPFS into a fake handle, then materialize those exact bytes to a real file and open
    // it with the Node `fs` host — the reverse direction of the cross-host golden (hosts.md §5).
    const handle = new FakeSyncAccessHandle();
    const odb = createOpfsWithHandle(handle, { pageSize: 256 });
    for (const s of WORKLOAD) execute(odb, s);
    const opfsRows = odb.rowsInKeyOrder("t").map((r) => r.map(render));
    closeOpfs(odb);

    const path = join(dir, "from-opfs.jed");
    writeFileSync(path, handle.bytes());
    const fsdb = open(path);
    const fsRows = fsdb.rowsInKeyOrder("t").map((r) => r.map(render));
    close(fsdb);

    assert.deepEqual(
      fsRows,
      opfsRows,
      "the Node host must read the OPFS-written file into identical rows",
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("OPFS host: reopen-after-close sees the committed data (durability through the handle)", () => {
  // Close releases the handle; re-acquiring a fresh OpfsBlockStore over the same bytes must see every
  // committed row — the autocommit-durability contract (api.md §2.3), exercised on OPFS.
  const handle = new FakeSyncAccessHandle();
  const db = createOpfsWithHandle(handle, { pageSize: 256 });
  for (const s of WORKLOAD) execute(db, s);
  closeOpfs(db);

  const reopened = openOpfsWithHandle(new FakeSyncAccessHandle(handle.bytes()), {});
  const rows = reopened.rowsInKeyOrder("t");
  // 80 inserted in the batch + 1 after - 1 deleted (k=7) = 80 rows.
  assert.equal(rows.length, 80);
  for (const r of rows) assert.notEqual(intOf(r[0]!), 7n, "k=7 was deleted");
  closeOpfs(reopened);
});

test("OPFS host: loads a shared golden fixture (cross-core read parity)", () => {
  // The load-bearing cross-core round-trip (CLAUDE.md §8) read through OPFS: a golden authored by the
  // Ruby reference at page_size 256 — tall_tree has interior B-tree nodes, so this faults leaf pages
  // through OpfsBlockStore.readAt via the demand-paging pool.
  const golden = new Uint8Array(readFileSync(specPath("fileformat/fixtures/tall_tree.jed")));
  const db = openOpfsWithHandle(new FakeSyncAccessHandle(golden), { cacheBytes: 3 * 256 });
  const names = db.tableNames();
  assert.ok(names.length > 0, "the golden has at least one table");
  for (const name of names) {
    const rows = db.rowsInKeyOrder(name);
    assert.ok(rows.length > 0, `golden table ${name} has rows`);
  }
  closeOpfs(db);
});

test("OPFS host: create rejects an invalid page size before touching the handle (0A000)", () => {
  const handle = new FakeSyncAccessHandle();
  let code = "";
  try {
    createOpfsWithHandle(handle, { pageSize: 1000 }); // in range but not a power of two
  } catch (e) {
    if (e instanceof EngineError) code = e.code();
  }
  assert.equal(code, "0A000", "non-power-of-two page size is feature_not_supported");
  assert.equal(
    handle.getSize(),
    0,
    "a rejected create leaves the handle untouched (write-in-place safety)",
  );
});
