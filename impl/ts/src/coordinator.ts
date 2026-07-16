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

type LockFiles = Record<(typeof LOCK_NAMES)[number], NativeLockHandle>;

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

  const opened: Partial<LockFiles> = {};
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

function resolvedLocking(mode: Locking | undefined): Locking {
  const value = mode ?? "auto";
  if (value === "auto") return "shared";
  if (value === "shared" || value === "exclusive" || value === "none") return value;
  throw engineError("feature_not_supported", `unknown file locking mode: ${String(value)}`);
}

function timeoutValue(value: number | undefined): number {
  const timeout = value ?? DEFAULT_FILE_LOCK_TIMEOUT_MS;
  if (!Number.isSafeInteger(timeout) || timeout < 0) {
    throw engineError(
      "numeric_value_out_of_range",
      "fileLockTimeoutMs must be a nonnegative integer",
    );
  }
  return timeout;
}

function tryLock(file: NativeLockHandle, exclusive: boolean): boolean {
  try {
    return exclusive ? file.tryLockExclusive() : file.tryLockShared();
  } catch (error) {
    ioError("file lock", error);
  }
}

function unlock(file: NativeLockHandle): void {
  try {
    file.unlock();
  } catch (error) {
    ioError("file unlock", error);
  }
}

function lockUntil(
  file: NativeLockHandle,
  exclusive: boolean,
  deadline: number | null,
  path: string,
  writer: boolean,
): void {
  for (;;) {
    if (tryLock(file, exclusive)) return;
    if (deadline !== null && performance.now() >= deadline) {
      if (writer) {
        throw engineError("lock_not_available", "could not obtain the database writer lock");
      }
      throw engineError("object_in_use", `database file is in use: ${path}`);
    }
    sleep(RETRY_MS);
  }
}

function canonicalOpenPath(path: string): string {
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
  private readonly files: LockFiles;
  private readonly openerPid = process.pid;
  private probe: NodeJS.Timeout | null = null;
  private closed = false;

  private constructor(path: string, files: LockFiles, state: LeaseState) {
    this.path = path;
    this.files = files;
    this.state = state;
  }

  static open(path: string, mode?: Locking, fileLockTimeoutMs?: number): FileCoordinator | null {
    const locking = resolvedLocking(mode);
    if (locking === "none") return null;
    return FileCoordinator.acquire(canonicalOpenPath(path), locking, fileLockTimeoutMs);
  }

  static create(path: string, mode?: Locking, fileLockTimeoutMs?: number): FileCoordinator | null {
    const locking = resolvedLocking(mode);
    if (locking === "none") return null;
    return FileCoordinator.acquire(canonicalCreatePath(path), locking, fileLockTimeoutMs);
  }

  private static acquire(path: string, mode: Locking, timeoutOption?: number): FileCoordinator {
    if (openPaths.has(path)) {
      throw engineError(
        "object_in_use",
        `database is already open in this process: ${path} (share one Database handle)`,
      );
    }
    openPaths.add(path);
    let files: LockFiles | null = null;
    try {
      files = openBundle(path);
      const deadline = performance.now() + timeoutValue(timeoutOption);
      lockUntil(files.arrival, false, deadline, path, false);
      validateProtocol(path);
      let state: LeaseState;
      if (mode === "exclusive") {
        for (;;) {
          lockUntil(files.transition, true, deadline, path, false);
          const alone = tryLock(files.presence, true);
          unlock(files.transition);
          if (alone) {
            state = "exclusive";
            break;
          }
          if (performance.now() >= deadline) {
            throw engineError("object_in_use", `database file is in use: ${path}`);
          }
          sleep(RETRY_MS);
        }
      } else {
        lockUntil(files.transition, true, deadline, path, false);
        const alone = tryLock(files.presence, true);
        unlock(files.transition);
        if (alone) state = "alone";
        else {
          lockUntil(files.presence, false, deadline, path, false);
          state = "shared";
        }
      }
      unlock(files.arrival);
      return new FileCoordinator(path, files, state);
    } catch (error) {
      if (files !== null) for (const file of Object.values(files)) file.close();
      openPaths.delete(path);
      throw error;
    }
  }

  checkPid(): void {
    if (process.pid !== this.openerPid) {
      throw engineError(
        "object_in_use",
        "database handle was inherited across fork; reopen it in the child",
      );
    }
  }

  startProbe(tick: () => void): void {
    if (this.state === "exclusive" || this.probe !== null) return;
    this.probe = setInterval(tick, 1000);
    this.probe.unref();
  }

  lockCommitShared(): void {
    lockUntil(this.files.commit, false, null, this.path, false);
  }

  lockCommitExclusive(): void {
    lockUntil(this.files.commit, true, null, this.path, false);
  }

  unlockCommit(): void {
    unlock(this.files.commit);
  }

  lockWriter(timeoutMs: number): void {
    const deadline = timeoutMs === 0 ? null : performance.now() + timeoutMs;
    lockUntil(this.files.writer, true, deadline, this.path, true);
  }

  unlockWriter(): void {
    unlock(this.files.writer);
  }

  tryArrivalExclusive(): boolean {
    return tryLock(this.files.arrival, true);
  }

  unlockArrival(): void {
    unlock(this.files.arrival);
  }

  lockTransition(): void {
    lockUntil(this.files.transition, true, null, this.path, false);
  }

  unlockTransition(): void {
    unlock(this.files.transition);
  }

  downgradePresence(): void {
    this.state = "shared";
    unlock(this.files.presence);
    lockUntil(this.files.presence, false, null, this.path, false);
  }

  tryUpgradePresence(): boolean {
    unlock(this.files.presence);
    if (tryLock(this.files.presence, true)) return true;
    lockUntil(this.files.presence, false, null, this.path, false);
    return false;
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    if (this.probe !== null) clearInterval(this.probe);
    this.probe = null;
    for (const file of Object.values(this.files)) file.close();
    openPaths.delete(this.path);
  }
}
