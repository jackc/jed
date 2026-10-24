// The stream transport's client (spec/design/page-service.md §2, §13; spec/pageservice/README.md
// §5.3): the same protocol over a child process's stdin and stdout instead of a local socket, which is
// how a client on another machine reaches an owner over SSH (`ssh host jed pages /path`).
//
// RemoteLockHost is the page-service host of a remote database. The socket worker (client_worker.ts)
// runs the transport command once per connection. The client never owns: there is no bundle to lock
// here, so the election always finds a serving owner and dials it by running the command again.
// Re-election and commit_status therefore work exactly as for a local client. Every hello carries the
// handle's credential, if it has one. The serving end (`jed pages`) is the Rust core's. Node-only.

import type { Credential } from "../crypto.ts";
import { EngineError, engineError } from "../errors.ts";
import type { LockFiles, LockHandle, LockHost } from "../lockhost.ts";
import type { PsOwnerRecord } from "./codec.ts";
import { currentUid } from "./endpoint.ts";
import type { StreamTransport } from "./net.ts";

// REMOTE_RETRY_MS is how long the remote client waits between transport attempts.
const REMOTE_RETRY_MS = 250;

// remoteNames is the process-local duplicate-open registry of remote handles, by name.
const remoteNames = new Set<string>();

const waitCell = new Int32Array(new SharedArrayBuffer(4));

// A bundle file's placeholder lock: never contended.
const PLACEHOLDER: LockHandle = Symbol("remote lock");

export class RemoteLockHost implements LockHost {
  readonly stream: StreamTransport;

  constructor(command: string[], credential: Credential | null) {
    this.stream = { command: [...command], credential, lastFailure: "" };
  }

  claimPath(name: string): boolean {
    if (remoteNames.has(name)) return false;
    remoteNames.add(name);
    return true;
  }

  releasePath(name: string): void {
    remoteNames.delete(name);
  }

  openBundle(_path: string): LockFiles {
    return { presence: PLACEHOLDER, arrival: PLACEHOLDER };
  }

  validateMarker(_path: string): void {}

  closeFiles(_files: LockFiles): void {}

  // tryLock grants arrival (shared) always and presence (exclusive) never: the database is owned on
  // the remote side.
  tryLock(_handle: LockHandle, exclusive: boolean): boolean {
    return !exclusive;
  }

  unlock(_handle: LockHandle): void {}

  nowMicros(): number {
    return performance.now() * 1000;
  }

  pause(): void {
    Atomics.wait(waitCell, 0, 0, REMOTE_RETRY_MS);
  }

  pid(): number {
    return process.pid;
  }

  // readOwnerRecord always names a serving owner of this user, reached by running the command.
  readOwnerRecord(_dbPath: string): PsOwnerRecord {
    return {
      serving: true,
      uid: currentUid(),
      incarnation: new Uint8Array(16),
      pid: 0,
      endpoint: "remote",
    };
  }

  writeOwnerRecord(_dbPath: string, _record: PsOwnerRecord): void {}

  removeOwnerRecord(_dbPath: string): void {}

  endpointPath(_dbPath: string, _uid: number, _incarnation: Uint8Array): string {
    return "";
  }

  removeEndpoint(_endpoint: string): void {}

  // openError words a failed open: an election that timed out without a connection reports the
  // transport's own complaint (08006).
  openError(error: unknown): unknown {
    if (!(error instanceof EngineError) || !["55006", "08006"].includes(error.code())) return error;
    const detail = this.stream.lastFailure.trim();
    const shown = this.stream.command.join(" ");
    return engineError(
      "connection_failure",
      detail === ""
        ? `could not reach the database through \`${shown}\``
        : `could not reach the database through \`${shown}\`: ${detail}`,
    );
  }
}
