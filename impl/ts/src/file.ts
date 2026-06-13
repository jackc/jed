// Host file layer for the TS core (spec/design/api.md §2): open/create/commit/close a single-file
// database durably on the Node `fs` host. Isolated here so the future browser/OPFS host is a sibling,
// not a reshape (storage.md §2). create lays down the from-scratch image (temp-file + fsync + atomic
// rename + directory fsync, api.md §3); every later commit is an incremental copy-on-write write of
// just the dirty pages, published by alternating the meta slot (spec/fileformat/format.md, P6.1 part
// B) — the block seam below pwrites pages (writeSync at a position) into the open file.

import { closeSync, existsSync, fsyncSync, openSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

import { DEFAULT_PAGE_SIZE, Database, Snapshot } from "./executor.ts";
import { engineError } from "./errors.ts";
import { incrementalImage, loadDatabasePaged, metaPage, toImage } from "./format.ts";
import { cacheLeaves, DEFAULT_CACHE_BYTES, SharedPaging } from "./paging.ts";
import { Pager } from "./pager.ts";

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
  // Adopt the just-written file as the open pager + buffer pool, so later commits write through the
  // seam without re-opening (spec/design/pager.md). A freshly-created database has no rows, so nothing
  // is OnDisk yet — tables built in this session stay resident until a reopen demand-pages them.
  let fd: number;
  try {
    fd = openSync(path, "r+");
  } catch (e) {
    throw ioError(e);
  }
  db.paging = new SharedPaging(Pager.fromFd(fd), cacheLeaves(DEFAULT_CACHE_BYTES, db.pageSize)); // valid header
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

// OpenOptions are open-time settings for a file-backed database (spec/design/api.md §2.1). Unlike
// DatabaseOptions (create-time, fixed into the file), these are handle settings — not stored in the
// file, so a different host may reopen the same file with different ones. cacheBytes is the buffer-pool
// budget in bytes: roughly the maximum memory the resident leaf cache holds at once (pager.md §3,
// P6.4b/c). Bytes, not a page count, so the budget does not silently scale with the file's page size;
// the engine converts it to a leaf-page capacity by the file's page size as max(1, cacheBytes /
// pageSize) (cacheLeaves). The bound that lets a database far larger than RAM be served (pager.md §1);
// it never changes what a query observes (§3/§5). Default DEFAULT_CACHE_BYTES (256 MiB).
export type OpenOptions = { cacheBytes?: number };

// open opens an existing file-backed database at path with optional open settings (the memory budget,
// opts.cacheBytes). Loads its committed state, adopting its page size / txid. The path must exist —
// 58P01 otherwise; a malformed file is XX001, a read failure 58030 (api.md §2.1).
//
// The demand-paged loader builds only the interior B-tree skeleton resident, faulting each leaf through
// the bounded buffer pool on access, so the resident set is bounded by the pool — not the file size
// (P6.4b). The byte budget is converted to a leaf-page capacity by the file's page size (cacheLeaves).
// The budget is a handle setting, not stored in the file (§3). Later commits write through the same
// pager kept open for the handle's life.
export function open(path: string, opts: OpenOptions = {}): Database {
  if (!existsSync(path)) {
    throw engineError("undefined_file", "database file does not exist: " + path);
  }
  const cacheBytes = opts.cacheBytes ?? DEFAULT_CACHE_BYTES;
  let fd: number;
  try {
    fd = openSync(path, "r+");
  } catch (e) {
    throw ioError(e);
  }
  try {
    // Read the file's page size first, then convert the byte budget to a leaf-page capacity; the loader
    // rejects an out-of-range page size as corrupt (cacheLeaves clamps the divisor so a malformed
    // page_size = 0 cannot divide by zero before that check runs).
    const pager = Pager.fromFd(fd);
    const db = loadDatabasePaged(new SharedPaging(pager, cacheLeaves(cacheBytes, pager.pageSize)));
    db.path = path;
    db.persistHook = persistImpl; // autocommit each later write (transactions.md §4.1)
    return db;
  } catch (e) {
    closeSync(fd); // a malformed file / read failure must not leak the fd
    if (e instanceof Error && e.name === "EngineError") throw e;
    throw ioError(e);
  }
}

// persistImpl durably publishes snap to the backing file via an incremental copy-on-write commit
// (spec/fileformat/format.md *Allocation & incremental commit*; transactions.md §9), installed as the
// Database.persistHook by create/open and called by commitTx with the working snapshot being
// published. Write the dirty pages this transaction introduced — reusing free-list pages a prior root
// abandoned before extending the file (P6.2) — fsync, write the alternate meta slot (snap.txid & 1),
// fsync. Clean pages are never rewritten. A crash between the two fsyncs leaves the prior meta — and
// thus the prior snapshot — intact (its pages were not overwritten: a reused free page is reachable
// from no live snapshot). An in-memory database has no persistHook. db.pageCount / db.freePages advance
// only after both fsyncs succeed, so a write failure leaves db, committed, and the file's prior meta
// untouched (the working snapshot is then discarded). The future synchronous=off mode gates here.
function persistImpl(db: Database, snap: Snapshot): void {
  // An in-memory database has no paging context — a no-op success (committed swaps in commitTx after
  // this). JS is single-threaded, so the read (fault) and this commit-write path never overlap.
  if (db.paging === null) return;
  const write = incrementalImage(snap, db.pageSize, db.pageCount, db.freePages, db.paging);
  for (const pg of write.pages) {
    db.paging.writeBlock(pg.index, pg.bytes);
  }
  db.paging.sync(); // body pages durable before the meta can reference them
  const meta = metaPage(db.pageSize, snap.txid, write.rootPage, write.pageCount);
  db.paging.writeBlock(Number(snap.txid & 1n), meta);
  db.paging.sync(); // the commit is published
  db.pageCount = write.pageCount;
  db.freePages = write.freeRemaining;
}

// residentLeaves is the number of leaf pages currently resident in the buffer pool — 0 for an
// in-memory database (it is fully resident, nothing to page). The read-only gauge the
// OpenOptions.cacheBytes budget bounds (≤ cacheBytes / pageSize by construction; spec/design/pager.md §3).
export function residentLeaves(db: Database): number {
  return db.paging === null ? 0 : db.paging.residentLeaves();
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
  if (db.paging !== null) {
    db.paging.close(); // drop the open fd (close it)
    db.paging = null;
  }
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
