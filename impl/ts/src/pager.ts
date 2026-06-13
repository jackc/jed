// Block-device pager — the storage seam (spec/design/storage.md §2) for a file-backed database
// (spec/design/pager.md). It owns the open file descriptor for the handle's life so pages can be
// read on demand and the incremental commit (P6.1) can write them without re-opening the file each
// time. Node `fs` host (the browser/OPFS host is a sibling later — storage.md §2).
//
// P6.4a (this slice) routes the whole-image load and the commit through readBlock/writeBlock with no
// residency change — the loader still assembles the full image (readAll) and builds the whole tree.
// The bounded buffer pool + lazy node loading that make the resident set bounded (P6.4b) read through
// this same readBlock.

import { closeSync, fdatasyncSync, fstatSync, fsyncSync, readSync, writeSync } from "node:fs";

import { type EngineError, engineError } from "./errors.ts";

// PREALLOC_CHUNK_BYTES is the file-growth step — ~1 MiB worth of pages preallocated at once. Growing
// the file in chunks of real, durably-allocated zero blocks is what lets a steady-state commit write
// its body into already-allocated space, so the per-commit fdatasync (Pager.sync) carries no ext4
// metadata-journaling for a file-size change — the durable-commit win (spec/design/pager.md §7,
// TODO.md). The chunk's one-time allocating fsync (Pager.reserve) amortizes across the chunk's commits.
const PREALLOC_CHUNK_BYTES = 1024 * 1024;

// preallocChunkPages is the preallocation chunk in pages for a file of pageSize bytes: max(1,
// 1 MiB / pageSize). Page sizes are powers of two ≤ 64 KiB, so this divides 1 MiB evenly — the
// physical file therefore grows in exact 1 MiB steps regardless of page size.
function preallocChunkPages(pageSize: number): number {
  return Math.max(1, Math.floor(PREALLOC_CHUNK_BYTES / Math.max(1, pageSize)));
}

// FaultPoint selects a point in the commit write sequence at which the fault-injection seam
// (spec/design/storage.md §7) simulates a crash. Pages 0/1 are always the meta slots and every
// body/catalog page is ≥ 2 (format.md), so "meta_write" is identified by the page index, never by
// counting body pages. Testing only.
export type FaultPoint = "body_write" | "meta_write" | "sync";

// CommitFault is a one-shot crash/tear the pager simulates at a chosen commit point (storage.md §7).
// Testing only. Not a §8 byte contract: like the buffer pool (pager.md §3), the seam is per-core
// internal machinery; the cross-core contract is the recovery outcome.
export interface CommitFault {
  point: FaultPoint;
  // For body_write/sync: the 1-based ordinal (nth body-page write / nth sync since arming). Ignored
  // for meta_write.
  n?: number;
  // For a write point: the count of leading page bytes to write before failing (a torn page); omitted
  // or negative means write nothing (a clean crash before the page lands). Ignored for sync.
  tearBytes?: number;
}

// injectedCrash is the error an armed fault throws to abort persistImpl mid-commit — a simulated
// crash, reported as an ordinary I/O failure so the commit path rolls back exactly as a real write
// error would (spec/design/storage.md §7). Testing only.
function injectedCrash(): EngineError {
  return engineError("io_error", "injected commit crash (fault injection)");
}

// A file-backed block device: fixed-size pages addressed by index, over an open fd kept for the
// handle's life. One page at a time (storage.md §2); the demand-paging buffer pool (P6.4b) faults
// pages in through readBlock.
export class Pager {
  private fd: number;
  pageSize: number;
  // allocatedPages is the number of pages physically allocated on disk — the file length in pages,
  // which the chunked preallocation (reserve) runs ahead of the committed high-water. A commit whose
  // pages all fall below this never grows the file (storage.md §9). Distinct from the committed
  // logical pageCount the meta records: the slack pages in [pageCount, allocatedPages) are
  // unreferenced trailing zeros (no byte-contract impact — past the high-water).
  private allocatedPages: number;
  // fault is the armed one-shot commit fault — the fault-injection seam (spec/design/storage.md §7),
  // null unless a test armed one with armFault. Production never arms one, so the checks in
  // writeBlock/sync are a single null branch. bodyWrites/syncs count body-page writes (index ≥ 2) and
  // sync() calls since arming, driving "body_write" / "sync".
  private fault: CommitFault | null = null;
  private bodyWrites = 0;
  private syncs = 0;

  private constructor(fd: number, pageSize: number, allocatedPages: number) {
    this.fd = fd;
    this.pageSize = pageSize;
    this.allocatedPages = allocatedPages;
  }

  // armFault arms a one-shot commit fault (the fault-injection seam, spec/design/storage.md §7) and
  // resets the since-arm counters, so the next commit's body-write / meta-write / sync sequence
  // triggers it. The fault auto-disarms when it fires. Testing only.
  armFault(fault: CommitFault): void {
    this.fault = fault;
    this.bodyWrites = 0;
    this.syncs = 0;
  }

  // faultOnWrite is the fault-injection seam's write hook (storage.md §7): null-fault → no-op. If an
  // armed fault targets this write it optionally performs a torn partial write, then disarms and
  // throws an injected-crash error so persistImpl aborts mid-commit.
  private faultOnWrite(index: number, bytes: Uint8Array): void {
    const f = this.fault;
    if (f === null) return;
    let hit = false;
    if (f.point === "meta_write") {
      hit = index < 2;
    } else if (f.point === "body_write" && index >= 2) {
      this.bodyWrites++;
      hit = this.bodyWrites === f.n;
    }
    if (!hit) return;
    this.fault = null; // one-shot
    const tear = f.tearBytes ?? -1;
    if (tear >= 0) {
      const k = Math.min(tear, bytes.length);
      writeSync(this.fd, bytes.subarray(0, k), 0, k, index * this.pageSize);
    }
    throw injectedCrash();
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
    // The allocation high-water is the current file length in pages — already past the committed
    // pageCount if a prior session preallocated slack (reused for free on this session's growth).
    const allocatedPages = Math.floor(fstatSync(fd).size / pageSize);
    return new Pager(fd, pageSize, allocatedPages);
  }

  // readBlock reads one page (block index) — random access, the demand-paging read path (P6.4b).
  readBlock(index: number): Uint8Array {
    const buf = new Uint8Array(this.pageSize);
    readSync(this.fd, buf, 0, this.pageSize, index * this.pageSize);
    return buf;
  }

  // writeBlock writes one page (bytes) at block index. Overwrites in place — persistImpl always
  // reserves the high-water first, so the target is already-allocated space (a reused free page, or a
  // preallocated slot past the old high-water). bytes is one page wide.
  writeBlock(index: number, bytes: Uint8Array): void {
    this.faultOnWrite(index, bytes);
    writeSync(this.fd, bytes, 0, bytes.length, index * this.pageSize);
  }

  // reserve ensures the file has at least minPages physically-allocated pages, growing it in fixed
  // chunks (preallocChunkPages) of real, durably-allocated zero blocks when short. persistImpl calls
  // it before each commit's body write with the new committed high-water, so that write — and almost
  // every commit's — lands entirely in already-allocated space and its fdatasync (Pager.sync) pays no
  // metadata journaling (spec/design/pager.md §7). The growth itself is a full fsync: the block
  // allocation + the new file size must be durable before commits rely on writing into the region
  // (else the body fdatasync would have to flush that metadata, defeating the point). Crash-safe: the
  // preallocated pages are unreferenced zeros past the committed pageCount, so a crash before the next
  // commit publishes simply ignores them.
  reserve(minPages: number): void {
    if (minPages <= this.allocatedPages) return;
    const chunk = preallocChunkPages(this.pageSize);
    const target = Math.ceil(minPages / chunk) * chunk;
    const zeros = new Uint8Array((target - this.allocatedPages) * this.pageSize);
    writeSync(this.fd, zeros, 0, zeros.length, this.allocatedPages * this.pageSize);
    fsyncSync(this.fd); // the allocation must be durable before in-region commits
    this.allocatedPages = target;
  }

  // sync is the metadata-free durability barrier (fdatasync). Called twice per commit — body pages,
  // then the meta — to honour the body-before-meta write-ordering rule (format.md, file.ts
  // persistImpl). fdatasync (not fsync) so an overwrite into the preallocated region (reserve) flushes
  // only the data, never a file-size/inode-timestamp metadata journal (spec/design/pager.md §7).
  sync(): void {
    if (this.fault !== null && this.fault.point === "sync") {
      this.syncs++;
      if (this.syncs === this.fault.n) {
        this.fault = null; // one-shot
        throw injectedCrash();
      }
    }
    fdatasyncSync(this.fd);
  }

  // close closes the open fd (close()).
  close(): void {
    closeSync(this.fd);
  }
}
