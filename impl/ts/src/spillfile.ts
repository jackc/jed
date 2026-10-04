// The Node `fs` spill backing for the external merge sort (spec/design/spill.md §4). This is the ONE
// node:fs-using piece of the sort path, lifted out of spill.ts behind the SpillSink seam so spill.ts —
// and thus the whole executor — imports no `node:*` and lands in a browser bundle (the OPFS host).
// The Node file host (file.ts) injects a FileSpillSink on the Engine handle; an in-memory or OPFS
// database leaves it null and never spills (sorts stay resident — spill.md §2). Node stdlib I/O only
// (no dependency — CLAUDE.md §14); the run file's bytes are a per-core internal codec, never the §8
// on-disk format (spill.md §6).

import { closeSync, openSync, readSync, unlinkSync, writeFileSync, writeSync } from "node:fs";
import { join } from "node:path";
import { engineError } from "./errors.ts";
import type { SpillByteReader, SpillRun, SpillSink, SpillScratch } from "./spill.ts";

// A unique-per-process counter for spill file names (combined with the process id), so concurrent
// sorters never collide. Internal — it never affects results (spill.md §6).
let spillSeq = 0;

// FileSpillSink writes each spilled run to a temp file under the host scratch directory (the Node
// file host defaults to os.tmpdir(), never the database directory) and hands back a FileSpillRun to
// read it lazily. Scratch can be shared, so creation is exclusive and private.
export class FileSpillSink implements SpillSink {
  private dir: string;

  constructor(dir: string) {
    this.dir = dir;
  }

  createScratch(): SpillScratch {
    for (;;) {
      const path = join(this.dir, `jed-spill-${process.pid}-${spillSeq++}.tmp`);
      try {
        return new FileScratch(path, openSync(path, "wx+", 0o600));
      } catch (e) {
        if (!isAlreadyExists(e)) throw spillIoError(e);
      }
    }
  }

  writeRun(bytes: Uint8Array): SpillRun {
    for (;;) {
      const path = join(this.dir, `jed-spill-${process.pid}-${spillSeq++}.tmp`);
      let fd: number;
      try {
        fd = openSync(path, "wx", 0o600);
      } catch (e) {
        if (isAlreadyExists(e)) continue;
        throw spillIoError(e);
      }
      let open = true;
      try {
        writeFileSync(fd, bytes);
        closeSync(fd);
        open = false;
        return new FileSpillRun(path);
      } catch (e) {
        if (open) {
          try {
            closeSync(fd);
          } catch {
            // preserve the original write/close error
          }
        }
        try {
          unlinkSync(path);
        } catch {
          // best-effort cleanup of a partial run
        }
        throw spillIoError(e);
      }
    }
  }
}

function isAlreadyExists(e: unknown): boolean {
  return typeof e === "object" && e !== null && "code" in e && e.code === "EEXIST";
}

function spillIoError(e: unknown): Error {
  const msg = e instanceof Error ? e.message : String(e);
  return engineError("io_error", "I/O error: " + msg);
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

// One descriptor per spool/map, independent of row count and partition skew.
class FileScratch implements SpillScratch {
  size = 0;
  private closed = false;
  private path: string;
  private fd: number;
  constructor(path: string, fd: number) {
    this.path = path;
    this.fd = fd;
  }
  read(position: number, length: number): Uint8Array {
    const bytes = new Uint8Array(length);
    let done = 0;
    try {
      while (done < length) {
        const n = readSync(this.fd, bytes, done, length - done, position + done);
        if (n === 0) throw new Error("unexpected EOF in spill scratch");
        done += n;
      }
      return bytes;
    } catch (e) {
      throw spillIoError(e);
    }
  }
  write(position: number, bytes: Uint8Array): void {
    let done = 0;
    try {
      while (done < bytes.length) {
        const n = writeSync(this.fd, bytes, done, bytes.length - done, position + done);
        if (n === 0) throw new Error("short write in spill scratch");
        done += n;
      }
      this.size = Math.max(this.size, position + bytes.length);
    } catch (e) {
      throw spillIoError(e);
    }
  }
  close(): void {
    if (this.closed) return;
    this.closed = true;
    try {
      closeSync(this.fd);
    } finally {
      try {
        unlinkSync(this.path);
      } catch {}
    }
  }
}
