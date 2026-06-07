// Block-device pager — the storage seam (spec/design/storage.md §2) for a file-backed database
// (spec/design/pager.md). It owns the open file descriptor for the handle's life so pages can be
// read on demand and the incremental commit (P6.1) can write them without re-opening the file each
// time. Node `fs` host (the browser/OPFS host is a sibling later — storage.md §2).
//
// P6.4a (this slice) routes the whole-image load and the commit through readBlock/writeBlock with no
// residency change — the loader still assembles the full image (readAll) and builds the whole tree.
// The bounded buffer pool + lazy node loading that make the resident set bounded (P6.4b) read through
// this same readBlock.

import { closeSync, fsyncSync, readSync, writeSync } from "node:fs";

import { engineError } from "./errors.ts";

// A file-backed block device: fixed-size pages addressed by index, over an open fd kept for the
// handle's life. One page at a time (storage.md §2); the demand-paging buffer pool (P6.4b) faults
// pages in through readBlock.
export class Pager {
  private fd: number;
  pageSize: number;

  private constructor(fd: number, pageSize: number) {
    this.fd = fd;
    this.pageSize = pageSize;
  }

  // fromFd adopts an already-open (r+) fd as the backing, reading the page size from its meta header
  // (offset 8, format.md). The host layer (file.ts) opens the fd — mapping a missing path to 58P01 —
  // and hands it here. A header too short or a zero page size is XX001.
  static fromFd(fd: number): Pager {
    const header = new Uint8Array(12);
    if (readSync(fd, header, 0, 12, 0) < 12) {
      throw engineError("data_corrupted", "database file smaller than a meta header");
    }
    const pageSize = new DataView(header.buffer, header.byteOffset, header.byteLength).getUint32(8, false);
    if (pageSize === 0) {
      throw engineError("data_corrupted", "zero page size in meta header");
    }
    return new Pager(fd, pageSize);
  }

  // readBlock reads one page (block index) — random access, the demand-paging read path (P6.4b).
  readBlock(index: number): Uint8Array {
    const buf = new Uint8Array(this.pageSize);
    readSync(this.fd, buf, 0, this.pageSize, index * this.pageSize);
    return buf;
  }

  // writeBlock writes one page (bytes) at block index. Extends the file when index is the high-water,
  // overwrites in place when reusing a free page (P6.2). bytes is one page wide.
  writeBlock(index: number, bytes: Uint8Array): void {
    writeSync(this.fd, bytes, 0, bytes.length, index * this.pageSize);
  }

  // sync is the durability barrier (fsync). Called twice per commit — body pages, then the meta — to
  // honour the body-before-meta write-ordering rule (format.md, file.ts persistImpl).
  sync(): void {
    fsyncSync(this.fd);
  }

  // close closes the open fd (close()).
  close(): void {
    closeSync(this.fd);
  }
}
