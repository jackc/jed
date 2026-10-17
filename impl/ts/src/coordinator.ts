// Stable Node file coordination (spec/design/locking.md). Database bytes are never the lock
// identity: five persistent empty files beside the database carry crash-clean whole-file OS locks.

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
import type { FileCoordinatorHost, LeaseState } from "./session.ts";

const PROTOCOL_MARKER = "protocol-v1";
const LOCK_NAMES = ["presence", "arrival", "transition", "writer", "commit"] as const;
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
      "Node shared file locking needs the matching first-party jed_lock native artifact",
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
    if (entry.startsWith("protocol-v") && entry !== PROTOCOL_MARKER) {
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

export function resolvedLocking(mode: Locking | undefined): Locking {
  const value = mode ?? "auto";
  if (value === "auto") return "shared";
  if (value === "shared" || value === "exclusive" || value === "none") return value;
  if (value === "service") {
    // The test-only page-service mode (spec/design/page-service.md §16): Unix only (README §5.2).
    if (process.platform === "win32" || typeof process.geteuid !== "function") {
      throw engineError(
        "feature_not_supported",
        `the page service is unavailable on ${process.platform}`,
      );
    }
    return value;
  }
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

// osLocks is the production coordination host: the native whole-file locks, the monotonic clock, a
// RETRY_MS sleep between polls, and a one-second unref'd probe timer.
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
  startProbe(tick: () => void): () => void {
    const probe = setInterval(tick, 1000);
    probe.unref();
    return () => clearInterval(probe);
  },
  pid(): number {
    return process.pid;
  },
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
  state: LeaseState;
  private readonly host: LockHost;
  private readonly files: LockFiles;
  private readonly openerPid: number;
  private stopProbe: (() => void) | null = null;
  private closed = false;

  private constructor(host: LockHost, path: string, files: LockFiles, state: LeaseState) {
    this.host = host;
    this.path = path;
    this.files = files;
    this.state = state;
    this.openerPid = host.pid();
  }

  static open(path: string, mode?: Locking, fileLockTimeoutMs?: number): FileCoordinator | null {
    const locking = resolvedLocking(mode);
    if (locking === "none") return null;
    return FileCoordinator.acquire(osLocks, canonicalOpenPath(path), locking, fileLockTimeoutMs);
  }

  static create(path: string, mode?: Locking, fileLockTimeoutMs?: number): FileCoordinator | null {
    const locking = resolvedLocking(mode);
    if (locking === "none") return null;
    return FileCoordinator.acquire(osLocks, canonicalCreatePath(path), locking, fileLockTimeoutMs);
  }

  // acquire joins the coordination protocol for the (already canonical) path through host: arrival
  // SH, then presence EX (alone or exclusive) or SH (shared) under transition EX (locking.md §4).
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
      host.validateMarker(path);
      let state: LeaseState;
      // Creating under the page service takes presence EX like exclusive: the creator is the owner.
      if (mode === "exclusive" || mode === "service") {
        for (;;) {
          lockUntil(host, files.transition, true, deadline, path, false);
          const alone = host.tryLock(files.presence, true);
          host.unlock(files.transition);
          if (alone) {
            state = "exclusive";
            break;
          }
          if (host.nowMicros() >= deadline) {
            throw engineError("object_in_use", `database file is in use: ${path}`);
          }
          host.pause();
        }
      } else {
        lockUntil(host, files.transition, true, deadline, path, false);
        const alone = host.tryLock(files.presence, true);
        host.unlock(files.transition);
        if (alone) state = "alone";
        else {
          lockUntil(host, files.presence, false, deadline, path, false);
          state = "shared";
        }
      }
      host.unlock(files.arrival);
      return new FileCoordinator(host, path, files, state);
    } catch (error) {
      if (files !== null) host.closeFiles(files);
      host.releasePath(path);
      throw error;
    }
  }

  // fromElection wraps the bundle an elected page-service owner holds (presence EX, arrival released):
  // it behaves as an exclusive lease — no join-window probe (spec/pageservice/README.md §6.1).
  static fromElection(host: LockHost, path: string, files: LockFiles): FileCoordinator {
    return new FileCoordinator(host, path, files, "exclusive");
  }

  checkPid(): void {
    if (this.host.pid() !== this.openerPid) {
      throw engineError(
        "object_in_use",
        "database handle was inherited across fork; reopen it in the child",
      );
    }
  }

  startProbe(tick: () => void): void {
    if (this.state === "exclusive" || this.stopProbe !== null) return;
    this.stopProbe = this.host.startProbe(tick);
  }

  lockCommitShared(): void {
    lockUntil(this.host, this.files.commit, false, null, this.path, false);
  }

  lockCommitExclusive(): void {
    lockUntil(this.host, this.files.commit, true, null, this.path, false);
  }

  unlockCommit(): void {
    this.host.unlock(this.files.commit);
  }

  lockWriter(timeoutMs: number): void {
    const deadline = timeoutMs === 0 ? null : this.host.nowMicros() + timeoutMs * 1000;
    lockUntil(this.host, this.files.writer, true, deadline, this.path, true);
  }

  unlockWriter(): void {
    this.host.unlock(this.files.writer);
  }

  tryArrivalExclusive(): boolean {
    return this.host.tryLock(this.files.arrival, true);
  }

  unlockArrival(): void {
    this.host.unlock(this.files.arrival);
  }

  lockTransition(): void {
    lockUntil(this.host, this.files.transition, true, null, this.path, false);
  }

  unlockTransition(): void {
    this.host.unlock(this.files.transition);
  }

  downgradePresence(): void {
    this.state = "shared";
    this.host.unlock(this.files.presence);
    lockUntil(this.host, this.files.presence, false, null, this.path, false);
  }

  tryUpgradePresence(): boolean {
    this.host.unlock(this.files.presence);
    if (this.host.tryLock(this.files.presence, true)) return true;
    lockUntil(this.host, this.files.presence, false, null, this.path, false);
    return false;
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    if (this.stopProbe !== null) this.stopProbe();
    this.stopProbe = null;
    this.host.closeFiles(this.files);
    this.host.releasePath(this.path);
  }
}
