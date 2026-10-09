// The storage-host seam — the byte device a `Pager` composes (spec/design/hosts.md §2/§3). A
// `BlockStore` is the per-platform byte backing for one database file: an opaque, growable byte file
// addressed by byte offset + length, with NO notion of pages, meta slots, or the B-tree (those live in
// the pager above this seam). Keeping the host surface this small is what lets every host — Node `fs`
// (FileBlockStore, fileblockstore.ts), OPFS (OpfsBlockStore, opfsblockstore.ts), an
// encrypting/replicating wrap, even a pure in-memory buffer — be a thin adapter that cannot drift.
//
// This module holds the interface plus the host-free ClosedBlockStore, so it imports no `node:*` and is
// browser-bundle-clean: the pager and OpfsBlockStore type-import `BlockStore` from here without dragging
// the Node `fs` host in (the host impls live in their own modules — the same interface/impl split as
// spill.ts/spillfile.ts).

import { engineError } from "./errors.ts";

// BlockStore is the byte backing for one database file (spec/design/hosts.md §1/§2). The pager converts
// a page index to a byte offset (offset = index × pageSize) and drives this device; the host knows only
// offsets and lengths. The first five methods are the spec's §2 surface; close is a lifecycle method (a
// host owning an OS handle must be able to release it), not part of the data contract.
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
  // skipsSync reports whether sync skips its durability barrier (fsync=off, api.md §2.1), so a
  // compaction rewriting this store's file (api.md §2.6) applies the same setting. Absent means false.
  skipsSync?(): boolean;
}

// ClosedBlockStore is the store a pager holds after compaction closed its file (spec/design/api.md
// §2.6): the database now lives in a replacement store, so any snapshot still bound to this pager fails
// closed with 58030 instead of reading pages of the old layout.
export class ClosedBlockStore implements BlockStore {
  readAt(_offset: number, _len: number): Uint8Array {
    throw closedError();
  }

  writeAt(_offset: number, _bytes: Uint8Array): void {
    throw closedError();
  }

  sync(): void {
    throw closedError();
  }

  size(): number {
    throw closedError();
  }

  setSize(_bytes: number): void {
    throw closedError();
  }

  close(): void {
    // Nothing to release: the file was closed when this store took its place.
  }
}

function closedError(): Error {
  return engineError(
    "io_error",
    "database storage was replaced by compaction; close and reopen the handle",
  );
}
