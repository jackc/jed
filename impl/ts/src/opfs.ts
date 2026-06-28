// Host bootstrap for the Browser/OPFS storage host (spec/design/api.md §2, hosts.md §5): open/create a
// single-file database backed by OPFS. The sibling of file.ts (the Node `fs` host) — same shape, OPFS
// backing. Two layers:
//   • a SYNCHRONOUS core (createOpfsWithHandle / openOpfsWithHandle / closeOpfs) that drives the engine
//     against an ALREADY-ACQUIRED sync access handle, identical in shape to file.ts; and
//   • thin ASYNC wrappers (createOpfs / openOpfs) that do the async OPFS acquisition (getDirectory →
//     getFileHandle → createSyncAccessHandle), then hand the handle to the sync core.
// The split keeps the engine synchronous (the seam stays sync — hosts.md §5) and makes the byte path
// testable in Node with a fake handle (the parity test), while the async wrappers reach `navigator` only
// INSIDE their bodies (via globalThis), so importing this module under Node is safe. This module imports
// no `node:*` directly; once executor.ts's transitive `node:*` deps are cleared (the worker slice), it
// is browser-clean.
//
// Resolved open questions (hosts.md §5):
//   • create writes the from-scratch image IN PLACE — no temp-file + rename (OPFS has no POSIX rename);
//     the meta-slot swap + per-page CRC carry all-or-nothing for every later commit, and a torn create
//     is detected as XX001 on open, never silent bad data.
//   • a single long-lived, exclusive sync access handle is held for the database's life (the exclusive
//     lock is the single-writer guarantee, CLAUDE.md §3) and released by closeOpfs.
//   • db.path is left null: OPFS is file-backed via db.paging, but spill-to-disk (executor newSorterFor)
//     uses node:fs and has no OPFS backing yet, so it is disabled (sorts stay resident, like an
//     in-memory database — spill.md §2); spilling via OPFS is a later enhancement.

import { Engine, DEFAULT_PAGE_SIZE } from "./executor.ts";
import { engineError } from "./errors.ts";
import { loadEnginePaged, toImage } from "./format.ts";
import { cacheLeaves, DEFAULT_CACHE_BYTES, SharedPaging } from "./paging.ts";
import { Pager } from "./pager.ts";
import { persistImpl } from "./persist.ts";
import { OpfsBlockStore, type SyncAccessHandle } from "./opfsblockstore.ts";

// DatabaseOptions / OpenOptions mirror file.ts (spec/design/api.md §2/§2.1): create-time pageSize is
// fixed into the file's meta; open-time cacheBytes / readOnly / workMem are handle settings.
export type DatabaseOptions = { pageSize?: number };
export type OpenOptions = { cacheBytes?: number; readOnly?: boolean; workMem?: number };

// createOpfsWithHandle creates a new file-backed database over an already-acquired, EMPTY OPFS sync
// access handle (the async createOpfs acquires it; the parity test passes a fake one). Writes the
// from-scratch image in place + flush, then adopts the handle as the open pager so later commits write
// through the seam incrementally (api.md §3, hosts.md §5). toImage validates the page size (0A000 if out
// of range / not a power of two) BEFORE any write, so an invalid page size leaves the handle untouched.
export function createOpfsWithHandle(handle: SyncAccessHandle, opts: DatabaseOptions = {}): Engine {
  const db = new Engine();
  db.pageSize = opts.pageSize ?? DEFAULT_PAGE_SIZE;
  db.committed.txid = 1n; // the initial empty image is committed as txid 1
  db.persistHook = persistImpl; // publish each later commit incrementally (transactions.md §4.1/§9)
  // Lay down the whole from-scratch image in place (write-in-place create — hosts.md §5). toImage seeds
  // both meta slots and validates the page size; every later commit is incremental (persistImpl).
  const bytes = toImage(db.committed, db.pageSize, db.committed.txid);
  handle.write(bytes, { at: 0 });
  handle.flush();
  db.pageCount = Math.floor(bytes.length / db.pageSize);
  // Adopt the just-written handle as the open pager + buffer pool, so later commits write through the
  // seam without re-acquiring (spec/design/pager.md). Pager.fromStore reads the page size from the meta
  // header just written.
  db.paging = new SharedPaging(
    Pager.fromStore(new OpfsBlockStore(handle)),
    cacheLeaves(DEFAULT_CACHE_BYTES, db.pageSize),
  );
  return db;
}

// openOpfsWithHandle opens an existing database over an already-acquired sync access handle. Mirrors
// file.ts open: Pager.fromStore reads the page size from the meta header, the demand-paged loader builds
// the interior skeleton resident and faults leaves through the bounded pool, bounded by cacheBytes
// (pager.md §1, P6.4b). A malformed file is XX001, a read failure 58030 (api.md §2.1). db.path stays
// null (spill disabled for OPFS, see header).
export function openOpfsWithHandle(handle: SyncAccessHandle, opts: OpenOptions = {}): Engine {
  const cacheBytes = opts.cacheBytes ?? DEFAULT_CACHE_BYTES;
  const readOnly = opts.readOnly ?? false;
  const pager = Pager.fromStore(new OpfsBlockStore(handle));
  const db = loadEnginePaged(new SharedPaging(pager, cacheLeaves(cacheBytes, pager.pageSize)));
  db.persistHook = persistImpl; // autocommit each later write (transactions.md §4.1)
  db.readOnly = readOnly;
  if (opts.workMem !== undefined) db.session.workMem = opts.workMem;
  return db;
}

// closeOpfs releases the handle (spec/design/api.md §2.3): roll back any open explicit transaction (its
// in-progress work is discarded), then release the OPFS sync access handle (close()). Under autocommit
// every prior statement is already durable, so close does NOT drop committed work. Idempotent.
export function closeOpfs(db: Engine): void {
  db.rollbackTx();
  if (db.paging !== null) {
    db.paging.close(); // releases the exclusive sync access handle
    db.paging = null;
  }
}

// ---------------------------------------------------------------------------------------------------
// Async acquisition edge (browser / worker only). These reach navigator.storage and are async because
// OPFS handle acquisition is async — a documented per-platform divergence from the synchronous file
// create/open (api.md §6, hosts.md §5). They are NOT callable under Node (opfsStorage throws).
// ---------------------------------------------------------------------------------------------------

// The minimal structural subset of the OPFS acquisition API these wrappers use (a slice of the DOM
// FileSystemDirectoryHandle / FileSystemFileHandle), declared locally so the core stays on @types/node
// without pulling the whole DOM lib. The I/O surface (SyncAccessHandle) lives in opfsblockstore.ts.
interface OpfsFileHandle {
  createSyncAccessHandle(): Promise<SyncAccessHandle>;
}
interface OpfsDirHandle {
  getFileHandle(name: string, opts?: { create?: boolean }): Promise<OpfsFileHandle>;
  removeEntry(name: string, opts?: { recursive?: boolean }): Promise<void>;
}
interface OpfsStorageManager {
  getDirectory(): Promise<OpfsDirHandle>;
}

// opfsStorage returns navigator.storage, reached via globalThis so it compiles under @types/node (which
// already declares a global `navigator`, so a `declare const navigator` here would collide). Throws a
// clear feature_not_supported in any environment without OPFS (e.g. Node) — createOpfs/openOpfs are
// browser/worker only.
function opfsStorage(): OpfsStorageManager {
  // Cast through unknown: under the DOM lib globalThis.navigator is the full Navigator (whose
  // storage.getDirectory returns the real FileSystemDirectoryHandle), which does not overlap our minimal
  // structural OpfsStorageManager — so go through unknown. Works the same under Node/WebWorker libs.
  const nav = (globalThis as unknown as { navigator?: { storage?: OpfsStorageManager } }).navigator;
  if (nav?.storage === undefined) {
    throw engineError(
      "feature_not_supported",
      "OPFS is not available in this environment (browser/worker only)",
    );
  }
  return nav.storage;
}

// createOpfs creates a new OPFS-backed database file named `name` under the origin's private file system
// root. 58P02 duplicate_file if it already exists — create never clobbers (api.md §2.1). Browser/worker
// only. On a mid-create failure (e.g. an invalid page size from createOpfsWithHandle) the half-created
// file is rolled back: the handle is released and the entry removed (write-in-place create — hosts.md §5).
export async function createOpfs(name: string, opts: DatabaseOptions = {}): Promise<Engine> {
  const dir = await opfsStorage().getDirectory();
  // create must not clobber: getFileHandle WITHOUT { create } throwing means the file is absent.
  let exists = true;
  try {
    await dir.getFileHandle(name);
  } catch {
    exists = false;
  }
  if (exists) {
    throw engineError("duplicate_file", "database file already exists: " + name);
  }
  const fileHandle = await dir.getFileHandle(name, { create: true });
  const handle = await fileHandle.createSyncAccessHandle();
  try {
    return createOpfsWithHandle(handle, opts);
  } catch (e) {
    try {
      handle.close();
    } catch {
      // best-effort release
    }
    try {
      await dir.removeEntry(name);
    } catch {
      // best-effort cleanup of the half-created file
    }
    throw e;
  }
}

// openOpfs opens an existing OPFS-backed database file named `name`. 58P01 undefined_file if absent —
// open never creates (api.md §2.1). Browser/worker only. A single exclusive sync access handle is held
// for the database's life (hosts.md §5).
export async function openOpfs(name: string, opts: OpenOptions = {}): Promise<Engine> {
  const dir = await opfsStorage().getDirectory();
  let fileHandle: OpfsFileHandle;
  try {
    fileHandle = await dir.getFileHandle(name);
  } catch {
    throw engineError("undefined_file", "database file does not exist: " + name);
  }
  const handle = await fileHandle.createSyncAccessHandle();
  try {
    return openOpfsWithHandle(handle, opts);
  } catch (e) {
    try {
      handle.close(); // a malformed file / read failure must not leak the handle
    } catch {
      // best-effort release
    }
    throw e;
  }
}
