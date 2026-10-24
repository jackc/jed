// Host file layer for the TS core (spec/design/api.md §2): open/create/commit/close a single-file
// database durably on the Node `fs` host. Isolated here so the future browser/OPFS host is a sibling,
// not a reshape (storage.md §2). create lays down the from-scratch image (temp-file + fsync + atomic
// rename + directory fsync, api.md §3); every later commit is an incremental copy-on-write write of
// just the dirty pages, published by alternating the meta slot (spec/fileformat/format.md, P6.1 part
// B) — the block seam below pwrites pages (writeSync at a position) into the open file.

import "./crc32_node.ts";
import "./crypto_node.ts";

import {
  chmodSync,
  closeSync,
  existsSync,
  fsyncSync,
  openSync,
  realpathSync,
  renameSync,
  statSync,
  unlinkSync,
  writeSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname } from "node:path";

import { type BlockStore, ClosedBlockStore } from "./blockstore.ts";
import { FileBlockStore } from "./fileblockstore.ts";
import { DEFAULT_PAGE_SIZE, Engine, type Snapshot } from "./executor.ts";
import { EngineError, engineError } from "./errors.ts";
import { Environment } from "./environment.ts";
import type { FileHost, WriteAt } from "./filehost.ts";
import { capFirstBody, capOf } from "./cap.ts";
import {
  type Argon2Params,
  createCodec,
  type Credential,
  DEFAULT_ARGON2_PARAMS,
  ENCRYPTED_MIN_PAGE_SIZE,
  type PageCodec,
  sealScratch,
} from "./crypto.ts";
import {
  adoptHeldCodecSteps,
  heldKeyStaleSteps,
  loadEnginePagedSteps,
  toImage,
  unlockPagerSteps,
  writeImage,
} from "./format.ts";
import { runSync, type Steps } from "./execution.ts";
import { cacheLeaves, DEFAULT_CACHE_BYTES, SharedPaging } from "./paging.ts";
import { Pager, type PageTransform } from "./pager.ts";
import { persistImpl, registerDirectorySync, repairKeyBlockSteps } from "./persist.ts";
import {
  type AttachSource,
  buildInMemory,
  Database,
  type OpenedAttachment,
  registerFileAttachOpener,
  registerFileCompactor,
  type RekeyTarget,
  type ServiceHandoffHost,
} from "./shared.ts";
import { FileSpillSink } from "./spillfile.ts";
import type { SpillSink } from "./spill.ts";
import { canonicalOpenPath, FileCoordinator, resolvedLocking } from "./coordinator.ts";
import { type HandoffHosts, type Ownership, ServiceHandoff } from "./pageservice/handoff.ts";
import { RemoteLockHost } from "./pageservice/remote.ts";

// DatabaseOptions are the settings for a newly-created database file (spec/design/api.md §2).
// pageSize is fixed into the file's meta at creation and cannot change thereafter.
// noSync is the fsync=off host setting (api.md §2.1): a commit writes identical bytes in the same order
// but skips the fdatasync barrier. Unlike pageSize it is NOT fixed into the file — it is a runtime handle
// setting. DEV/TESTING only (durable across a process crash, not an OS crash). environment is the
// handle's Environment (spec/design/environment.md); absent ⇒ a fresh empty one (§7).
// credential creates the file encrypted (spec/fileformat/encrypted.md) and kdf stretches a passphrase
// credential (CreateOptions below).
export type DatabaseOptions = {
  pageSize?: number;
  noSync?: boolean;
  environment?: Environment;
  credential?: Credential;
  kdf?: Argon2Params;
  scratchEncryption?: ScratchEncryption;
};

// ScratchEncryption is whether a handle seals its scratch files — sort runs, hash partitions, row
// spools, spilled temp domains (spec/fileformat/encrypted.md §11, api.md): "auto" (the default) seals
// them when the main database is encrypted; "on" always does, so the handle may attach encrypted
// databases while keeping its scratch target. Fixed at open/create for the handle's life.
export type ScratchEncryption = "auto" | "on";

// scratchSink is a file handle's scratch target: the OS temp directory, sealed when encrypt
// (encrypted.md §11) with keys from a scratch stream of environment's key entropy.
function scratchSink(encrypt: boolean, environment: Environment): SpillSink {
  return sealScratch(new FileSpillSink(tmpdir()), encrypt, environment)!;
}

// CreateOptions are the settings for creating a fresh database (spec/design/api.md §2.1/§2.1.1).
// path selects the backing: absent → an in-memory database (never touches the filesystem); present
// → a single file at that path (58P02 if it already exists). It is a genuine optional, not an
// overloaded empty-string sentinel (api.md §2.1). pageSize (absent/0 → DEFAULT_PAGE_SIZE) is locked
// into a file's meta at creation and fixes an in-memory database's tree fan-out (the page-backed
// B-tree's fan-out tracks the page size), so it is meaningful for both backings.
// skipFsync turns off the per-commit fsync for this handle (the fsync=off host setting, api.md
// §2.1): commits write identical bytes in the same order but skip the fdatasync barrier.
// DEV/TESTING ONLY — durable across a process crash, not an OS crash / power loss. Ignored for an
// in-memory database (no path) which never fsyncs. Byte/cost/result-neutral; default false.
// Locking selects how a file database coordinates with other processes (spec/design/locking.md,
// page-service.md): "auto" (the default) is the page service — the opener that takes presence EX
// owns the file and serves it, every other opener is its page client (read-only exactly when the
// handle is opened read-only) — where the host can serve (Unix), and a non-serving owner elsewhere;
// "exclusive" is a non-serving owner (a second opener waits, then fails 55006); "none" skips
// coordination (an unsafe escape hatch).
export type Locking = "auto" | "exclusive" | "none";

// Synchronous is the durability mode of a file-backed handle (spec/design/transactions.md §9, api.md
// §2.1): "on" (the default) makes every commit durable before it returns; "off" acknowledges a commit
// once its pages and meta are written and batches the barrier — a later flush (the batch cap, close,
// …) makes a run of commits durable together. A power loss or OS crash can lose the commits
// acknowledged since the last flush, never the file's integrity (spec/design/validated-cow.md §4). A
// handle setting, not stored in the file; the handle's file attachments follow it. Ignored in memory.
export type Synchronous = "on" | "off";

export type CreateOptions = {
  path?: string;
  pageSize?: number;
  skipFsync?: boolean;
  synchronous?: Synchronous;
  locking?: Locking;
  fileLockTimeoutMs?: number;
  // idleInTransactionTimeoutMs, idleSessionTimeoutMs, yieldGraceMs, shutdownGraceMs, maxConnections,
  // and ownership are the OpenOptions fields of the same names, for a creator under the page service:
  // it is the owner, whatever its ownership preference, and keeps them for later roles.
  idleInTransactionTimeoutMs?: number;
  idleSessionTimeoutMs?: number;
  yieldGraceMs?: number;
  shutdownGraceMs?: number;
  maxConnections?: number;
  ownership?: Ownership;
  // The handle's Environment (spec/design/environment.md): its loaded Unicode and time-zone data,
  // host functions, and random + clock sources, fixed for the handle's lifetime and shared by every
  // session, attachment, and temp domain. Not stored in the file — a reopening host supplies its own.
  // Absent ⇒ a fresh empty environment (§7), never a process-wide default.
  environment?: Environment;
  // The committed-storage limit in bytes (spec/design/memory.md §8): a commit that would grow the
  // database past it fails 54P06. An in-memory database measures its page high-water (pageCount ×
  // pageSize, §8.2), a file-backed one its live pages (livePages × pageSize, §8.7). Zero, negative, or
  // absent is unlimited (the default). Not stored anywhere; Database.setMaxStorageBytes changes it.
  maxStorageBytes?: bigint;
  // credential creates the file ENCRYPTED (spec/design/encryption.md, spec/fileformat/encrypted.md):
  // its pages are sealed under a random data key that only the credential — a passphrase or a 32-byte
  // host key — unwraps. Needs a pageSize of at least 1024 (0A000 otherwise); an in-memory database
  // with a credential is 55000. An encrypted file is served by the page service like a plaintext one
  // (every process holds the key, encrypted.md §14) and seals its scratch files (§11). Absent (the
  // default) creates a plaintext file.
  credential?: Credential;
  // kdf is the Argon2id parameters a passphrase credential is stretched with (default RFC 9106's second
  // recommendation; 22023 outside the format's bounds). Ignored for a host key or a plaintext file.
  kdf?: Argon2Params;
  // scratchEncryption is whether the handle seals its scratch files (ScratchEncryption above): "auto"
  // (the default) seals them when the database is encrypted.
  scratchEncryption?: ScratchEncryption;
};

// create makes a new file-backed database at path with opts (the page size is locked into the
// file). The path must not already exist — 58P02 otherwise. An initial empty image is written
// durably immediately, so the file exists with its page size fixed (api.md §2).
export function create(path: string, opts: DatabaseOptions = {}): Engine {
  return createVia(osFiles, path, opts);
}

// createVia is create through the file host files (filehost.ts): the existence check, the temp file,
// its rename, the directory barrier, and the open all go through it.
export function createVia(files: FileHost, path: string, opts: DatabaseOptions = {}): Engine {
  const pageSize = opts.pageSize ?? DEFAULT_PAGE_SIZE;
  if (opts.credential !== undefined && pageSize < ENCRYPTED_MIN_PAGE_SIZE) {
    throw engineError(
      "feature_not_supported",
      "an encrypted database needs a page size of at least 1024",
    );
  }
  if (files.exists(path)) {
    throw engineError("duplicate_file", "database file already exists: " + path);
  }
  const environment = opts.environment ?? new Environment();
  // A new encrypted file's key block and codec (spec/fileformat/encrypted.md §5, §8): the data key,
  // KDF salt and wrap nonce are the first draws of the database's key-entropy stream.
  const codec =
    opts.credential === undefined
      ? null
      : createCodec(opts.credential, opts.kdf ?? DEFAULT_ARGON2_PARAMS, environment.keyStream());
  const db = new Engine(environment);
  db.path = path;
  db.pageSize = pageSize;
  db.committed.txid = 1n; // the initial empty image is committed as txid 1
  db.persistHook = persistImpl; // publish each later commit incrementally (transactions.md §4.1/§9)
  const noSync = opts.noSync ?? false;
  writeFullImage(files, db, noSync, codec); // lay down the from-scratch image; later commits are incremental
  // Adopt the just-written file as the open pager + buffer pool, so later commits write through the
  // seam without re-opening (spec/design/pager.md). Tables built in this session bind this pager at
  // creation (Snapshot.storePaging), so their committed leaves demote at each commit and fault back
  // through the pool — same residency shape as after a reopen.
  const store = files.openStore(path, noSync, false);
  let pager: Pager;
  try {
    pager = Pager.fromStore(store); // valid header
  } catch (e) {
    store.close();
    throw e;
  }
  db.paging = new SharedPaging(pager, cacheLeaves(DEFAULT_CACHE_BYTES, db.pageSize), codec);
  // Both slots carry the bootstrap txid, so the floor slot is txid & 1 (format.md, *Slot alternation*).
  db.paging.setFloorSlot(Number(db.committed.txid & 1n));
  db.committed.storePaging = db.paging;
  // Scratch is independent of the persistence path: a read-only database directory must still be
  // able to serve a bounded ORDER BY (spill.md §4, api.md §2.1). An encrypted database never spills
  // plaintext: its scratch files are sealed (encrypted.md §11).
  db.spillSink = scratchSink(codec !== null || opts.scratchEncryption === "on", environment);
  return db;
}

// writeFullImage lays down the whole from-scratch image of the committed snapshot (the all-dirty
// special case — spec/fileformat/format.md) durably via temp-file + rename, and records the on-disk
// page high-water. Used by create to establish a fresh file with both meta slots seeded; every later
// commit is incremental (persistImpl).
function writeFullImage(
  files: FileHost,
  db: Engine,
  noSync: boolean,
  codec: PageCodec | null,
): void {
  if (db.path === null) return;
  const bytes = toImage(db.committed, db.pageSize, db.committed.txid, codec);
  writeAtomic(files, db.path, bytes, noSync);
  db.pageCount = Math.floor(bytes.length / db.pageSize);
  // The image is of an empty database: every body page is live, and every one is a catalog page
  // (spec/design/memory.md §8.7).
  const body = db.pageCount - capFirstBody(capOf(db.pageSize, codec !== null));
  db.live = { live: body, catalog: body, gist: 0 };
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
// never changes what a query observes (spill.md §6). 0 (or unset) ⇒ the default (256 MiB), NOT
// unlimited — the zero value stays a safe finite budget (matching Go/Rust); an unbounded/never-spill
// budget is a runtime-only mode via setWorkMem(0). Default DEFAULT_WORK_MEM (256 MiB).
// skipFsync turns off the per-commit fsync (the fsync=off host setting, api.md §2.1): a commit still writes
// the same bytes in the same order but the fdatasync barrier becomes a no-op — much faster. DEV/TESTING
// ONLY: the data survives a process crash (the OS page cache still flushes) but NOT an OS crash / power
// loss. Never changes what a query observes or the on-disk bytes; default false.
export type OpenOptions = {
  cacheBytes?: number;
  readOnly?: boolean;
  workMem?: number;
  skipFsync?: boolean;
  // synchronous is the durability mode (Synchronous above; transactions.md §9). Default "on".
  synchronous?: Synchronous;
  locking?: Locking;
  fileLockTimeoutMs?: number;
  // idleInTransactionTimeoutMs is a page-service owner's idle-in-transaction timeout for client write
  // transactions (spec/design/page-service.md §11.1): a client holding the writer gate that sends
  // nothing for this long is revoked (25P03 at its next request). Absent is the 60 s default; 0
  // disables it. Only the page service (`locking = "auto"`) reads it.
  idleInTransactionTimeoutMs?: number;
  // idleSessionTimeoutMs closes a page-service connection idle between transactions for this long
  // (page-service.md §11.1); absent or 0 never does. yieldGraceMs and shutdownGraceMs are how long
  // a yielding or closing owner lets running transactions finish (absent: 5 s and 30 s).
  // maxConnections bounds an owner's connections (53300 beyond it); absent or 0 is unlimited. Only
  // the page service (`locking = "auto"`) reads them, as an owner's limits; a client keeps them for
  // the day it becomes owner.
  idleSessionTimeoutMs?: number;
  yieldGraceMs?: number;
  shutdownGraceMs?: number;
  maxConnections?: number;
  // ownership is the opener's ownership preference under the page service (page-service.md §8.2):
  // "prefer" (the default) becomes owner when the database is free and asks a yielding owner to
  // yield; "yield" hands ownership to a prefer requester (for tools); "never" never owns (an open with
  // no owner fails 55006, and a client never promotes). Only the page service (`locking = "auto"`)
  // reads it.
  ownership?: Ownership;
  // The handle's Environment (spec/design/environment.md): the file's referenced collations, zones,
  // and host functions resolve against it at open, and it supplies the random + clock sources. A
  // handle setting like the rest — not stored in the file. Absent ⇒ a fresh empty environment (§7).
  environment?: Environment;
  // The committed-storage limit in bytes over the file's live pages (spec/design/memory.md §8.7): a
  // commit that would grow livePages × pageSize past it fails 54P06. Zero, negative, or absent is
  // unlimited (the default). A handle setting, not stored in the file; Database.setMaxStorageBytes
  // changes it.
  maxStorageBytes?: bigint;
  // credential unlocks an encrypted file (spec/design/encryption.md §5): required for one (28P02
  // without it, 28P01 when it does not unwrap the key), refused for a plaintext file (28P03). A handle
  // argument only — never stored, never shown.
  credential?: Credential;
  // scratchEncryption is whether the handle seals its scratch files (ScratchEncryption above): "auto"
  // (the default) seals them when the database is encrypted.
  scratchEncryption?: ScratchEncryption;
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
  return openVia(osFiles, path, opts);
}

// openWithTransformSteps is open as one continuation with a page codec transform installed — the
// asynchronous-codec conformance mode's opener (ts-runtime.md §11.5). Test tooling only.
export function* openWithTransformSteps(
  path: string,
  opts: OpenOptions,
  transform: PageTransform | null,
): Steps<Engine> {
  return yield* openViaSteps(osFiles, path, opts, transform);
}

// openVia is open through the file host files (filehost.ts). held is the data key a page client
// promoting itself to owner already holds (spec/fileformat/encrypted.md §14): the codec then opens the
// file in place of the credential, adopting the file's selected key-block copy, and a differing pair is
// left unrepaired (a process without the credential cannot know which copy it would be writing).
export function openVia(
  files: FileHost,
  path: string,
  opts: OpenOptions = {},
  held: PageCodec | null = null,
): Engine {
  return runSync(openViaSteps(files, path, opts, null, held));
}

function isXX001(e: unknown): boolean {
  return e instanceof EngineError && e.code() === "XX001";
}

// staleKey is 28P04: the owner rekeyed the database, so the held data key no longer opens it.
function staleKey(): EngineError {
  return engineError(
    "stale_key",
    "the database was rekeyed since this handle opened it; reopen it",
  );
}

// openViaSteps is openVia as one continuation (ts-runtime.md §11.1), with the page codec transform
// installed before any page is read. Without a transform (production) every read runs inline; the
// asynchronous-codec conformance mode drives it with runAsync and an async-only identity transform.
export function* openViaSteps(
  files: FileHost,
  path: string,
  opts: OpenOptions,
  transform: PageTransform | null,
  held: PageCodec | null = null,
): Steps<Engine> {
  const cacheBytes = opts.cacheBytes ?? DEFAULT_CACHE_BYTES;
  const readOnly = opts.readOnly ?? false;
  const store = files.openStore(path, opts.skipFsync ?? false, readOnly);
  try {
    // Read the file's page size first, then convert the byte budget to a leaf-page capacity; the loader
    // rejects an out-of-range page size as corrupt (cacheLeaves clamps the divisor so a malformed
    // page_size = 0 cannot divide by zero before that check runs).
    const pager = yield* Pager.fromStoreSteps(store);
    pager.setPageTransform(transform);
    const environment = opts.environment ?? new Environment();
    let codec: PageCodec | null;
    if (held !== null) {
      yield* adoptHeldCodecSteps(pager, held);
      codec = held;
    } else {
      codec = yield* unlockPagerSteps(pager, opts.credential, environment);
    }
    let db: Engine;
    try {
      db = yield* loadEnginePagedSteps(
        new SharedPaging(pager, cacheLeaves(cacheBytes, pager.pageSize), codec),
        environment,
      );
    } catch (e) {
      // A meta the held key cannot authenticate means the owner rekeyed the file before it left:
      // 28P04, so the host reopens with the credential (spec/fileformat/encrypted.md §14).
      if (held !== null && isXX001(e) && (yield* heldKeyStaleSteps(pager, held))) throw staleKey();
      throw e;
    }
    db.path = path;
    db.persistHook = persistImpl; // autocommit each later write (transactions.md §4.1)
    db.readOnly = readOnly;
    if (!readOnly && held === null) yield* repairKeyBlockSteps(db);
    db.storageBudget.limit = opts.maxStorageBytes ?? 0n;
    db.relaxed = opts.synchronous === "off";
    // Scratch is independent of the persistence path, so read-only database filesystems remain
    // readable when ORDER BY crosses work_mem (spill.md §4, api.md §2.1). An encrypted database seals
    // its scratch files (spec/fileformat/encrypted.md §11).
    db.spillSink = scratchSink(codec !== null || opts.scratchEncryption === "on", environment);
    // 0 (or unset) means "the default budget", not "unlimited" — the zero value stays a safe finite
    // budget (matching Go/Rust). Unbounded/never-spill is a runtime-only mode via setWorkMem(0).
    if (opts.workMem) db.session.workMem = opts.workMem;
    return db;
  } catch (e) {
    store.close(); // a malformed file / read failure must not leak the fd
    if (e instanceof Error && e.name === "EngineError") throw e;
    throw ioError(e);
  }
}

export { repairKeyBlockSteps };

// Register the Node file host as the file-attach opener (attached-databases.md §4, Slice 2): importing
// file.ts (the node host) enables `db.attach(name, attachFile(path))` without shared.ts importing a host
// module (it stays browser-clean). The OPFS host registers its own. Read-only opens the file O_RDONLY.
// The file opens through main's file host (null is the OS), so the simulated filesystem serves
// attachments too.
//
// Under `locking = "auto"` a file attachment joins the page service (page-service.md §5.5): it is
// elected owner or page client of the attached file exactly as an open is, through an inner handle of
// its own (openServiceAttachment). Under "exclusive" it is a non-serving owner of its file: another
// process holding the file makes the attach wait, then fail 55006. Either way it follows the handle's
// synchronous setting (validated-cow.md §4).
registerFileAttachOpener((source, readOnly, files, environment, relaxed, mainHandoff, seals) => {
  const path = source.path!;
  const locking = resolvedLocking(source.locking);
  if (locking === "auto") {
    return openServiceAttachment(
      path,
      source,
      readOnly,
      files,
      environment,
      relaxed,
      mainHandoff,
      seals,
    );
  }
  const coordinator = FileCoordinator.open(path, locking, source.fileLockTimeoutMs);
  try {
    // A file attachment follows the handle's synchronous setting (validated-cow.md §4).
    const engine = openVia(files ?? osFiles, coordinator?.path ?? path, {
      readOnly,
      locking: "none",
      environment,
      synchronous: relaxed ? "off" : "on",
      credential: source.credential,
    });
    return { kind: "file", engine, coordinator };
  } catch (error) {
    coordinator?.close();
    throw error;
  }
});

// openServiceAttachment elects this process owner or page client of the attachment file at path
// (page-service.md §5.5; spec/pageservice/README.md §6.6), through the handle's own hosts: main's
// handoff's lock, file, and client hosts and background flag when main is under the page service (the
// simulator's in a simulation), the OS lock host and main's file host otherwise. The inner handle takes
// main's ownership preference and owner limits when main is under the page service, the defaults
// otherwise.
function openServiceAttachment(
  path: string,
  source: AttachSource,
  readOnly: boolean,
  files: FileHost | null,
  environment: Environment,
  relaxed: boolean,
  mainHandoff: ServiceHandoffHost | null,
  seals: boolean,
): OpenedAttachment {
  const main = mainHandoff instanceof ServiceHandoff ? mainHandoff : null;
  const hosts: HandoffHosts = main !== null ? main.hosts() : { files };
  const hostFiles = hosts.files ?? null;
  const real = hostFiles === null ? canonicalOpenPath(path) : hostFiles.realPath(path);
  const options: OpenOptions = {
    readOnly,
    locking: "auto",
    fileLockTimeoutMs: source.fileLockTimeoutMs,
    environment,
    maxStorageBytes: source.maxStorageBytes,
    // The attachment follows the handle's synchronous setting (validated-cow.md §4).
    synchronous: relaxed ? "off" : "on",
    credential: source.credential,
    // The attachment seals its scratch files when the handle does (encrypted.md §11).
    scratchEncryption: seals ? "on" : "auto",
  };
  if (main !== null) {
    // A handle under the page service lends its ownership preference and owner limits to its
    // attachments (README §6.6).
    options.ownership = main.ownership;
    options.idleInTransactionTimeoutMs = main.limits.idleInTransactionTimeoutMs;
    options.idleSessionTimeoutMs = main.limits.idleSessionTimeoutMs;
    options.yieldGraceMs = main.yieldGraceMs;
    options.shutdownGraceMs = main.shutdownGraceMs;
    options.maxConnections = main.limits.maxConnections;
  }
  const adoptClient = hosts.adoptClient ?? null;
  const handoff = new ServiceHandoff(
    real,
    options,
    readOnly,
    source.fileLockTimeoutMs,
    {
      credential: source.credential,
      scratchEncryption: seals ? "on" : "auto",
    },
    environment,
    (held) => {
      const engine = openVia(hostFiles ?? osFiles, real, { ...options, locking: "none" }, held);
      // An owner's storage reaches the same file and spill hosts a client's does.
      engine.fileHost = hostFiles;
      adoptClient?.(engine);
      return engine;
    },
    hosts,
  );
  const service = openServiceWith(handoff);
  return { kind: "service", service, realPath: real };
}

// compactFile rewrites the file database at path as the from-scratch image of snap at txid and reopens
// it (host compaction, spec/design/api.md §2.6). The caller holds the writer gate with the reader
// watermark drained, so nothing else reads old while its file is swapped.
//
// The image streams page by page into the sibling temp file, which takes the old file's permission bits
// and is synced. Then old's file is closed (a ClosedBlockStore takes its place, so a stale snapshot fails
// closed rather than reading the new layout), the temp file is renamed over the real path, and the
// directory is synced. A failure before the rename leaves the old file and old exactly as they were. If
// the rename fails, the old file is reopened into old. If reopening the new file fails, old stays closed
// and poisoned, so the handle must be reopened. Returns the replacement paging context, a pool of
// capacity leaves over the new file.
//
// rekey, when given, is a rekey's target (spec/fileformat/encrypted.md §12): the image is written under
// its codec (null: plaintext) instead of old's, and an image whose live pages exceed limit bytes (when
// positive) fails 54P06 before the rename, naming the database name.
export function compactFile(
  host: FileHost | null,
  path: string,
  snap: Snapshot,
  pageSize: number,
  txid: bigint,
  old: SharedPaging,
  capacity: number,
  rekey?: RekeyTarget,
): SharedPaging {
  const files = host ?? osFiles;
  const noSync = old.skipsSync();
  // Rename over the real file, never over a symlink naming it (the symlink keeps pointing at it).
  const real = files.realPath(path);
  const tmp = real + ".jedtmp";
  // An encrypted file keeps its data key and key block; every page is resealed under fresh nonces
  // (spec/design/encryption.md §5). A rekey writes under its target codec instead.
  const codec = rekey === undefined ? old.codec() : rekey.codec;
  let pageCount = 0;
  try {
    files.writeFile(tmp, real, noSync, (writeAt) => {
      pageCount = writeImage(
        snap,
        pageSize,
        txid,
        (index, page) => writeAt(index * pageSize, page),
        codec,
      );
    });
  } catch (e) {
    files.remove(tmp);
    throw e;
  }
  if (rekey !== undefined && rekey.limit > 0n) {
    // A from-scratch image holds no free page: every body page is live (memory.md §8.7).
    const live = pageCount - capFirstBody(capOf(pageSize, codec !== null));
    if (BigInt(live) * BigInt(pageSize) > rekey.limit) {
      files.remove(tmp);
      throw engineError(
        "storage_limit_exceeded",
        `storage of database "${rekey.name}" exceeded the limit of ${rekey.limit} bytes`,
      );
    }
  }

  // Close the old file before the rename: Windows cannot replace a file that is still open.
  const closing = old.swapStore(new ClosedBlockStore());
  try {
    closing.close();
  } catch {
    // The descriptor is released either way; a close error loses nothing the rename needs.
  }
  try {
    files.rename(tmp, real);
  } catch (e) {
    files.remove(tmp);
    try {
      old.swapStore(files.openStore(real, noSync, false));
    } catch {
      old.poison();
    }
    throw e;
  }
  old.poison();
  if (!noSync) files.syncDir(dirname(real));
  const store = files.openStore(real, noSync, false);
  try {
    return new SharedPaging(Pager.fromStore(store), capacity, codec);
  } catch (e) {
    store.close();
    throw e;
  }
}

// osFiles is the Node file host (filehost.ts): create's and compaction's temp file, rename, directory
// barrier, and open on the real filesystem. A missing file is 58P01; every other OS error surfaces as
// io_error.
export const osFiles: FileHost = {
  realPath(path: string): string {
    try {
      return realpathSync(path);
    } catch (e) {
      throw ioError(e);
    }
  },
  exists(path: string): boolean {
    try {
      statSync(path);
      return true;
    } catch (e) {
      if (errnoCode(e) === "ENOENT") return false;
      throw ioError(e);
    }
  },
  writeFile(
    path: string,
    modeFrom: string,
    noSync: boolean,
    fill: (writeAt: WriteAt) => void,
  ): void {
    try {
      const fd = openSync(path, "w");
      try {
        fill((offset, bytes) => {
          let done = 0;
          while (done < bytes.length) {
            const count = writeSync(fd, bytes, done, bytes.length - done, offset + done);
            if (count <= 0) throw engineError("io_error", "file write made no progress");
            done += count;
          }
        });
        if (modeFrom !== "") chmodSync(path, statSync(modeFrom).mode & 0o7777);
        if (!noSync) fsyncSync(fd);
      } finally {
        closeSync(fd);
      }
    } catch (e) {
      if (e instanceof Error && e.name === "EngineError") throw e;
      throw ioError(e);
    }
  },
  rename(from: string, to: string): void {
    try {
      renameSync(from, to);
    } catch (e) {
      throw ioError(e);
    }
  },
  remove(path: string): void {
    removeQuietly(path);
  },
  syncDir(dir: string): void {
    // Best-effort, as in writeAtomic: not every platform can fsync a directory.
    try {
      const dfd = openSync(dir, "r");
      try {
        fsyncSync(dfd);
      } finally {
        closeSync(dfd);
      }
    } catch {
      // directory fsync unsupported on this platform — acceptable (api.md §3)
    }
  },
  openStore(path: string, noSync: boolean, readOnly: boolean): BlockStore {
    let fd: number;
    try {
      // A read-only open never writes the file, so it is not opened for writing at all — the OS
      // enforces what the executor's 25006 guards promise (api.md §2.1).
      fd = openSync(path, readOnly ? "r" : "r+");
    } catch (e) {
      if (errnoCode(e) === "ENOENT") {
        throw engineError("undefined_file", "database file does not exist: " + path);
      }
      throw ioError(e);
    }
    return new FileBlockStore(fd, noSync);
  },
};

// Register the Node file host as the file compactor (spec/design/api.md §2.6), the same injection as the
// attach opener above: shared.ts stays browser-clean and reaches compactFile only through this hook.
registerFileCompactor(compactFile);

// The directory barrier the first durable commit after open runs (persist.ts syncDirectoryOnce): the
// file's real directory, through the database's file host (null is the OS).
registerDirectorySync((host, path) => {
  const files = host ?? osFiles;
  files.syncDir(dirname(files.realPath(path)));
});

function removeQuietly(path: string): void {
  try {
    if (existsSync(path)) unlinkSync(path);
  } catch {
    // best-effort cleanup
  }
}

// createDatabase makes a fresh database — in-memory (opts.path absent) or file-backed (opts.path set)
// — and returns the host Database handle with its default session (spec/design/api.md §2.1/§2.1.1). A
// file that already exists is 58P02; the page size is locked into the file. The in-memory path cannot
// fail in substance (it never touches the filesystem) but shares the uniform signature — a caller
// wanting an infallible in-memory handle wraps this (the tests' memDb helper does). This is the one
// create constructor for both backings; the in-memory-specific constructors are removed.
export function createDatabase(opts: CreateOptions = {}): Database {
  const pageSize = opts.pageSize || DEFAULT_PAGE_SIZE;
  const maxStorageBytes = opts.maxStorageBytes ?? 0n;
  if (opts.path === undefined && opts.credential !== undefined) {
    // Nothing is at rest in an in-memory database (spec/design/encryption.md §5).
    throw engineError(
      "object_not_in_prerequisite_state",
      "an in-memory database cannot be encrypted",
    );
  }
  if (opts.path !== undefined) {
    const coordinator = FileCoordinator.create(
      opts.path,
      resolvedLocking(opts.locking),
      opts.fileLockTimeoutMs,
    );
    try {
      const engine = create(coordinator?.path ?? opts.path, {
        pageSize,
        noSync: opts.skipFsync,
        environment: opts.environment,
        credential: opts.credential,
        kdf: opts.kdf,
        scratchEncryption: opts.scratchEncryption,
      });
      engine.storageBudget.limit = maxStorageBytes;
      engine.relaxed = opts.synchronous === "off";
      if (coordinator?.serving === true) {
        // The creator is the owner (README §6.1); its handle keeps the path claim across roles.
        const path = coordinator.path;
        const environment = engine.environment;
        const handoff = new ServiceHandoff(
          path,
          opts,
          false,
          opts.fileLockTimeoutMs,
          { scratchEncryption: opts.scratchEncryption, credential: opts.credential },
          environment,
          (held) =>
            openVia(
              osFiles,
              path,
              {
                skipFsync: opts.skipFsync,
                synchronous: opts.synchronous,
                environment,
                locking: "none",
                scratchEncryption: opts.scratchEncryption,
                credential: opts.credential,
              },
              held,
            ),
        );
        return startServiceOwner(engine, coordinator, handoff);
      }
      return Database.fromEngine(engine, coordinator);
    } catch (error) {
      coordinator?.close();
      throw error;
    }
  }
  // in-memory never fsyncs; skipFsync is a no-op
  return buildInMemory(pageSize, opts.environment ?? new Environment(), maxStorageBytes);
}

// openDatabase opens an existing file-backed database at path with optional open settings and returns
// the host Database handle with its default session (the back-compat bridge, spec/design/session.md §2.4).
export function openDatabase(path: string, opts: OpenOptions = {}): Database {
  const locking = resolvedLocking(opts.locking);
  if (locking === "auto") return openServiceDatabase(path, opts);
  const coordinator = FileCoordinator.open(path, locking, opts.fileLockTimeoutMs);
  try {
    const engine = open(coordinator?.path ?? path, { ...opts, locking: "none" });
    return Database.fromEngine(engine, coordinator);
  } catch (error) {
    coordinator?.close();
    throw error;
  }
}

// openRemoteDatabase opens a database that another machine (or process) serves over a stream: a page
// client whose connection is the standard input and output of command, run once per connection
// (spec/pageservice/README.md §5.3). The CLI opens `ssh://host/path` as
// ["ssh", "--", "host", "jed pages '/path'"]. name identifies the database in this process (one handle
// per name, like a path) and is what the handle reports as its path.
//
// The handle is an ordinary page client: it runs its own engine over pages fetched through the
// command, commits through it, and when the connection ends it runs the command again (re-election,
// README §6.5) — it never becomes the owner, whatever opts.ownership says. opts.credential unlocks an
// encrypted database here and is also sent in every connection's hello, for a serving end that must
// own the database (README §3.1). An open that cannot reach the database in opts.fileLockTimeoutMs
// fails 08006 with the command's standard error; an error the remote side reports (58P01, 0A000, …)
// is thrown as is. Unix only.
export function openRemoteDatabase(
  command: string[],
  name: string,
  opts: OpenOptions = {},
): Database {
  if (command.length === 0) throw engineError("connection_failure", "empty transport command");
  if (process.platform === "win32") {
    throw engineError("feature_not_supported", "remote databases are unavailable on this platform");
  }
  const host = new RemoteLockHost(command, opts.credential ?? null);
  const environment = opts.environment ?? new Environment();
  const options: OpenOptions = { ...opts, ownership: "never", locking: "auto", environment };
  const handoff = new ServiceHandoff(
    name,
    options,
    options.readOnly ?? false,
    options.fileLockTimeoutMs,
    options,
    environment,
    () => {
      throw engineError("object_in_use", "a remote database is owned on the remote side");
    },
    { lock: host },
  );
  try {
    return openServiceWith(handoff);
  } catch (error) {
    throw host.openError(error);
  }
}

// openServiceDatabase opens path under the page service (`locking = "auto"`,
// spec/pageservice/README.md §6.1): as the serving owner when the election takes presence EX,
// otherwise as a page client.
function openServiceDatabase(path: string, opts: OpenOptions): Database {
  const real = canonicalOpenPath(path);
  const environment = opts.environment ?? new Environment();
  const handoff = new ServiceHandoff(
    real,
    opts,
    opts.readOnly ?? false,
    opts.fileLockTimeoutMs,
    opts,
    environment,
    (held) => openVia(osFiles, real, { ...opts, locking: "none", environment }, held),
  );
  return openServiceWith(handoff);
}

// openServiceWith runs handoff's election at open and opens the handle in the role it wins: an
// elected client arrives opened; an elected owner opens the file as an owner does (handoff's opener)
// and serves it. The deterministic simulator opens its processes through it with its own hosts.
export function openServiceWith(handoff: ServiceHandoff): Database {
  const election = handoff.electAtOpen();
  if (election.client !== null) {
    return Database.fromEngine(election.engine, null, { client: election.client, handoff });
  }
  const coordinator = FileCoordinator.fromElection(handoff.host, handoff.path, election.owner);
  let role: ReturnType<ServiceHandoff["startOwner"]>;
  try {
    role = handoff.startOwner(coordinator);
  } catch (error) {
    handoff.releasePath();
    throw error;
  }
  return Database.fromEngine(role.engine!, coordinator, { service: role.service!, handoff });
}

// startServiceOwner lifts an opened or created engine into a serving owner's handle; the caller's
// coordinator holds presence EX. A failure to serve releases presence and the path claim.
function startServiceOwner(
  engine: Engine,
  coordinator: FileCoordinator,
  handoff: ServiceHandoff,
): Database {
  let role: ReturnType<ServiceHandoff["startOwner"]>;
  try {
    role = handoff.startOwner(coordinator, engine);
  } catch (error) {
    handoff.releasePath();
    throw error;
  }
  return Database.fromEngine(engine, coordinator, { service: role.service!, handoff });
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
// work; durability is never hidden in a destructor. The default session's temp domain is dropped too
// (a spilled one's scratch file released — temp-tables.md §6). Idempotent.
export function close(db: Engine): void {
  db.rollbackTx();
  db.releaseTempStorage();
  db.path = null;
  if (db.paging !== null) {
    const paging = db.paging;
    db.paging = null;
    // A relaxed engine's pending commits become durable before the file is released
    // (validated-cow.md §4); a failed flush is what close reports.
    try {
      paging.flushPending();
    } finally {
      paging.close(); // drop the open fd (close it)
    }
  }
}

// writeAtomic writes bytes to path crash-safely through the file host files (spec/design/api.md §3):
// a sibling temp file, fsync, atomic rename over the target, then a best-effort directory fsync so the
// rename is durable. Under noSync (fsync=off) the two fsyncs are skipped — the write + rename still
// happen, but the bytes are only in the OS page cache (dev/testing; no durability on an OS crash).
function writeAtomic(files: FileHost, path: string, bytes: Uint8Array, noSync: boolean): void {
  const tmp = path + ".jedtmp";
  try {
    files.writeFile(tmp, "", noSync, (writeAt) => writeAt(0, bytes));
  } catch (e) {
    files.remove(tmp);
    throw e;
  }
  try {
    files.rename(tmp, path);
  } catch (e) {
    files.remove(tmp);
    throw e;
  }
  // Directory fsync makes the rename itself durable. Best-effort: not every platform allows opening a
  // directory for fsync (Windows), and the rename is already atomic there. Skipped under noSync.
  if (!noSync) files.syncDir(dirname(path));
}

// errnoCode is a Node fs error's code ("ENOENT", ...), or undefined.
function errnoCode(e: unknown): string | undefined {
  return typeof e === "object" && e !== null && "code" in e && typeof e.code === "string"
    ? e.code
    : undefined;
}

function ioError(e: unknown): Error {
  const msg = e instanceof Error ? e.message : String(e);
  return engineError("io_error", "I/O error: " + msg);
}
