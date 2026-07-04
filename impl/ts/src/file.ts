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
  renameSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { dirname } from "node:path";

import { FileBlockStore } from "./fileblockstore.ts";
import { DEFAULT_PAGE_SIZE, Engine } from "./executor.ts";
import { engineError } from "./errors.ts";
import { loadEnginePaged, toImage } from "./format.ts";
import { cacheLeaves, DEFAULT_CACHE_BYTES, SharedPaging } from "./paging.ts";
import { Pager } from "./pager.ts";
import { persistImpl } from "./persist.ts";
import { buildInMemory, Database, registerFileAttachOpener } from "./shared.ts";
import { FileSpillSink } from "./spillfile.ts";

// DatabaseOptions are the settings for a newly-created database file (spec/design/api.md §2).
// pageSize is fixed into the file's meta at creation and cannot change thereafter.
// noSync is the fsync=off host setting (api.md §2.1): a commit writes identical bytes in the same order
// but skips the fdatasync barrier. Unlike pageSize it is NOT fixed into the file — it is a runtime handle
// setting. DEV/TESTING only (durable across a process crash, not an OS crash).
export type DatabaseOptions = { pageSize?: number; noSync?: boolean };

// CreateOptions are the settings for creating a fresh database (spec/design/api.md §2.1/§2.1.1). path
// selects the backing: absent → an in-memory database (never touches the filesystem); present → a
// single file at that path (58P02 if it already exists). It is a genuine optional, not an overloaded
// empty-string sentinel (api.md §2.1). pageSize (absent/0 → DEFAULT_PAGE_SIZE) is locked into a file's
// meta at creation and fixes an in-memory database's tree fan-out (the page-backed B-tree's fan-out
// tracks the page size), so it is meaningful for both backings.
// noFsync turns off the per-commit fsync for this handle (the fsync=off host setting, api.md §2.1):
// commits write identical bytes in the same order but skip the fdatasync barrier. DEV/TESTING ONLY —
// durable across a process crash, not an OS crash / power loss. Ignored for an in-memory database (no
// path) which never fsyncs. Byte/cost/result-neutral; default false.
export type CreateOptions = { path?: string; pageSize?: number; noFsync?: boolean };

// create makes a new file-backed database at path with opts (the page size is locked into the
// file). The path must not already exist — 58P02 otherwise. An initial empty image is written
// durably immediately, so the file exists with its page size fixed (api.md §2).
export function create(path: string, opts: DatabaseOptions = {}): Engine {
  if (existsSync(path)) {
    throw engineError("duplicate_file", "database file already exists: " + path);
  }
  const db = new Engine();
  db.path = path;
  db.pageSize = opts.pageSize ?? DEFAULT_PAGE_SIZE;
  db.committed.txid = 1n; // the initial empty image is committed as txid 1
  db.persistHook = persistImpl; // publish each later commit incrementally (transactions.md §4.1/§9)
  const noSync = opts.noSync ?? false;
  writeFullImage(db, noSync); // lay down the from-scratch image; later commits are incremental
  // Adopt the just-written file as the open pager + buffer pool, so later commits write through the
  // seam without re-opening (spec/design/pager.md). Tables built in this session bind this pager at
  // creation (Snapshot.storePaging), so their committed leaves demote at each commit and fault back
  // through the pool — same residency shape as after a reopen.
  let fd: number;
  try {
    fd = openSync(path, "r+");
  } catch (e) {
    throw ioError(e);
  }
  db.paging = new SharedPaging(
    Pager.fromStore(new FileBlockStore(fd, noSync)),
    cacheLeaves(DEFAULT_CACHE_BYTES, db.pageSize),
  ); // valid header
  db.committed.storePaging = db.paging;
  db.spillSink = new FileSpillSink(dirname(path)); // ORDER BY spills next to the database file (spill.md §4)
  return db;
}

// writeFullImage lays down the whole from-scratch image of the committed snapshot (the all-dirty
// special case — spec/fileformat/format.md) durably via temp-file + rename, and records the on-disk
// page high-water. Used by create to establish a fresh file with both meta slots seeded; every later
// commit is incremental (persistImpl).
function writeFullImage(db: Engine, noSync: boolean): void {
  if (db.path === null) return;
  const bytes = toImage(db.committed, db.pageSize, db.committed.txid);
  writeAtomic(db.path, bytes, noSync);
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
// readOnly opens the file read-only (api.md §2.1): the handle then behaves like PostgreSQL hot
// standby — every transaction defaults to READ ONLY, an explicit READ WRITE request and any write
// statement are 25006, and the file is opened without write access, so it is never written.
// workMem is the work-memory budget in bytes for a blocking operator before it spills to disk
// (spec/design/spill.md §3, api.md §2.1): the ORDER BY external merge sort holds at most roughly this
// many bytes of rows resident, then spills sorted runs. Like cacheBytes it is a handle setting that
// never changes what a query observes (spill.md §6). Default DEFAULT_WORK_MEM (256 MiB).
// noFsync turns off the per-commit fsync (the fsync=off host setting, api.md §2.1): a commit still writes
// the same bytes in the same order but the fdatasync barrier becomes a no-op — much faster. DEV/TESTING
// ONLY: the data survives a process crash (the OS page cache still flushes) but NOT an OS crash / power
// loss. Never changes what a query observes or the on-disk bytes; default false.
export type OpenOptions = {
  cacheBytes?: number;
  readOnly?: boolean;
  workMem?: number;
  noFsync?: boolean;
};

// open opens an existing file-backed database at path with optional open settings (the memory budget,
// opts.cacheBytes). Loads its committed state, adopting its page size / txid. The path must exist —
// 58P01 otherwise; a malformed file is XX001, a read failure 58030 (api.md §2.1).
//
// The demand-paged loader builds only the interior B-tree skeleton resident, faulting each leaf through
// the bounded buffer pool on access, so the resident set is bounded by the pool — not the file size
// (P6.4b). The byte budget is converted to a leaf-page capacity by the file's page size (cacheLeaves).
// The budget is a handle setting, not stored in the file (§3). Later commits write through the same
// pager kept open for the handle's life.
export function open(path: string, opts: OpenOptions = {}): Engine {
  if (!existsSync(path)) {
    throw engineError("undefined_file", "database file does not exist: " + path);
  }
  const cacheBytes = opts.cacheBytes ?? DEFAULT_CACHE_BYTES;
  const readOnly = opts.readOnly ?? false;
  let fd: number;
  try {
    // A read-only open never writes the file, so it is not opened for writing at all — the OS
    // enforces what the executor's 25006 guards promise (api.md §2.1).
    fd = openSync(path, readOnly ? "r" : "r+");
  } catch (e) {
    throw ioError(e);
  }
  try {
    // Read the file's page size first, then convert the byte budget to a leaf-page capacity; the loader
    // rejects an out-of-range page size as corrupt (cacheLeaves clamps the divisor so a malformed
    // page_size = 0 cannot divide by zero before that check runs).
    const pager = Pager.fromStore(new FileBlockStore(fd, opts.noFsync ?? false));
    const db = loadEnginePaged(new SharedPaging(pager, cacheLeaves(cacheBytes, pager.pageSize)));
    db.path = path;
    db.persistHook = persistImpl; // autocommit each later write (transactions.md §4.1)
    db.readOnly = readOnly;
    db.spillSink = new FileSpillSink(dirname(path)); // ORDER BY spills next to the database file (spill.md §4)
    if (opts.workMem !== undefined) db.session.workMem = opts.workMem;
    return db;
  } catch (e) {
    closeSync(fd); // a malformed file / read failure must not leak the fd
    if (e instanceof Error && e.name === "EngineError") throw e;
    throw ioError(e);
  }
}

// Register the Node file host as the file-attach opener (attached-databases.md §4, Slice 2): importing
// file.ts (the node host) enables `db.attach(name, attachFile(path))` without shared.ts importing a host
// module (it stays browser-clean). The OPFS host registers its own. Read-only opens the file O_RDONLY.
registerFileAttachOpener((path, readOnly) => open(path, { readOnly }));

// createDatabase makes a fresh database — in-memory (opts.path absent) or file-backed (opts.path set)
// — and returns the host Database handle with its default session (spec/design/api.md §2.1/§2.1.1). A
// file that already exists is 58P02; the page size is locked into the file. The in-memory path cannot
// fail in substance (it never touches the filesystem) but shares the uniform signature — a caller
// wanting an infallible in-memory handle wraps this (the tests' memDb helper does). This is the one
// create constructor for both backings; the in-memory-specific constructors are removed.
export function createDatabase(opts: CreateOptions = {}): Database {
  const pageSize = opts.pageSize || DEFAULT_PAGE_SIZE;
  if (opts.path !== undefined) {
    return Database.fromEngine(create(opts.path, { pageSize, noSync: opts.noFsync }));
  }
  return buildInMemory(pageSize); // in-memory never fsyncs; noFsync is a no-op
}

// openDatabase opens an existing file-backed database at path with optional open settings and returns
// the host Database handle with its default session (the back-compat bridge, spec/design/session.md §2.4).
export function openDatabase(path: string, opts: OpenOptions = {}): Database {
  return Database.fromEngine(open(path, opts));
}

// residentLeaves is the number of leaf pages currently resident in the buffer pool — 0 for an
// in-memory database (it is fully resident, nothing to page). The read-only gauge the
// OpenOptions.cacheBytes budget bounds (≤ cacheBytes / pageSize by construction; spec/design/pager.md §3).
export function residentLeaves(db: Engine): number {
  return db.paging === null ? 0 : db.paging.residentLeaves();
}

// commit commits the current transaction (spec/design/api.md §2.2, transactions.md §4.2).
// Publishes the open explicit block durably (per synchronous, via the persistHook); a commit with
// no open block is a lenient no-op success (under autocommit each statement already committed).
// Drives the same mechanism as SQL COMMIT.
export function commit(db: Engine): void {
  db.commitTx();
}

// rollback rolls back the current transaction (spec/design/api.md §2.2, transactions.md §4.2).
// Discards the open explicit block's working set; a rollback with no open block is a no-op
// success. Drives the same mechanism as SQL ROLLBACK.
export function rollback(db: Engine): void {
  db.rollbackTx();
}

// close releases the handle (spec/design/api.md §2.3). It rolls back any open explicit transaction
// (its in-progress work is discarded) and does not commit one. Under autocommit every prior
// statement is already durable, so — unlike the original model — close does NOT drop committed
// work; durability is never hidden in a destructor. Idempotent.
export function close(db: Engine): void {
  db.rollbackTx();
  db.path = null;
  if (db.paging !== null) {
    db.paging.close(); // drop the open fd (close it)
    db.paging = null;
  }
}

// writeAtomic writes bytes to path crash-safely (spec/design/api.md §3): a sibling temp file,
// fsync, atomic rename over the target, then a best-effort directory fsync so the rename is
// durable. Under noSync (fsync=off) the two fsyncs are skipped — the write + rename still happen,
// but the bytes are only in the OS page cache (dev/testing; no durability on an OS crash).
function writeAtomic(path: string, bytes: Uint8Array, noSync: boolean): void {
  const tmp = path + ".jedtmp";
  try {
    const fd = openSync(tmp, "w");
    try {
      writeFileSync(fd, bytes);
      if (!noSync) fsyncSync(fd);
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
  if (noSync) return; // fsync=off: skip the directory-fsync barrier too (dev/testing).
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
