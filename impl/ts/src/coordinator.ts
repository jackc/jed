// Node file coordination (spec/design/locking.md). Database bytes are never the lock identity:
// persistent empty files in a bundle beside the database carry crash-clean whole-file OS locks.
// presence EX is ownership: a page-service owner (spec/design/page-service.md §4.1), an exclusive
// opener, or a file attachment holds it for its whole tenure; arrival SH guards the protocol-marker
// check.

import {
  lstatSync,
  mkdirSync,
  readdirSync,
  realpathSync,
  statSync,
  type Stats,
  writeFileSync,
} from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import process from "node:process";

import { engineError } from "./errors.ts";
import type { Locking } from "./file.ts";
import type { LockFiles, LockHandle, LockHost } from "./lockhost.ts";
import {
  currentUid,
  endpointPath,
  newIncarnation,
  ownerRecordPath,
  pageServiceSupported,
  readOwnerRecord,
  removeQuietly,
  writeOwnerRecord,
} from "./pageservice/endpoint.ts";
import type { FileCoordinatorHost } from "./session.ts";

// PROTOCOL_MARKER names the bundle's protocol: v2 is presence ownership under the page service.
// LEGACY_PROTOCOL_MARKER (shared mode) may remain in an existing bundle, with its unused lock
// files; a v1 binary rejects the v2 marker, so the two never run on one file (page-service.md §12).
const PROTOCOL_MARKER = "protocol-v2";
const LEGACY_PROTOCOL_MARKER = "protocol-v1";
const LOCK_NAMES = ["presence", "arrival"] as const;
const DEFAULT_FILE_LOCK_TIMEOUT_MS = 5000;
const RETRY_MS = 2;
const waitCell = new Int32Array(new SharedArrayBuffer(4));
const require = createRequire(import.meta.url);

interface NativeLockHandle {
  tryLockShared(): boolean;
  tryLockExclusive(): boolean;
  unlock(): void;
  close(): void;
}

interface NativeBinding {
  abiVersion(): number;
  NativeLockFile: { open(path: string): NativeLockHandle };
}

let nativeBinding: NativeBinding | null | undefined;

function native(): NativeBinding {
  if (nativeBinding === undefined) {
    try {
      nativeBinding = require("../jed_lock.node") as NativeBinding;
      if (nativeBinding.abiVersion() !== 1) nativeBinding = null;
    } catch {
      nativeBinding = null;
    }
  }
  if (nativeBinding === null) {
    throw engineError(
      "feature_not_supported",
      "Node file locking needs the matching first-party jed_lock native artifact",
    );
  }
  return nativeBinding;
}

const openPaths = new Set<string>();

function errno(error: unknown): string | undefined {
  return typeof error === "object" && error !== null && "code" in error
    ? String((error as { code: unknown }).code)
    : undefined;
}

function ioError(operation: string, error: unknown): never {
  const message = error instanceof Error ? error.message : String(error);
  if (message.startsWith("UNSUPPORTED:")) {
    throw engineError("feature_not_supported", `OS file locks are unavailable: ${message}`);
  }
  throw engineError("io_error", `${operation}: ${message}`);
}

function sleep(milliseconds: number): void {
  Atomics.wait(waitCell, 0, 0, milliseconds);
}

function ensureRegular(path: string): void {
  try {
    writeFileSync(path, "", { flag: "wx", mode: 0o644 });
  } catch (error) {
    if (errno(error) !== "EEXIST") ioError("create coordination entry", error);
  }
  let info: Stats;
  try {
    info = lstatSync(path);
  } catch (error) {
    ioError("inspect coordination entry", error);
  }
  if (!info.isFile() || info.isSymbolicLink()) {
    throw engineError("io_error", `coordination entry is not a regular file: ${path}`);
  }
}

function validateProtocol(path: string): void {
  const bundle = `${path}.lock`;
  let entries: string[];
  try {
    entries = readdirSync(bundle);
  } catch (error) {
    ioError("read coordination directory", error);
  }
  for (const entry of entries) {
    if (
      entry.startsWith("protocol-v") &&
      entry !== PROTOCOL_MARKER &&
      entry !== LEGACY_PROTOCOL_MARKER
    ) {
      throw engineError(
        "feature_not_supported",
        `unsupported coordination protocol marker ${entry}; supported: ${PROTOCOL_MARKER}`,
      );
    }
  }
  ensureRegular(join(bundle, PROTOCOL_MARKER));
}

function openBundle(path: string): LockFiles {
  const bundle = `${path}.lock`;
  try {
    mkdirSync(bundle, { mode: 0o755 });
  } catch (error) {
    if (errno(error) !== "EEXIST") ioError("create coordination directory", error);
  }
  let info: Stats;
  try {
    info = lstatSync(bundle);
  } catch (error) {
    ioError("inspect coordination directory", error);
  }
  if (!info.isDirectory() || info.isSymbolicLink()) {
    throw engineError("io_error", `coordination path is not a directory: ${bundle}`);
  }
  validateProtocol(path);
  for (const name of LOCK_NAMES) ensureRegular(join(bundle, name));

  const opened: Partial<Record<(typeof LOCK_NAMES)[number], NativeLockHandle>> = {};
  try {
    for (const name of LOCK_NAMES) {
      opened[name] = native().NativeLockFile.open(join(bundle, name));
    }
  } catch (error) {
    for (const file of Object.values(opened)) file.close();
    ioError("open coordination entry", error);
  }
  return opened as LockFiles;
}

// resolvedLocking resolves a locking mode: "auto" is the page service where the host can serve
// (page-service.md §14), otherwise a non-serving owner ("exclusive"). Unsupported OS locks fail
// 0A000 when the bundle's native locks are first loaded.
export function resolvedLocking(mode: Locking | undefined): Locking {
  const value = mode ?? "auto";
  if (value === "auto") return pageServiceSupported() ? "auto" : "exclusive";
  if (value === "exclusive" || value === "none") return value;
  throw engineError("feature_not_supported", `unknown file locking mode: ${String(value)}`);
}

export function timeoutValue(value: number | undefined): number {
  const timeout = value ?? DEFAULT_FILE_LOCK_TIMEOUT_MS;
  if (!Number.isSafeInteger(timeout) || timeout < 0) {
    throw engineError(
      "numeric_value_out_of_range",
      "fileLockTimeoutMs must be a nonnegative integer",
    );
  }
  return timeout;
}

// osLocks is the production coordination host: the native whole-file locks, the monotonic clock,
// a RETRY_MS sleep between polls, and the owner record and endpoint on the filesystem. It has no
// pageNet: the page service runs on worker threads over OS sockets.
export const osLocks: LockHost = {
  claimPath(path: string): boolean {
    if (openPaths.has(path)) return false;
    openPaths.add(path);
    return true;
  },
  releasePath(path: string): void {
    openPaths.delete(path);
  },
  openBundle,
  validateMarker: validateProtocol,
  closeFiles(files: LockFiles): void {
    for (const file of Object.values(files)) (file as NativeLockHandle).close();
  },
  tryLock(handle: LockHandle, exclusive: boolean): boolean {
    const file = handle as NativeLockHandle;
    try {
      return exclusive ? file.tryLockExclusive() : file.tryLockShared();
    } catch (error) {
      ioError("file lock", error);
    }
  },
  unlock(handle: LockHandle): void {
    try {
      (handle as NativeLockHandle).unlock();
    } catch (error) {
      ioError("file unlock", error);
    }
  },
  nowMicros(): number {
    return performance.now() * 1000;
  },
  pause(): void {
    sleep(RETRY_MS);
  },
  pid(): number {
    return process.pid;
  },
  readOwnerRecord,
  writeOwnerRecord,
  removeOwnerRecord(dbPath: string): void {
    removeQuietly(ownerRecordPath(dbPath));
  },
  endpointPath(dbPath: string, uid: number, incarnation: Uint8Array): string {
    return endpointPath(dbPath, uid, incarnation);
  },
  removeEndpoint: removeQuietly,
};

// lockUntil polls handle until it is locked or the deadline (microseconds on host's clock; null is
// none) passes.
export function lockUntil(
  host: LockHost,
  handle: LockHandle,
  exclusive: boolean,
  deadline: number | null,
  path: string,
  writer: boolean,
): void {
  for (;;) {
    if (host.tryLock(handle, exclusive)) return;
    if (deadline !== null && host.nowMicros() >= deadline) {
      if (writer) {
        throw engineError("lock_not_available", "could not obtain the database writer lock");
      }
      throw engineError("object_in_use", `database file is in use: ${path}`);
    }
    host.pause();
  }
}

export function canonicalOpenPath(path: string): string {
  let real: string;
  try {
    real = realpathSync(path);
  } catch (error) {
    if (errno(error) === "ENOENT") {
      throw engineError("undefined_file", `database file does not exist: ${path}`);
    }
    ioError("resolve database path", error);
  }
  try {
    if (statSync(real).nlink !== 1) {
      throw engineError(
        "object_in_use",
        `hard-linked database paths cannot use jed locking: ${real}`,
      );
    }
  } catch (error) {
    if (error instanceof Error && error.name === "EngineError") throw error;
    ioError("inspect database identity", error);
  }
  return real;
}

function canonicalCreatePath(path: string): string {
  try {
    return join(realpathSync(dirname(path)), path.split(/[\\/]/).at(-1)!);
  } catch (error) {
    ioError("resolve database parent", error);
  }
}

export class FileCoordinator implements FileCoordinatorHost {
  readonly path: string;
  // serving is true for a page-service owner's coordinator, false for a non-serving owner (an
  // exclusive opener or a file attachment).
  readonly serving: boolean;
  private readonly host: LockHost;
  private readonly files: LockFiles;
  private readonly openerPid: number;
  private closed = false;
  // keepPath leaves the process-local path claim at close: a page-service owner's claim belongs to its
  // handle, which keeps the path through role changes (spec/pageservice/README.md §6.5).
  private keepPath = false;
  // record is set when this coordinator wrote a non-serving owner record, removed at close
  // (spec/pageservice/README.md §5.1).
  private record = false;

  private constructor(host: LockHost, path: string, files: LockFiles, serving: boolean) {
    this.host = host;
    this.path = path;
    this.files = files;
    this.serving = serving;
    this.openerPid = host.pid();
  }

  // open takes ownership of the existing database at path as a non-serving owner ("exclusive");
  // "auto" opens go through the page-service election instead (file.ts).
  static open(path: string, mode?: Locking, fileLockTimeoutMs?: number): FileCoordinator | null {
    const locking = resolvedLocking(mode);
    if (locking === "none") return null;
    return FileCoordinator.acquire(osLocks, canonicalOpenPath(path), locking, fileLockTimeoutMs);
  }

  // create takes ownership of the database about to be created at path: a serving owner under
  // "auto", a non-serving owner under "exclusive".
  static create(path: string, mode?: Locking, fileLockTimeoutMs?: number): FileCoordinator | null {
    const locking = resolvedLocking(mode);
    if (locking === "none") return null;
    return FileCoordinator.acquire(osLocks, canonicalCreatePath(path), locking, fileLockTimeoutMs);
  }

  // acquire takes presence EX for the (already canonical) path through host with the exclusive
  // acquisition loop — arrival SH while the marker is checked, then presence EX until the open
  // timeout (55006). Mode "exclusive" (an exclusive opener or a file attachment) writes a
  // non-serving owner record, so a page-service opener words its timeout as "owned by a non-serving
  // process"; mode "auto" is a page-service creator, which writes its own record when it serves.
  static acquire(
    host: LockHost,
    path: string,
    mode: Locking,
    timeoutOption?: number,
  ): FileCoordinator {
    if (!host.claimPath(path)) {
      throw engineError(
        "object_in_use",
        `database is already open in this process: ${path} (share one Database handle)`,
      );
    }
    let files: LockFiles | null = null;
    try {
      files = host.openBundle(path);
      const deadline = host.nowMicros() + timeoutValue(timeoutOption) * 1000;
      lockUntil(host, files.arrival, false, deadline, path, false);
      try {
        host.validateMarker(path);
        try {
          lockUntil(host, files.presence, true, deadline, path, false);
        } catch (error) {
          const reason = ownerReason(host, path);
          if (reason === "") throw error;
          throw engineError("object_in_use", `database file is in use: ${path} (${reason})`);
        }
      } finally {
        host.unlock(files.arrival);
      }
      const coordinator = new FileCoordinator(host, path, files, mode !== "exclusive");
      if (mode === "exclusive") coordinator.writeRecord();
      else coordinator.keepPath = true;
      return coordinator;
    } catch (error) {
      if (files !== null) host.closeFiles(files);
      host.releasePath(path);
      throw error;
    }
  }

  // fromElection wraps the bundle an elected page-service owner holds (presence EX, arrival
  // released) (spec/pageservice/README.md §6.1).
  static fromElection(host: LockHost, path: string, files: LockFiles): FileCoordinator {
    const coordinator = new FileCoordinator(host, path, files, true);
    coordinator.keepPath = true;
    return coordinator;
  }

  // writeRecord publishes a non-serving owner record (flags 0, empty endpoint; README §5.1). It is
  // advisory — only the words of another opener's timeout depend on it — so a failure is ignored.
  private writeRecord(): void {
    try {
      this.host.writeOwnerRecord(this.path, {
        serving: false,
        uid: currentUid(),
        incarnation: newIncarnation(),
        pid: process.pid,
        endpoint: "",
      });
      this.record = true;
    } catch {
      // Advisory only.
    }
  }

  checkPid(): void {
    if (this.host.pid() !== this.openerPid) {
      throw engineError(
        "object_in_use",
        "database handle was inherited across fork; reopen it in the child",
      );
    }
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    if (this.record) this.host.removeOwnerRecord(this.path);
    this.host.closeFiles(this.files);
    if (!this.keepPath) this.host.releasePath(this.path);
  }
}

// ownerReason words why presence EX is held elsewhere, from the owner record: another user's
// process or a non-serving one ("" when the record is absent or names a serving owner of this
// user).
function ownerReason(host: LockHost, path: string): string {
  const record = host.readOwnerRecord(path);
  if (record === null) return "";
  if (record.uid !== currentUid()) return "owned by another user's process";
  if (!record.serving) return "owned by a non-serving process";
  return "";
}
