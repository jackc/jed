// MemoryBlockStore — the pure in-memory storage host (bplus-reshape.md B3): a growable byte buffer
// with the same positioned-read/write and zero-fill growth semantics as a file host, but with no
// durability work to do. It is the block-device building block for both in-memory databases (B3) and,
// since the temp-blockstore slice, per-domain session-local TEMP-table stores (newTempStorage,
// spec/design/temp-tables.md §6) — each rides the same pager + packed-leaf read path, with
// within-session compaction reclaiming its copy-on-write orphans (a temp store is never reopened).
// Browser-clean: no node:* imports.

import type { BlockStore } from "./blockstore.ts";

export class MemoryBlockStore implements BlockStore {
  private bytes: Uint8Array;

  constructor(image: Uint8Array = new Uint8Array()) {
    this.bytes = image.slice();
  }

  readAt(offset: number, len: number): Uint8Array {
    this.checkOffset(offset);
    if (len < 0 || offset + len > this.bytes.length) {
      throw new Error("I/O error: short read in memory block store");
    }
    return this.bytes.slice(offset, offset + len);
  }

  writeAt(offset: number, bytes: Uint8Array): void {
    this.checkOffset(offset);
    const end = offset + bytes.length;
    if (end > this.bytes.length) this.resize(end);
    this.bytes.set(bytes, offset);
  }

  sync(): void {
    // In memory: nothing to flush.
  }

  size(): number {
    return this.bytes.length;
  }

  setSize(bytes: number): void {
    if (bytes < 0) throw new Error("I/O error: negative memory block store size");
    this.resize(bytes);
  }

  close(): void {
    // In memory: nothing to release.
  }

  private checkOffset(offset: number): void {
    if (offset < 0) throw new Error("I/O error: negative memory block store offset");
  }

  private resize(size: number): void {
    if (size === this.bytes.length) return;
    const next = new Uint8Array(size);
    next.set(this.bytes.subarray(0, Math.min(this.bytes.length, size)));
    this.bytes = next;
  }
}
