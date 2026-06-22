// FileBlockStore — the Node `fs` storage host (spec/design/hosts.md §4): an open fd with positioned
// readSync/writeSync (no shared cursor), a data-only fdatasync barrier, and a durable-grow zero-write +
// full fsync. The one host built by the BlockStore-extraction slice; OPFS (opfsblockstore.ts) /
// encrypting / replicating / in-memory are the catalog's other rows. Kept in its own module (separate
// from the browser-clean BlockStore interface in blockstore.ts) so the `node:fs` import never reaches a
// browser bundle — the same interface/impl split as spill.ts / spillfile.ts. fs calls throw raw Node
// errors here exactly as the inlined pager did before the extraction; the host program layer (file.ts)
// maps them at its boundaries.

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

  constructor(fd: number) {
    this.fd = fd;
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
      fsyncSync(this.fd);
    } else if (bytes < cur) {
      ftruncateSync(this.fd, bytes); // truncate; no barrier needed
    }
  }

  close(): void {
    closeSync(this.fd);
  }
}
