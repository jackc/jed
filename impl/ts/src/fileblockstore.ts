// FileBlockStore — the Node `fs` storage host (spec/design/hosts.md §4): an open fd with positioned
// readSync/writeSync (no shared cursor), a data-only fdatasync barrier, and a durable-grow zero-write +
// full fsync. Kept in its own module (separate from the browser-clean BlockStore interface in
// blockstore.ts, OpfsBlockStore in opfsblockstore.ts, and MemoryBlockStore in memoryblockstore.ts) so
// the `node:fs` import never reaches a browser bundle — the same interface/impl split as spill.ts /
// spillfile.ts. fs calls throw raw Node errors here exactly as the inlined pager did before the
// extraction; the host program layer (file.ts) maps them at its boundaries.

import {
  closeSync,
  fdatasyncSync,
  fstatSync,
  fsyncSync,
  ftruncateSync,
  readSync,
  writeSync,
} from "node:fs";
import type { BlockStore } from "./blockstore.ts";

export class FileBlockStore implements BlockStore {
  private fd: number;
  // fsync=off (the host setting, api.md §2.1): make sync() and the durable-grow fsync no-ops. The
  // commit writes the same bytes in the same order; only the flush to the platter is skipped.
  // DEV/TESTING only — durable across a process crash (the OS page cache still flushes) but NOT across
  // an OS crash / power loss. Default false (fsync on).
  private noSync: boolean;

  constructor(fd: number, noSync = false) {
    this.fd = fd;
    this.noSync = noSync;
  }

  readAt(offset: number, len: number): Uint8Array {
    const buf = new Uint8Array(len);
    readSync(this.fd, buf, 0, len, offset);
    return buf;
  }

  writeAt(offset: number, bytes: Uint8Array): void {
    writeSync(this.fd, bytes, 0, bytes.length, offset);
  }

  sync(): void {
    if (this.noSync) return; // fsync=off (api.md §2.1): skip the durability barrier — dev/testing only.
    // fdatasync, not a full fsync: an overwrite into the preallocated region flushes only data, never
    // a file-size/inode-timestamp metadata journal (spec/design/pager.md §7).
    fdatasyncSync(this.fd);
  }

  size(): number {
    return fstatSync(this.fd).size;
  }

  setSize(bytes: number): void {
    const cur = fstatSync(this.fd).size;
    if (bytes > cur) {
      // Grow with real zero blocks, then a full fsync: the allocation + new size must be durable
      // before a later in-region commit relies on it (else the per-commit data-only sync would have to
      // flush that metadata, defeating the durable-commit win — spec/design/pager.md §7).
      const zeros = new Uint8Array(bytes - cur);
      writeSync(this.fd, zeros, 0, zeros.length, cur);
      if (!this.noSync) fsyncSync(this.fd); // fsync=off skips the durable-grow barrier too (dev/testing).
    } else if (bytes < cur) {
      ftruncateSync(this.fd, bytes); // truncate; no barrier needed
    }
  }

  close(): void {
    closeSync(this.fd);
  }
}
