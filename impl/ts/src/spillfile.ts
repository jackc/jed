// The Node `fs` spill backing for the external merge sort (spec/design/spill.md §4). This is the ONE
// node:fs-using piece of the sort path, lifted out of spill.ts behind the SpillSink seam so spill.ts —
// and thus the whole executor — imports no `node:*` and lands in a browser bundle (the OPFS host).
// The Node file host (file.ts) injects a FileSpillSink on the Engine handle; an in-memory or OPFS
// database leaves it null and never spills (sorts stay resident — spill.md §2). Node stdlib I/O only
// (no dependency — CLAUDE.md §14); the run file's bytes are a per-core internal codec, never the §8
// on-disk format (spill.md §6).

import { closeSync, openSync, readSync, unlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { SpillByteReader, SpillRun, SpillSink } from "./spill.ts";

// A unique-per-process counter for spill file names (combined with the process id), so concurrent
// sorters never collide. Internal — it never affects results (spill.md §6).
let spillSeq = 0;

// FileSpillSink writes each spilled run to a temp file under `dir` (the database file's directory,
// guaranteed writable — spill.md §4) and hands back a FileSpillRun to read it lazily.
export class FileSpillSink implements SpillSink {
  private dir: string;

  constructor(dir: string) {
    this.dir = dir;
  }

  writeRun(bytes: Uint8Array): SpillRun {
    const path = join(this.dir, `jed-spill-${process.pid}-${spillSeq++}.tmp`);
    writeFileSync(path, bytes);
    return new FileSpillRun(path);
  }
}

// FileSpillRun is a written run file; open() streams it back once (the k-way merge opens each run once).
class FileSpillRun implements SpillRun {
  private path: string;

  constructor(path: string) {
    this.path = path;
  }

  open(): SpillByteReader {
    return new FileSpillReader(this.path);
  }
}

const READ_CHUNK = 1 << 16; // 64 KiB — a bounded read buffer per open run

// FileSpillReader is the streaming byte reader over one run file (a bounded chunk buffer keeps peak
// memory at one chunk per run). close() closes the fd and deletes the run file — eager cleanup so no
// temp file leaks even when a LIMIT stops the merge early (spill.md §4).
class FileSpillReader implements SpillByteReader {
  private path: string;
  private fd: number;
  private filePos = 0;
  private chunk = Buffer.allocUnsafe(READ_CHUNK);
  private chunkLen = 0;
  private chunkPos = 0;
  private closed = false;

  constructor(path: string) {
    this.path = path;
    this.fd = openSync(path, "r");
  }

  private refill(): void {
    this.chunkLen = readSync(this.fd, this.chunk, 0, this.chunk.length, this.filePos);
    this.filePos += this.chunkLen;
    this.chunkPos = 0;
  }

  byte(): number {
    if (this.chunkPos >= this.chunkLen) {
      this.refill();
      if (this.chunkLen === 0) throw new Error("unexpected EOF in spill run");
    }
    return this.chunk[this.chunkPos++]!;
  }

  bytes(n: number): Uint8Array {
    const out = new Uint8Array(n);
    let off = 0;
    while (off < n) {
      if (this.chunkPos >= this.chunkLen) {
        this.refill();
        if (this.chunkLen === 0) throw new Error("unexpected EOF in spill run");
      }
      const take = Math.min(n - off, this.chunkLen - this.chunkPos);
      out.set(this.chunk.subarray(this.chunkPos, this.chunkPos + take), off);
      this.chunkPos += take;
      off += take;
    }
    return out;
  }

  u64(): bigint {
    let n = 0n;
    for (let i = 0n; i < 8n; i++) n |= BigInt(this.byte()) << (i * 8n);
    return n;
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    closeSync(this.fd);
    try {
      unlinkSync(this.path);
    } catch {
      // best-effort cleanup
    }
  }
}
