// Host file layer for the TS core (spec/design/api.md §2): open/create/commit/close a single-file
// database durably (whole-image model) on the Node `fs` host. Isolated here so the future
// browser/OPFS host is a sibling, not a reshape (storage.md §2). The crash-safe commit is
// temp-file + fsync + atomic rename + directory fsync (api.md §3); since a commit rewrites the
// whole file, rename gives all-or-nothing replacement for free.

import {
  closeSync,
  existsSync,
  fsyncSync,
  openSync,
  readFileSync,
  renameSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { dirname } from "node:path";

import { DEFAULT_PAGE_SIZE, Database } from "./executor.ts";
import { engineError } from "./errors.ts";
import { loadDatabase, toImage } from "./format.ts";

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
  db.txid = 0n;
  db.persistHook = persistImpl; // autocommit each later write (transactions.md §4.1)
  persistImpl(db); // materialize the empty image (txid 0 -> 1)
  return db;
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

// persistImpl durably writes the whole current image to the backing file and increments txid —
// the single synchronous-commit chokepoint (spec/design/transactions.md §9), installed as the
// Database.persistHook by create/open. The autocommit path calls it after every successful write
// statement; create calls it for the initial image. An in-memory database (no path) is a no-op
// success. The future synchronous=off mode (batched/deferred fsync) gates here.
function persistImpl(db: Database): void {
  if (db.path === null) return;
  const nextTxid = db.txid + 1n;
  const bytes = toImage(db, db.pageSize, nextTxid);
  writeAtomic(db.path, bytes);
  db.txid = nextTxid;
}

// commit commits the current transaction (spec/design/api.md §2.2). jed autocommits each
// statement (transactions.md §4.1), so in this slice there is no open explicit transaction to
// publish — commit is a lenient no-op success (§4.2). Explicit BEGIN … COMMIT blocks, where
// commit does the durable publish, arrive in P5.2.
export function commit(_db: Database): void {}

// rollback rolls back the current transaction (spec/design/api.md §2.2). With autocommit and no
// open explicit transaction (this slice), there is nothing uncommitted to discard — a no-op
// success. Discarding an open explicit block's working set arrives with BEGIN in P5.2.
export function rollback(_db: Database): void {}

// close releases the handle (spec/design/api.md §2.3). Under autocommit, every prior statement is
// already durable, so — unlike the original model — close does NOT drop committed work; it would
// roll back an open explicit transaction (none in this slice). Durability is never hidden in a
// destructor. Idempotent.
export function close(db: Database): void {
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
