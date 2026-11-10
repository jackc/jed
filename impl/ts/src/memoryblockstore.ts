// MemoryBlockStore — the pure in-memory storage host (bplus-reshape.md B3): a growable byte device
// with the same positioned-read/write and zero-fill growth semantics as a file host, but with no
// durability work to do. It is the block-device building block for both in-memory databases (B3) and,
// since the temp-blockstore slice, per-domain session-local TEMP-table stores (newTempStorage,
// spec/design/temp-tables.md §6) — each rides the same pager + packed-leaf read path, with
// within-session compaction reclaiming its copy-on-write orphans (a temp store is never reopened).
// Browser-clean: no node:* imports.
//
// The bytes are kept as blocks of the database's page size, and a stored block is never modified:
// writeAt installs a fresh block, so readAt returns a whole aligned block itself rather than a copy.
// A page read through the pager is therefore resident once, in the store, however many readers and
// cached leaves hold it, and a reader that still holds a page after its block is rewritten keeps the
// bytes it read. Page readers never write into the bytes they get back. Growth appends block
// references; it never copies the image.

import type { BlockStore } from "./blockstore.ts";
import { engineError } from "./errors.ts";

// DEFAULT_BLOCK_SIZE is the block size of a store whose image names no valid page size; reads of
// another size are then assembled into a copy.
const DEFAULT_BLOCK_SIZE = 4096;

export class MemoryBlockStore implements BlockStore {
  private blockSize: number;
  // blocks holds each block, null for one never written (it reads as zeros).
  private blocks: (Uint8Array | null)[] = [];
  private length = 0;

  // The constructor copies image, so the caller's buffer cannot observe later writes.
  constructor(image: Uint8Array = new Uint8Array()) {
    this.blockSize = imageBlockSize(image);
    this.writeAt(0, image);
  }

  // owning builds a store over a fresh image the caller hands over and never touches again: its
  // blocks are views of the image's own bytes.
  static owning(image: Uint8Array): MemoryBlockStore {
    const store = new MemoryBlockStore();
    const bs = imageBlockSize(image);
    store.blockSize = bs;
    for (let off = 0; off < image.length; off += bs) {
      if (off + bs <= image.length) {
        store.blocks.push(image.subarray(off, off + bs));
      } else {
        const last = new Uint8Array(bs);
        last.set(image.subarray(off));
        store.blocks.push(last);
      }
    }
    store.length = image.length;
    return store;
  }

  readAt(offset: number, len: number): Uint8Array {
    this.checkOffset(offset);
    if (len < 0 || offset + len > this.length) {
      // A page id past the end is a misdirected pointer in the image; like a file host's short read
      // it is the host's I/O error (58030), never a bare Error.
      throw engineError("io_error", "I/O error: short read in memory block store");
    }
    const bs = this.blockSize;
    if (len === bs && offset % bs === 0) {
      const b = this.blocks[offset / bs]!;
      if (b !== null) return b;
    }
    const out = new Uint8Array(len);
    for (let done = 0; done < len; ) {
      const at = offset + done;
      const i = Math.floor(at / bs);
      const lo = at % bs;
      const n = Math.min(bs - lo, len - done);
      const b = this.blocks[i]!;
      if (b !== null) out.set(b.subarray(lo, lo + n), done);
      done += n;
    }
    return out;
  }

  writeAt(offset: number, bytes: Uint8Array): void {
    this.checkOffset(offset);
    const end = offset + bytes.length;
    if (end > this.length) this.grow(end);
    const bs = this.blockSize;
    for (let done = 0; done < bytes.length; ) {
      const at = offset + done;
      const i = Math.floor(at / bs);
      const lo = at % bs;
      const n = Math.min(bs - lo, bytes.length - done);
      // Never edit a stored block in place: a reader may hold it.
      const b = new Uint8Array(bs);
      const old = this.blocks[i]!;
      if (n < bs && old !== null) b.set(old);
      b.set(bytes.subarray(done, done + n), lo);
      this.blocks[i] = b;
      done += n;
    }
  }

  sync(): void {
    // In memory: nothing to flush.
  }

  size(): number {
    return this.length;
  }

  setSize(bytes: number): void {
    if (bytes < 0) throw new Error("I/O error: negative memory block store size");
    if (bytes >= this.length) {
      this.grow(bytes);
      return;
    }
    const bs = this.blockSize;
    const keep = Math.ceil(bytes / bs);
    this.blocks.length = keep;
    // Bytes past the new end of a kept partial block must read as zeros after a later growth.
    const tail = bytes % bs;
    const last = keep > 0 ? this.blocks[keep - 1]! : null;
    if (tail !== 0 && last !== null) {
      const b = new Uint8Array(bs);
      b.set(last.subarray(0, tail));
      this.blocks[keep - 1] = b;
    }
    this.length = bytes;
  }

  close(): void {
    // In memory: nothing to release.
  }

  private checkOffset(offset: number): void {
    if (offset < 0) throw engineError("io_error", "I/O error: negative memory block store offset");
  }

  // grow extends the store to size bytes of zeros.
  private grow(size: number): void {
    const need = Math.ceil(size / this.blockSize);
    while (this.blocks.length < need) this.blocks.push(null);
    this.length = size;
  }
}

// imageBlockSize is the page size an image's meta header names (offset 8, format.md), else
// DEFAULT_BLOCK_SIZE.
function imageBlockSize(image: Uint8Array): number {
  if (image.length >= 12) {
    const ps = new DataView(image.buffer, image.byteOffset, image.byteLength).getUint32(8, false);
    if (ps >= 256 && ps <= 65536 && (ps & (ps - 1)) === 0) return ps;
  }
  return DEFAULT_BLOCK_SIZE;
}
