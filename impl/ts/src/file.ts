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
  commit(db); // materialize the empty image (txid 0 -> 1)
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
  return db;
}

// commit durably persists the whole current image to the backing file and increments txid. An
// in-memory database (no path) is a no-op success (spec/design/api.md §2).
export function commit(db: Database): void {
  if (db.path === null) return;
  const nextTxid = db.txid + 1n;
  const bytes = toImage(db, db.pageSize, nextTxid);
  writeAtomic(db.path, bytes);
  db.txid = nextTxid;
}

// close releases the handle. It does NOT commit — uncommitted changes since the last commit are
// discarded (spec/design/api.md §2). Idempotent.
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
