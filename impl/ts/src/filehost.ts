// The file host: the filesystem operations a file database needs beyond its own byte device — host
// compaction's sibling temp file, the rename over the real path, and the directory barrier that makes
// the rename durable (spec/design/api.md §2.6). The Node host (file.ts osFiles) uses the OS. The
// deterministic simulator (spec/design/simulation-testing.md §14 S4) substitutes a simulated
// filesystem, so a crash can land between any two of these steps. Type-only, so the engine stays free
// of node:* imports.

import type { BlockStore } from "./blockstore.ts";

// A positioned write into a file being filled.
export type WriteAt = (offset: number, bytes: Uint8Array) => void;

export interface FileHost {
  // realPath resolves symlinks, so a rename replaces the real file, never a symlink naming it.
  realPath(path: string): string;
  // writeFile creates (or truncates) path, lets fill write it with positioned writes, gives it the
  // permission bits of the file at modeFrom, makes it durable unless noSync, and closes it. The
  // caller removes path on failure.
  writeFile(
    path: string,
    modeFrom: string,
    noSync: boolean,
    fill: (writeAt: WriteAt) => void,
  ): void;
  rename(from: string, to: string): void;
  // remove deletes path, ignoring any error (cleanup after a failure).
  remove(path: string): void;
  // syncDir makes dir's entries durable — best effort: not every platform can fsync a directory.
  syncDir(dir: string): void;
  // openStore opens an existing file read-write as a database's byte device.
  openStore(path: string, noSync: boolean): BlockStore;
}
