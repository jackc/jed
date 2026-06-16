// The Browser/OPFS storage host (spec/design/hosts.md §4/§5): a BlockStore backed by an OPFS
// FileSystemSyncAccessHandle. It is the one new host the browser slice adds; the five-method seam maps
// one-to-one onto the sync access handle's read/write/truncate/getSize/flush (hosts.md §5). This module
// is deliberately browser-CLEAN — it imports no `node:*`, only a type from blockstore.ts (erased at
// runtime) — so it lands in a browser bundle without dragging the Node `fs` host (FileBlockStore) in.
//
// The class is synchronous, like the whole core above the seam: it wraps an ALREADY-ACQUIRED handle.
// Acquiring the handle (navigator.storage.getDirectory → getFileHandle → createSyncAccessHandle) is
// async and lives at the bootstrap edge (opfs.ts), not here — so this stays a thin, testable adapter
// (the parity test drives it in Node with a fake handle — hosts.md §5).

import type { BlockStore } from "./blockstore.ts";

// SyncAccessHandle is the structural subset of the DOM FileSystemSyncAccessHandle that OpfsBlockStore
// uses (spec/design/hosts.md §5). Declaring our own interface (rather than depending on the DOM lib
// types) keeps the core on `@types/node` only AND lets the parity test drive OpfsBlockStore with a Node
// fake handle — no browser needed to verify the byte contract.
export interface SyncAccessHandle {
  // read fills buffer from byte offset opts.at and returns the number of bytes read (fewer than
  // buffer.length only at end-of-file). Mirrors FileSystemSyncAccessHandle.read.
  read(buffer: Uint8Array, opts: { at: number }): number;
  // write writes buffer at byte offset opts.at and returns the number of bytes written.
  write(buffer: Uint8Array, opts: { at: number }): number;
  // truncate resizes the file to newSize bytes (shrinking discards the tail).
  truncate(newSize: number): void;
  // getSize returns the current file length in bytes.
  getSize(): number;
  // flush makes prior writes durable — OPFS's only durability primitive (hosts.md §5).
  flush(): void;
  // close releases the (exclusive) access handle.
  close(): void;
}

// OpfsBlockStore is the OPFS storage host (spec/design/hosts.md §4/§5): the five BlockStore methods over
// a FileSystemSyncAccessHandle. The pager above it converts a page index to a byte offset and is wholly
// unaware this is OPFS rather than a file (hosts.md §1/§3).
//
// OPFS has ONE durability barrier — flush() — so the §2.1 data-only/metadata barrier split collapses
// onto it: sync() IS flush(), and setSize's durable grow is "write real zero blocks, then flush()" (a
// bare truncate(n) would grow sparsely and re-allocate-journal on first write — hosts.md §5). By the
// §2.1 contract this is correct, just slower than the file hosts' two-barrier path — the right default
// for a browser. Byte behavior MUST match FileBlockStore exactly (incl. a short read past the
// high-water leaving the tail zero), since the cross-core round-trip golden pins the bytes (CLAUDE.md §8).
export class OpfsBlockStore implements BlockStore {
  private handle: SyncAccessHandle;

  constructor(handle: SyncAccessHandle) {
    this.handle = handle;
  }

  readAt(offset: number, len: number): Uint8Array {
    const buf = new Uint8Array(len);
    // A short read (offset+len past the file size) fills only the prefix; the tail stays zero — the
    // same observable result as FileBlockStore's readSync, which the pager's header/page checks expect.
    this.handle.read(buf, { at: offset });
    return buf;
  }

  writeAt(offset: number, bytes: Uint8Array): void {
    this.handle.write(bytes, { at: offset });
  }

  sync(): void {
    // OPFS's sole durability primitive; serves as both the data-only and metadata barrier (hosts.md §5).
    this.handle.flush();
  }

  size(): number {
    return this.handle.getSize();
  }

  setSize(bytes: number): void {
    const cur = this.handle.getSize();
    if (bytes > cur) {
      // Durable grow: write REAL zero blocks (not a sparse truncate), then flush — so a later in-region
      // write + sync need not re-allocate (hosts.md §5 / §2.1). OPFS forgoes the file hosts'
      // metadata-free win, which the §2.1 contract calls correct, just slower.
      const zeros = new Uint8Array(bytes - cur);
      this.handle.write(zeros, { at: cur });
      this.handle.flush();
    } else if (bytes < cur) {
      this.handle.truncate(bytes); // shrink; no barrier needed
    }
  }

  close(): void {
    this.handle.close();
  }
}
