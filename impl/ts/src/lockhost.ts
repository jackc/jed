// The coordination host: everything the shared-file coordinator (coordinator.ts, spec/design/
// locking.md) needs from its environment — the lock bundle beside the database file, whole-file locks
// on it, the clock its deadlines read, the pause between polls of a contended lock, the background
// presence probe, the process id the fork check compares, and the process-local duplicate-open
// registry. Production uses the OS (coordinator.ts osLocks). The deterministic simulator
// (spec/design/simulation-testing.md §14 S5) substitutes simulated processes sharing one lock table,
// so the protocol runs, crashes, and interleaves deterministically. Type-only, so it imports nothing.

// A LockHandle is one bundle file's lock: a native lock file for the OS host.
export type LockHandle = unknown;

export type LockFiles = {
  presence: LockHandle;
  arrival: LockHandle;
  transition: LockHandle;
  writer: LockHandle;
  commit: LockHandle;
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
  // startProbe runs tick periodically and returns the function that stops it.
  startProbe(tick: () => void): () => void;
  pid(): number;
}
