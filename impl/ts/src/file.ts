// Host file layer for the TS core (spec/design/api.md §2): open/create/commit/close a single-file
// database durably on the Node `fs` host. Isolated here so the future browser/OPFS host is a sibling,
// not a reshape (storage.md §2). create lays down the from-scratch image (temp-file + fsync + atomic
// rename + directory fsync, api.md §3); every later commit is an incremental copy-on-write write of
// just the dirty pages, published by alternating the meta slot (spec/fileformat/format.md, P6.1 part
// B) — the block seam below pwrites pages (writeSync at a position) into the open file.

import {
  closeSync,
  existsSync,
  fsyncSync,
  openSync,
  readFileSync,
  renameSync,
  unlinkSync,
  writeFileSync,
  writeSync,
} from "node:fs";
import { dirname } from "node:path";

import { DEFAULT_PAGE_SIZE, Database, Snapshot } from "./executor.ts";
import { engineError } from "./errors.ts";
import { incrementalImage, loadDatabase, metaPage, toImage } from "./format.ts";

// DatabaseOptions are the settings for a newly-created database file (spec/design/api.md §2).
// pageSize is fixed into the file's meta at creation and cannot change thereafter.
export type DatabaseOptions = { pageSize?: number };

// create makes a new file-backed database at path with opts (the page size is locked into the
// file). The path must not already exist — 58P02 otherwise. An initial empty image is written
// durably immediately, so the file exists with its page size fixed (api.md §2).
export function create(path: string, opts: DatabaseOptions = {}): Database {
  if (existsSync(path)) {
    throw engineError("duplicate_file", "database file already exists: " + path);
  }
  const db = new Database();
  db.path = path;
  db.pageSize = opts.pageSize ?? DEFAULT_PAGE_SIZE;
  db.committed.txid = 1n; // the initial empty image is committed as txid 1
  db.persistHook = persistImpl; // publish each later commit incrementally (transactions.md §4.1/§9)
  writeFullImage(db); // lay down the from-scratch image; later commits are incremental
  return db;
}

// writeFullImage lays down the whole from-scratch image of the committed snapshot (the all-dirty
// special case — spec/fileformat/format.md) durably via temp-file + rename, and records the on-disk
// page high-water. Used by create to establish a fresh file with both meta slots seeded; every later
// commit is incremental (persistImpl).
function writeFullImage(db: Database): void {
  if (db.path === null) return;
  const bytes = toImage(db.committed, db.pageSize, db.committed.txid);
  writeAtomic(db.path, bytes);
  db.pageCount = Math.floor(bytes.length / db.pageSize);
}

// open opens an existing file-backed database at path (loading its committed state and adopting
// its page size / txid). The path must exist — 58P01 otherwise; a malformed file is XX001, a read
// failure 58030 (api.md §2).
export function open(path: string): Database {
  let bytes: Uint8Array;
  try {
    if (!existsSync(path)) {
      throw engineError("undefined_file", "database file does not exist: " + path);
    }
    bytes = readFileSync(path);
  } catch (e) {
    if (e instanceof Error && e.name === "EngineError") throw e;
    throw ioError(e);
  }
  const db = loadDatabase(bytes);
  db.path = path;
  db.persistHook = persistImpl; // autocommit each later write (transactions.md §4.1)
  return db;
}

// persistImpl durably publishes snap to the backing file via an incremental copy-on-write commit
// (spec/fileformat/format.md *Allocation & incremental commit*; transactions.md §9), installed as the
// Database.persistHook by create/open and called by commitTx with the working snapshot being
// published. Append the dirty pages this transaction introduced, fsync, write the alternate meta slot
// (snap.txid & 1), fsync. Clean pages are never rewritten; pages an old root drops are leaked (P6.2
// reclaims). A crash between the two fsyncs leaves the prior meta — and thus the prior snapshot —
// intact (the body pages were only appended). An in-memory database has no persistHook. db.pageCount
// advances only after both fsyncs succeed, so a write failure leaves db, committed, and the file's
// prior meta untouched (the working snapshot is then discarded). The future synchronous=off mode
// gates here.
function persistImpl(db: Database, snap: Snapshot): void {
  if (db.path === null) return;
  const write = incrementalImage(snap, db.pageSize, db.pageCount);
  const ps = db.pageSize;
  const fd = openSync(db.path, "r+");
  try {
    for (const pg of write.pages) {
      writeSync(fd, pg.bytes, 0, pg.bytes.length, pg.index * ps);
    }
    fsyncSync(fd); // body pages durable before the meta can reference them
    const meta = metaPage(ps, snap.txid, write.rootPage, write.pageCount);
    writeSync(fd, meta, 0, meta.length, Number(snap.txid & 1n) * ps);
    fsyncSync(fd); // the commit is published
  } finally {
    closeSync(fd);
  }
  db.pageCount = write.pageCount;
}

// commit commits the current transaction (spec/design/api.md §2.2, transactions.md §4.2).
// Publishes the open explicit block durably (per synchronous, via the persistHook); a commit with
// no open block is a lenient no-op success (under autocommit each statement already committed).
// Drives the same mechanism as SQL COMMIT.
export function commit(db: Database): void {
  db.commitTx();
}

// rollback rolls back the current transaction (spec/design/api.md §2.2, transactions.md §4.2).
// Discards the open explicit block's working set; a rollback with no open block is a no-op
// success. Drives the same mechanism as SQL ROLLBACK.
export function rollback(db: Database): void {
  db.rollbackTx();
}

// close releases the handle (spec/design/api.md §2.3). It rolls back any open explicit transaction
// (its in-progress work is discarded) and does not commit one. Under autocommit every prior
// statement is already durable, so — unlike the original model — close does NOT drop committed
// work; durability is never hidden in a destructor. Idempotent.
export function close(db: Database): void {
  db.rollbackTx();
  db.path = null;
}

// writeAtomic writes bytes to path crash-safely (spec/design/api.md §3): a sibling temp file,
// fsync, atomic rename over the target, then a best-effort directory fsync so the rename is
// durable.
function writeAtomic(path: string, bytes: Uint8Array): void {
  const tmp = path + ".jedtmp";
  try {
    const fd = openSync(tmp, "w");
    try {
      writeFileSync(fd, bytes);
      fsyncSync(fd);
    } finally {
      closeSync(fd);
    }
    renameSync(tmp, path);
  } catch (e) {
    try {
      if (existsSync(tmp)) unlinkSync(tmp);
    } catch {
      // best-effort cleanup
    }
    throw ioError(e);
  }
  // Directory fsync makes the rename itself durable. Best-effort: not every platform allows
  // opening a directory for fsync (Windows), and the rename is already atomic there.
  try {
    const dfd = openSync(dirname(path), "r");
    try {
      fsyncSync(dfd);
    } finally {
      closeSync(dfd);
    }
  } catch {
    // directory fsync unsupported on this platform — acceptable (api.md §3)
  }
}

function ioError(e: unknown): Error {
  const msg = e instanceof Error ? e.message : String(e);
  return engineError("io_error", "I/O error: " + msg);
}
