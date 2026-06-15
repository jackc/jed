// The storage-host seam — the byte device a `Pager` composes (spec/design/hosts.md §2/§3). A
// `BlockStore` is the per-platform byte backing for one database file: an opaque, growable byte file
// addressed by byte offset + length, with NO notion of pages, meta slots, or the B-tree (those live in
// the pager above this seam). Keeping the host surface this small is what lets every host — Node `fs`,
// OPFS, an encrypting/replicating wrap, even a pure in-memory buffer — be a thin adapter that cannot
// drift.
//
// This slice extracts the seam and ships the one Node `fs` host (`FileBlockStore`); the in-memory,
// OPFS, encrypting, and replicating hosts are the catalog's other rows (hosts.md §4) and are NOT built
// here. The extraction is a pure refactor: the file-specific bits (open, the data-only fdatasync, the
// durable-grow write+fsync) move out of `pager.ts` into `FileBlockStore`, while the policy — page
// math, the 1 MiB preallocation chunk, which barrier each step needs, the fault-injection seam — stays
// in the host-independent `Pager` (hosts.md §3). No behavior or byte change. Isolated here precisely so
// the future browser/OPFS host is a sibling, not a reshape (storage.md §2).

import { closeSync, fdatasyncSync, fstatSync, fsyncSync, ftruncateSync, readSync, writeSync } from "node:fs";

// BlockStore is the byte backing for one database file (spec/design/hosts.md §1/§2). The pager
// converts a page index to a byte offset (offset = index × pageSize) and drives this device; the host
// knows only offsets and lengths. The first five methods are the spec's §2 surface; close is a
// lifecycle method (a host owning an OS handle must be able to release it), not part of the data
// contract.
export interface BlockStore {
  // readAt reads len bytes at byte offset. A short read past size() is the host's error (58030).
  readAt(offset: number, len: number): Uint8Array;
  // writeAt stages a write of bytes at byte offset — staged, not durable until sync (or setSize's
  // grow). Positioned: it must not move a shared cursor (hosts.md §2.1).
  writeAt(offset: number, bytes: Uint8Array): void;
  // sync is the data-only durability barrier (fdatasync): every prior in-region writeAt becomes
  // durable WITHOUT a file-size/inode metadata journal (hosts.md §2.1, spec/design/pager.md §7). A
  // host lacking a data-only barrier may implement it as a full fsync (correct, just slower).
  sync(): void;
  // size is the current file length in bytes.
  size(): number;
  // setSize durably grows (real zero blocks + a full fsync) or truncates to bytes — the metadata
  // barrier (hosts.md §2.1). After it returns, bytes in [old, bytes) read back as zero AND the
  // allocation is durable, so a later in-region writeAt + data-only sync need not flush a growth journal.
  setSize(bytes: number): void;
  // close releases the backing (the OS file descriptor). Lifecycle, not part of the §2 data surface.
  close(): void;
}

// FileBlockStore is the Node `fs` storage host (spec/design/hosts.md §4): an open fd with positioned
// readSync/writeSync (no shared cursor), a data-only fdatasync barrier, and a durable-grow zero-write +
// full fsync. The one host built by the BlockStore-extraction slice; OPFS / encrypting / replicating /
// in-memory are the catalog's other rows. fs calls throw raw Node errors here exactly as the inlined
// pager did before the extraction; the host program layer (file.ts) maps them at its boundaries.
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
