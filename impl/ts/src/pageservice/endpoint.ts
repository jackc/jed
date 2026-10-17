// The page-service endpoint and owner record on the host filesystem (spec/pageservice/README.md §5):
// where an owner binds its socket, how a directory proves it is private to one user, and the owner
// record clients read to find the endpoint. Unix only: Windows serving is not yet specified
// (page-service.md §14), so `locking = "auto"` is a non-serving owner there. Node-only.

import { randomFillSync } from "node:crypto";
import { lstatSync, mkdirSync, readFileSync, renameSync, unlinkSync, writeFileSync } from "node:fs";
import { join, normalize } from "node:path";
import process from "node:process";

import { engineError } from "../errors.ts";
import {
  decodeOwnerRecord,
  encodeOwnerRecord,
  PS_ENDPOINT_MAX_PATH_BYTES,
  type PsOwnerRecord,
} from "./codec.ts";

export function pageServiceSupported(): boolean {
  return process.platform !== "win32" && typeof process.geteuid === "function";
}

// currentUid is the effective uid (0 where the platform has none, as on Windows).
export function currentUid(): number {
  return process.geteuid?.() ?? 0;
}

// newIncarnation draws a nonzero random 16-byte owner incarnation (README §5.1).
export function newIncarnation(): Uint8Array {
  const id = new Uint8Array(16);
  while (id.every((b) => b === 0)) randomFillSync(id);
  return id;
}

function hex(bytes: Uint8Array): string {
  return Buffer.from(bytes).toString("hex");
}

function fits(path: string): boolean {
  return Buffer.byteLength(path) <= PS_ENDPOINT_MAX_PATH_BYTES;
}

// privateDir reports whether dir is a real directory (not a symlink) owned by uid (the effective uid)
// with mode exactly 0700 — the check every endpoint directory must pass (README §5.2).
export function privateDir(dir: string, uid: number): boolean {
  try {
    const info = lstatSync(dir);
    // Permission bits exactly 0700, and no setuid, setgid, or sticky bit.
    return info.isDirectory() && (info.mode & 0o7777) === 0o700 && info.uid === uid;
  } catch {
    return false;
  }
}

// ensurePrivateDir creates dir with mode 0700 when it is absent, then checks it.
function ensurePrivateDir(dir: string, uid: number): boolean {
  try {
    mkdirSync(dir, { mode: 0o700 });
  } catch {
    // Already present (checked below) or not creatable (fails the check).
  }
  return privateDir(dir, uid);
}

// endpointPath picks the owner's socket path (README §5.2): the bundle's per-user directory when the
// path fits, otherwise a runtime directory. "" means the owner cannot serve. platform and env are
// parameters so a test can choose them.
export function endpointPath(
  dbPath: string,
  uid: number,
  incarnation: Uint8Array,
  platform: string = process.platform,
  env: Record<string, string | undefined> = process.env,
): string {
  const bundleDir = join(`${dbPath}.lock`, `uid-${uid}`);
  const inBundle = join(bundleDir, "endpoint");
  if (fits(inBundle) && ensurePrivateDir(bundleDir, uid)) return inBundle;
  const name = `jed-${hex(incarnation.subarray(0, 8))}.sock`;
  const candidates: string[] = [];
  if (platform === "linux" && env.XDG_RUNTIME_DIR) candidates.push(env.XDG_RUNTIME_DIR);
  if (platform === "darwin" && env.TMPDIR)
    candidates.push(normalize(env.TMPDIR).replace(/\/$/, ""));
  for (const dir of candidates) {
    const path = join(dir, name);
    if (fits(path) && privateDir(dir, uid)) return path;
  }
  const dir = `/tmp/jed-${uid}`;
  const path = join(dir, name);
  if (fits(path) && ensurePrivateDir(dir, uid)) return path;
  return "";
}

// removeQuietly unlinks path, ignoring a missing file.
export function removeQuietly(path: string): void {
  try {
    unlinkSync(path);
  } catch {
    // Absent or already removed.
  }
}

// ownerRecordPath is the owner record's place in the bundle (README §5.1).
export function ownerRecordPath(dbPath: string): string {
  return join(`${dbPath}.lock`, "owner");
}

// writeOwnerRecord publishes rec atomically: a temp file named by the incarnation, then a rename.
export function writeOwnerRecord(dbPath: string, rec: PsOwnerRecord): void {
  const final = ownerRecordPath(dbPath);
  const tmp = `${final}.tmp-${hex(rec.incarnation)}`;
  try {
    writeFileSync(tmp, encodeOwnerRecord(rec), { mode: 0o644 });
    renameSync(tmp, final);
  } catch (error) {
    removeQuietly(tmp);
    const message = error instanceof Error ? error.message : String(error);
    throw engineError("io_error", `write owner record: ${message}`);
  }
}

// readOwnerRecord returns the current owner record, or null when it is absent or unparsable
// (README §5.1: such a record is treated as absent).
export function readOwnerRecord(dbPath: string): PsOwnerRecord | null {
  try {
    return decodeOwnerRecord(new Uint8Array(readFileSync(ownerRecordPath(dbPath))));
  } catch {
    return null;
  }
}
