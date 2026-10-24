// The coordination host: everything the file coordinator (coordinator.ts, spec/design/locking.md)
// and the page-service election (pageservice/election.ts) need from their environment — the lock
// bundle beside the database file, whole-file locks on it, the clock deadlines read, the pause
// between polls of a contended lock, the process id the fork check compares, and the process-local
// duplicate-open registry. Production uses the OS (coordinator.ts osLocks). The deterministic
// simulator (spec/design/simulation-testing.md §14 S8) substitutes simulated processes sharing one
// lock table and an in-memory page-service network, so the protocol runs, crashes, and interleaves
// deterministically. It also holds the page service's host operations (spec/pageservice/README.md
// §5): the owner record and the owner's endpoint. Type-only, so it imports nothing at run time.

import type { PsOwnerRecord } from "./pageservice/codec.ts";
import type { PageNet, StreamTransport } from "./pageservice/net.ts";

// A LockHandle is one bundle file's lock: a native lock file for the OS host.
export type LockHandle = unknown;

export type LockFiles = {
  presence: LockHandle;
  arrival: LockHandle;
};

export interface LockHost {
  // claimPath registers path as open in this process; false if it already is.
  claimPath(path: string): boolean;
  releasePath(path: string): void;
  // openBundle creates (if needed), validates, and opens the lock bundle beside path.
  openBundle(path: string): LockFiles;
  // validateMarker re-checks the protocol marker after arrival SH is held.
  validateMarker(path: string): void;
  closeFiles(files: LockFiles): void;
  tryLock(handle: LockHandle, exclusive: boolean): boolean;
  unlock(handle: LockHandle): void;
  // nowMicros is the clock lock deadlines read, in microseconds.
  nowMicros(): number;
  // pause waits between polls of a contended lock.
  pause(): void;
  pid(): number;
  // The page service's host operations (spec/pageservice/README.md §5): the owner record in the bundle
  // (null when absent or unparsable), and the owner's endpoint path ("" when it cannot serve).
  readOwnerRecord(dbPath: string): PsOwnerRecord | null;
  writeOwnerRecord(dbPath: string, record: PsOwnerRecord): void;
  removeOwnerRecord(dbPath: string): void;
  endpointPath(dbPath: string, uid: number, incarnation: Uint8Array): string;
  removeEndpoint(endpoint: string): void;
  // pageNet is the simulator's in-memory page-service network. Absent (the OS host), an owner serves
  // and a client connects on the process's worker threads over OS sockets.
  readonly pageNet?: PageNet;
  // stream is a remote database's transport (pageservice/remote.ts): a client connects by running its
  // command, sends its credential in every hello, and reads ahead. Absent, a client dials the owner's
  // endpoint socket.
  readonly stream?: StreamTransport;
}
