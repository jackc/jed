// The file host: the filesystem operations a file database needs beyond its own byte device — create's
// and host compaction's sibling temp file, the rename over the real path, the directory barrier that
// makes the rename durable (spec/design/api.md §2.1, §2.6), and opening the file (main or an
// attachment). The Node host (file.ts osFiles) uses the OS. The
// deterministic simulator (spec/design/simulation-testing.md §14 S4) substitutes a simulated
// filesystem, so a crash can land between any two of these steps. Type-only, so the engine stays free
// of node:* imports.

import type { BlockStore } from "./blockstore.ts";

// A positioned write into a file being filled.
export type WriteAt = (offset: number, bytes: Uint8Array) => void;

export interface FileHost {
  // realPath resolves symlinks, so a rename replaces the real file, never a symlink naming it.
  realPath(path: string): string;
  // exists reports whether path names an existing file (create refuses one, 58P02).
  exists(path: string): boolean;
  // writeFile creates (or truncates) path, lets fill write it with positioned writes, gives it the
  // permission bits of the file at modeFrom ("" keeps the default), makes it durable unless noSync,
  // and closes it. The caller removes path on failure.
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
  // openStore opens an existing file as a database's byte device, read-write unless readOnly. A
  // missing file is 58P01.
  openStore(path: string, noSync: boolean, readOnly: boolean): BlockStore;
}
