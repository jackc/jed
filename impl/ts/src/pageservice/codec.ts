// The page-service wire codec (spec/pageservice/README.md; spec/design/page-service.md phase 1):
// framing, every message body in spec/pageservice/messages.toml, and the owner record (README §5.1).
// Hand-written like the file-format codec and pinned by spec/pageservice/fixtures.toml; the Ruby
// reference is spec/pageservice/verify.rb, and this decoder rejects exactly what it rejects.
//
// Browser-safe: no node: imports, Uint8Array in and out. Integers are big-endian (DataView); u64
// fields are bigint, u8/u16/u32 are number. Every decode failure is a protocol violation (08P01).
// Counts and lengths are checked against the remaining bytes before anything is allocated, so a
// forged count cannot make the decoder allocate more than the frame holds. Decoded byte fields
// (id16, pages) are copies, never views of the input buffer.

import { crc32Ieee } from "../crc32.ts";
import { type EngineError, engineError } from "../errors.ts";

// --- constants (messages.toml top-level keys) ----------------------------------------------------

export const PS_PROTOCOL_VERSION = 1;
export const PS_MAX_FRAME_LENGTH = 16777216;
// length u32 + code u8 + request_id u32
export const PS_HEADER_LENGTH = 9;
export const PS_MAX_READ_PAGES_CAP = 1024;
export const PS_MAX_INVALIDATION_ENTRIES = 65536;
export const PS_INVALIDATION_WINDOW_COMMITS = 256;
export const PS_ENDPOINT_MAX_PATH_BYTES = 100;
export const PS_OWNER_RECORD_MAGIC = "JEDOWNER";
export const PS_OWNER_RECORD_VERSION = 1;

// The smallest legal `length` field: code u8 + request_id u32 with an empty body.
const MIN_LENGTH = 5;

// --- message codes (messages.toml [[message]] code) -----------------------------------------------

export const PS_CODE_HELLO = 0x01;
export const PS_CODE_BEGIN_READ = 0x02;
export const PS_CODE_READ_PAGES = 0x03;
export const PS_CODE_END_READ = 0x04;
export const PS_CODE_BEGIN_WRITE = 0x05;
export const PS_CODE_COMMIT_BEGIN = 0x06;
export const PS_CODE_COMMIT_PAGES = 0x07;
export const PS_CODE_COMMIT_END = 0x08;
export const PS_CODE_ABORT = 0x09;
export const PS_CODE_COMMIT_STATUS = 0x0a;
export const PS_CODE_REQUEST_OWNERSHIP = 0x0b;
// Reserved in protocol 1 (phases 6 and 7 define their bodies): decoding either is malformed.
export const PS_CODE_SNAPSHOT_STREAM = 0x0c;
export const PS_CODE_SUBSCRIBE = 0x0d;
export const PS_CODE_HELLO_OK = 0x41;
export const PS_CODE_READ_BEGUN = 0x42;
export const PS_CODE_PAGES = 0x43;
export const PS_CODE_WRITE_BEGUN = 0x45;
export const PS_CODE_COMMITTED = 0x48;
export const PS_CODE_COMMIT_STATUS_REPLY = 0x4a;
export const PS_CODE_OWNERSHIP_REPLY = 0x4b;
export const PS_CODE_NOTICE = 0x7e;
export const PS_CODE_ERROR = 0x7f;

// --- flag bits and enumerated byte values (README §3) ---------------------------------------------

// hello.flags
export const PS_HELLO_FLAG_READ_ONLY = 0x01;
// hello_ok.flags
export const PS_HELLO_OK_FLAG_CONNECTION_READ_ONLY = 0x01;
export const PS_HELLO_OK_FLAG_OWNER_READ_ONLY = 0x02;
// error.flags
export const PS_ERROR_FLAG_FATAL = 0x01;
// read_begun.invalidate / write_begun.invalidate
export const PS_INVALIDATE_EXACT = 0;
export const PS_INVALIDATE_ALL = 1;
// commit_status_reply.status
export const PS_COMMIT_STATUS_COMMITTED = 0;
export const PS_COMMIT_STATUS_NOT_COMMITTED = 1;
export const PS_COMMIT_STATUS_UNKNOWN = 2;
// ownership_reply.status
export const PS_OWNERSHIP_YIELDING = 0;
export const PS_OWNERSHIP_REFUSED = 1;
// notice.kind
export const PS_NOTICE_YIELD_BEGUN = 1;
export const PS_NOTICE_YIELD_CANCELLED = 2;
export const PS_NOTICE_SHUTDOWN_BEGUN = 3;
// owner record flags
export const PS_OWNER_FLAG_SERVING = 0x01;

// psMaxReadPages is hello_ok's max_read_pages for a page size: the most indices one read_pages may
// name, so that the `pages` reply fits in one frame (README §3.1).
export function psMaxReadPages(pageSize: number): number {
  return Math.min(
    PS_MAX_READ_PAGES_CAP,
    Math.floor((PS_MAX_FRAME_LENGTH - PS_HEADER_LENGTH) / pageSize),
  );
}

// --- messages -------------------------------------------------------------------------------------

export type PsIndexedPage = { index: number; page: Uint8Array };

export type PsHello = {
  kind: "hello";
  protocolMin: number;
  protocolMax: number;
  formatVersion: number;
  flags: number;
  clientName: string;
};
export type PsBeginRead = { kind: "begin_read"; incarnation: Uint8Array; lastTxid: bigint };
export type PsReadPages = { kind: "read_pages"; txid: bigint; indices: number[] };
export type PsEndRead = { kind: "end_read"; txid: bigint };
export type PsBeginWrite = {
  kind: "begin_write";
  incarnation: Uint8Array;
  lastTxid: bigint;
  lockTimeoutMs: bigint;
};
export type PsCommitBegin = {
  kind: "commit_begin";
  txid: bigint;
  meta: Uint8Array;
  manifestPages: PsIndexedPage[];
};
export type PsCommitPages = { kind: "commit_pages"; pages: PsIndexedPage[] };
export type PsCommitEnd = { kind: "commit_end" };
export type PsAbort = { kind: "abort" };
export type PsCommitStatus = { kind: "commit_status"; txid: bigint; digest: bigint };
export type PsRequestOwnership = { kind: "request_ownership" };
export type PsHelloOk = {
  kind: "hello_ok";
  protocol: number;
  formatVersion: number;
  pageSize: number;
  connectionId: bigint;
  flags: number;
  maxReadPages: number;
};
export type PsReadBegun = {
  kind: "read_begun";
  incarnation: Uint8Array;
  txid: bigint;
  invalidate: number;
  indices: number[];
  meta: Uint8Array;
};
export type PsPages = { kind: "pages"; pages: Uint8Array[] };
export type PsWriteBegun = {
  kind: "write_begun";
  incarnation: Uint8Array;
  txid: bigint;
  invalidate: number;
  indices: number[];
  reuseGrant: boolean;
  meta: Uint8Array;
};
export type PsCommitted = { kind: "committed"; txid: bigint };
export type PsCommitStatusReply = { kind: "commit_status_reply"; status: number };
export type PsOwnershipReply = { kind: "ownership_reply"; status: number };
export type PsNotice = { kind: "notice"; noticeKind: number; connectionId: bigint };
export type PsError = {
  kind: "error";
  sqlstate: string;
  message: string;
  detail: string;
  flags: number;
};

// PsMessage is every message with a defined body in protocol 1 (the reserved snapshot_stream and
// subscribe have none). notice's catalog field `kind` is `noticeKind` here, since `kind` is the
// union's discriminant.
export type PsMessage =
  | PsHello
  | PsBeginRead
  | PsReadPages
  | PsEndRead
  | PsBeginWrite
  | PsCommitBegin
  | PsCommitPages
  | PsCommitEnd
  | PsAbort
  | PsCommitStatus
  | PsRequestOwnership
  | PsHelloOk
  | PsReadBegun
  | PsPages
  | PsWriteBegun
  | PsCommitted
  | PsCommitStatusReply
  | PsOwnershipReply
  | PsNotice
  | PsError;

export type PsFrame = { requestId: number; message: PsMessage };

// PS_MESSAGE_CODES maps each message kind to its wire code.
export const PS_MESSAGE_CODES: Record<PsMessage["kind"], number> = {
  hello: PS_CODE_HELLO,
  begin_read: PS_CODE_BEGIN_READ,
  read_pages: PS_CODE_READ_PAGES,
  end_read: PS_CODE_END_READ,
  begin_write: PS_CODE_BEGIN_WRITE,
  commit_begin: PS_CODE_COMMIT_BEGIN,
  commit_pages: PS_CODE_COMMIT_PAGES,
  commit_end: PS_CODE_COMMIT_END,
  abort: PS_CODE_ABORT,
  commit_status: PS_CODE_COMMIT_STATUS,
  request_ownership: PS_CODE_REQUEST_OWNERSHIP,
  hello_ok: PS_CODE_HELLO_OK,
  read_begun: PS_CODE_READ_BEGUN,
  pages: PS_CODE_PAGES,
  write_begun: PS_CODE_WRITE_BEGUN,
  committed: PS_CODE_COMMITTED,
  commit_status_reply: PS_CODE_COMMIT_STATUS_REPLY,
  ownership_reply: PS_CODE_OWNERSHIP_REPLY,
  notice: PS_CODE_NOTICE,
  error: PS_CODE_ERROR,
};

function violation(message: string): EngineError {
  return engineError("protocol_violation", `page service: ${message}`);
}

// --- writer ---------------------------------------------------------------------------------------

const UTF8_ENCODER = new TextEncoder();
// fatal: invalid UTF-8 throws rather than decoding to U+FFFD. ignoreBOM: a leading U+FEFF is text,
// not a byte-order mark to strip, so a string round-trips exactly.
const UTF8_DECODER = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true });

const U64_MAX = 0xffffffffffffffffn;
const SQLSTATE_PATTERN = /^[0-9A-Z]{5}$/;

// Writer accumulates big-endian bytes, validating every value it is given. `pageSize` is the one
// page length every page in the message must share (0 until the first page fixes it, unless the
// caller supplied it).
class Writer {
  private buf = new Uint8Array(64);
  private view = new DataView(this.buf.buffer);
  length = 0;
  pageSize: number;

  constructor(pageSize: number) {
    this.pageSize = pageSize;
  }

  private reserve(n: number): number {
    const at = this.length;
    if (at + n > this.buf.length) {
      let size = this.buf.length * 2;
      while (size < at + n) size *= 2;
      const grown = new Uint8Array(size);
      grown.set(this.buf.subarray(0, at));
      this.buf = grown;
      this.view = new DataView(grown.buffer);
    }
    this.length = at + n;
    return at;
  }

  bytes(): Uint8Array {
    return this.buf.slice(0, this.length);
  }

  u8(v: number): void {
    checkUint(v, 0xff, "u8");
    const at = this.reserve(1);
    this.view.setUint8(at, v);
  }

  u16(v: number): void {
    checkUint(v, 0xffff, "u16");
    const at = this.reserve(2);
    this.view.setUint16(at, v);
  }

  u32(v: number): void {
    checkUint(v, 0xffffffff, "u32");
    const at = this.reserve(4);
    this.view.setUint32(at, v);
  }

  u64(v: bigint): void {
    if (v < 0n || v > U64_MAX) throw violation(`u64 ${v} out of range`);
    const at = this.reserve(8);
    this.view.setBigUint64(at, v);
  }

  bool(v: boolean): void {
    const at = this.reserve(1);
    this.view.setUint8(at, v ? 1 : 0);
  }

  raw(b: Uint8Array): void {
    const at = this.reserve(b.length); // may replace this.buf, so reserve before reading it
    this.buf.set(b, at);
  }

  id16(b: Uint8Array): void {
    if (b.length !== 16) throw violation(`id16 of ${b.length} bytes`);
    this.raw(b);
  }

  sqlstate(s: string): void {
    if (!SQLSTATE_PATTERN.test(s)) throw violation(`sqlstate ${JSON.stringify(s)}`);
    this.raw(UTF8_ENCODER.encode(s));
  }

  string(s: string): void {
    if (!s.isWellFormed()) throw violation("string is not well-formed UTF-16");
    const b = UTF8_ENCODER.encode(s);
    this.u32(b.length);
    this.raw(b);
  }

  page(p: Uint8Array): void {
    if (p.length === 0) throw violation("empty page");
    if (this.pageSize === 0) this.pageSize = p.length;
    if (p.length !== this.pageSize) {
      throw violation(`page of ${p.length} bytes, expected ${this.pageSize}`);
    }
    this.raw(p);
  }

  u32List(list: number[]): void {
    this.u32(list.length);
    for (const v of list) this.u32(v);
  }

  pageList(list: Uint8Array[]): void {
    this.u32(list.length);
    for (const p of list) this.page(p);
  }

  indexedPageList(list: PsIndexedPage[]): void {
    this.u32(list.length);
    for (const item of list) {
      this.u32(item.index);
      this.page(item.page);
    }
  }
}

function checkUint(v: number, max: number, type: string): void {
  if (!Number.isInteger(v) || v < 0 || v > max) throw violation(`${type} ${v} out of range`);
}

function writeBody(w: Writer, m: PsMessage): void {
  switch (m.kind) {
    case "hello":
      w.u16(m.protocolMin);
      w.u16(m.protocolMax);
      w.u16(m.formatVersion);
      w.u8(m.flags);
      w.string(m.clientName);
      return;
    case "begin_read":
      w.id16(m.incarnation);
      w.u64(m.lastTxid);
      return;
    case "read_pages":
      w.u64(m.txid);
      w.u32List(m.indices);
      return;
    case "end_read":
      w.u64(m.txid);
      return;
    case "begin_write":
      w.id16(m.incarnation);
      w.u64(m.lastTxid);
      w.u64(m.lockTimeoutMs);
      return;
    case "commit_begin":
      w.u64(m.txid);
      w.page(m.meta);
      w.indexedPageList(m.manifestPages);
      return;
    case "commit_pages":
      w.indexedPageList(m.pages);
      return;
    case "commit_end":
    case "abort":
    case "request_ownership":
      return;
    case "commit_status":
      w.u64(m.txid);
      w.u64(m.digest);
      return;
    case "hello_ok":
      w.u16(m.protocol);
      w.u16(m.formatVersion);
      w.u32(m.pageSize);
      w.u64(m.connectionId);
      w.u8(m.flags);
      w.u32(m.maxReadPages);
      return;
    case "read_begun":
      w.id16(m.incarnation);
      w.u64(m.txid);
      w.u8(m.invalidate);
      w.u32List(m.indices);
      w.page(m.meta);
      return;
    case "pages":
      w.pageList(m.pages);
      return;
    case "write_begun":
      w.id16(m.incarnation);
      w.u64(m.txid);
      w.u8(m.invalidate);
      w.u32List(m.indices);
      w.bool(m.reuseGrant);
      w.page(m.meta);
      return;
    case "committed":
      w.u64(m.txid);
      return;
    case "commit_status_reply":
    case "ownership_reply":
      w.u8(m.status);
      return;
    case "notice":
      w.u8(m.noticeKind);
      w.u64(m.connectionId);
      return;
    case "error":
      w.sqlstate(m.sqlstate);
      w.string(m.message);
      w.string(m.detail);
      w.u8(m.flags);
      return;
  }
}

// encodeFrame encodes one complete frame, length prefix included. Every page in the message must
// have the same nonzero length; when `pageSize` is given (the connection's), they must have exactly
// that length. A value out of its field's range, a malformed sqlstate, or a frame longer than
// max_frame_length throws 08P01: the frame would be one a conforming peer rejects.
export function encodeFrame(requestId: number, m: PsMessage, pageSize = 0): Uint8Array {
  const w = new Writer(pageSize);
  w.u32(0); // length, patched below
  w.u8(PS_MESSAGE_CODES[m.kind]);
  w.u32(requestId);
  writeBody(w, m);
  const length = w.length - 4;
  if (length > PS_MAX_FRAME_LENGTH) {
    throw violation(`frame length ${length} exceeds ${PS_MAX_FRAME_LENGTH}`);
  }
  const out = w.bytes();
  new DataView(out.buffer).setUint32(0, length);
  return out;
}

// --- reader ---------------------------------------------------------------------------------------

// Reader decodes big-endian fields from bytes[pos, end), throwing 08P01 on any read past `end`.
class Reader {
  private bytes: Uint8Array;
  private view: DataView;
  private pos: number;
  private end: number;
  private pageSize: number;

  constructor(bytes: Uint8Array, start: number, end: number, pageSize: number) {
    this.bytes = bytes;
    this.view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    this.pos = start;
    this.end = end;
    this.pageSize = pageSize;
  }

  remaining(): number {
    return this.end - this.pos;
  }

  private take(n: number): number {
    if (n > this.remaining()) throw violation(`truncated: need ${n} bytes at ${this.pos}`);
    const at = this.pos;
    this.pos += n;
    return at;
  }

  u8(): number {
    return this.view.getUint8(this.take(1));
  }

  u16(): number {
    return this.view.getUint16(this.take(2));
  }

  u32(): number {
    return this.view.getUint32(this.take(4));
  }

  u64(): bigint {
    return this.view.getBigUint64(this.take(8));
  }

  bool(): boolean {
    const b = this.u8();
    if (b > 1) throw violation(`bool byte ${b}`);
    return b === 1;
  }

  copy(n: number): Uint8Array {
    const at = this.take(n);
    return this.bytes.slice(at, at + n);
  }

  id16(): Uint8Array {
    return this.copy(16);
  }

  sqlstate(): string {
    const at = this.take(5);
    let s = "";
    for (let i = at; i < at + 5; i++) {
      const c = this.bytes[i]!;
      const digit = c >= 0x30 && c <= 0x39;
      const upper = c >= 0x41 && c <= 0x5a;
      if (!digit && !upper) throw violation("sqlstate characters must be 0-9 or A-Z");
      s += String.fromCharCode(c);
    }
    return s;
  }

  string(): string {
    const length = this.u32();
    if (length > this.remaining()) throw violation(`string length ${length} exceeds the body`);
    const at = this.take(length);
    try {
      return UTF8_DECODER.decode(this.bytes.subarray(at, at + length));
    } catch {
      throw violation("string is not valid UTF-8");
    }
  }

  page(): Uint8Array {
    if (this.pageSize <= 0) throw violation("page field without a connection page size");
    return this.copy(this.pageSize);
  }

  // count reads a list count and checks that `count` items of `itemBytes` fit in what remains.
  private count(itemBytes: number): number {
    const count = this.u32();
    if (count * itemBytes > this.remaining()) {
      throw violation(`${count} items of ${itemBytes} bytes exceed the body`);
    }
    return count;
  }

  u32List(): number[] {
    const count = this.count(4);
    const out: number[] = [];
    for (let i = 0; i < count; i++) out.push(this.u32());
    return out;
  }

  pageList(): Uint8Array[] {
    const count = this.count(this.pageSize);
    const out: Uint8Array[] = [];
    for (let i = 0; i < count; i++) out.push(this.page());
    return out;
  }

  indexedPageList(): PsIndexedPage[] {
    const count = this.count(4 + this.pageSize);
    const out: PsIndexedPage[] = [];
    for (let i = 0; i < count; i++) {
      const index = this.u32();
      out.push({ index, page: this.page() });
    }
    return out;
  }
}

function readBody(r: Reader, code: number): PsMessage {
  switch (code) {
    case PS_CODE_HELLO:
      return {
        kind: "hello",
        protocolMin: r.u16(),
        protocolMax: r.u16(),
        formatVersion: r.u16(),
        flags: r.u8(),
        clientName: r.string(),
      };
    case PS_CODE_BEGIN_READ:
      return { kind: "begin_read", incarnation: r.id16(), lastTxid: r.u64() };
    case PS_CODE_READ_PAGES:
      return { kind: "read_pages", txid: r.u64(), indices: r.u32List() };
    case PS_CODE_END_READ:
      return { kind: "end_read", txid: r.u64() };
    case PS_CODE_BEGIN_WRITE:
      return {
        kind: "begin_write",
        incarnation: r.id16(),
        lastTxid: r.u64(),
        lockTimeoutMs: r.u64(),
      };
    case PS_CODE_COMMIT_BEGIN:
      return {
        kind: "commit_begin",
        txid: r.u64(),
        meta: r.page(),
        manifestPages: r.indexedPageList(),
      };
    case PS_CODE_COMMIT_PAGES:
      return { kind: "commit_pages", pages: r.indexedPageList() };
    case PS_CODE_COMMIT_END:
      return { kind: "commit_end" };
    case PS_CODE_ABORT:
      return { kind: "abort" };
    case PS_CODE_COMMIT_STATUS:
      return { kind: "commit_status", txid: r.u64(), digest: r.u64() };
    case PS_CODE_REQUEST_OWNERSHIP:
      return { kind: "request_ownership" };
    case PS_CODE_HELLO_OK:
      return {
        kind: "hello_ok",
        protocol: r.u16(),
        formatVersion: r.u16(),
        pageSize: r.u32(),
        connectionId: r.u64(),
        flags: r.u8(),
        maxReadPages: r.u32(),
      };
    case PS_CODE_READ_BEGUN:
      return {
        kind: "read_begun",
        incarnation: r.id16(),
        txid: r.u64(),
        invalidate: r.u8(),
        indices: r.u32List(),
        meta: r.page(),
      };
    case PS_CODE_PAGES:
      return { kind: "pages", pages: r.pageList() };
    case PS_CODE_WRITE_BEGUN:
      return {
        kind: "write_begun",
        incarnation: r.id16(),
        txid: r.u64(),
        invalidate: r.u8(),
        indices: r.u32List(),
        reuseGrant: r.bool(),
        meta: r.page(),
      };
    case PS_CODE_COMMITTED:
      return { kind: "committed", txid: r.u64() };
    case PS_CODE_COMMIT_STATUS_REPLY:
      return { kind: "commit_status_reply", status: r.u8() };
    case PS_CODE_OWNERSHIP_REPLY:
      return { kind: "ownership_reply", status: r.u8() };
    case PS_CODE_NOTICE:
      return { kind: "notice", noticeKind: r.u8(), connectionId: r.u64() };
    case PS_CODE_ERROR:
      return {
        kind: "error",
        sqlstate: r.sqlstate(),
        message: r.string(),
        detail: r.string(),
        flags: r.u8(),
      };
    default:
      // Unknown, and the reserved snapshot_stream / subscribe.
      throw violation(`unknown message code 0x${code.toString(16).padStart(2, "0")}`);
  }
}

// checkLength rejects a frame `length` field outside [5, max_frame_length].
function checkLength(length: number): void {
  if (length < MIN_LENGTH || length > PS_MAX_FRAME_LENGTH) {
    throw violation(`frame length ${length} outside [${MIN_LENGTH}, ${PS_MAX_FRAME_LENGTH}]`);
  }
}

// decodeFrame decodes exactly one complete frame, length prefix included. `pageSize` is the
// connection's page size (from hello_ok); 0 before it is known, which makes any page field
// malformed. A short frame, a frame with bytes beyond its length, a bad length, an unknown or
// reserved code, a malformed field, or bytes after the last field throws 08P01.
export function decodeFrame(frame: Uint8Array, pageSize: number): PsFrame {
  if (frame.length < 4) throw violation("frame shorter than its length field");
  const view = new DataView(frame.buffer, frame.byteOffset, frame.byteLength);
  const length = view.getUint32(0);
  checkLength(length);
  if (frame.length - 4 > length) throw violation("frame bytes beyond its length");
  if (frame.length - 4 < length) throw violation("frame shorter than its length");
  const code = view.getUint8(4);
  const requestId = view.getUint32(5);
  const r = new Reader(frame, PS_HEADER_LENGTH, frame.length, pageSize);
  const message = readBody(r, code);
  if (r.remaining() !== 0) throw violation(`${r.remaining()} bytes after the last field`);
  return { requestId, message };
}

// --- frame accumulator ----------------------------------------------------------------------------

// FrameAccumulator reassembles frames from an evented byte stream. push() appends a chunk; next()
// returns the next complete decoded frame or null when one has not fully arrived. A bad `length`
// throws 08P01 as soon as its 4 bytes are buffered (from push or next), before the body is awaited.
// After any throw the stream is unusable: the connection must be closed.
export class FrameAccumulator {
  private buf = new Uint8Array(0);
  private start = 0;
  private end = 0;

  // buffered returns the number of bytes received but not yet consumed by next().
  buffered(): number {
    return this.end - this.start;
  }

  push(chunk: Uint8Array): void {
    if (this.end + chunk.length > this.buf.length) {
      const live = this.end - this.start;
      const needed = live + chunk.length;
      if (needed > this.buf.length) {
        let size = Math.max(this.buf.length * 2, 256);
        while (size < needed) size *= 2;
        const grown = new Uint8Array(size);
        grown.set(this.buf.subarray(this.start, this.end));
        this.buf = grown;
      } else {
        this.buf.copyWithin(0, this.start, this.end);
      }
      this.start = 0;
      this.end = live;
    }
    this.buf.set(chunk, this.end);
    this.end += chunk.length;
    this.pendingLength();
  }

  // pendingLength returns the buffered head frame's checked length field, or -1 with fewer than 4
  // bytes buffered.
  private pendingLength(): number {
    if (this.end - this.start < 4) return -1;
    const b = this.buf;
    const s = this.start;
    const length = ((b[s]! << 24) | (b[s + 1]! << 16) | (b[s + 2]! << 8) | b[s + 3]!) >>> 0;
    checkLength(length);
    return length;
  }

  next(pageSize: number): PsFrame | null {
    const length = this.pendingLength();
    if (length < 0 || this.end - this.start < 4 + length) return null;
    const frameEnd = this.start + 4 + length;
    const frame = decodeFrame(this.buf.subarray(this.start, frameEnd), pageSize);
    this.start = frameEnd;
    if (this.start === this.end) {
      this.start = 0;
      this.end = 0;
    }
    return frame;
  }
}

// --- owner record (README §5.1) -------------------------------------------------------------------

export type PsOwnerRecord = {
  serving: boolean;
  uid: number;
  incarnation: Uint8Array;
  pid: number;
  endpoint: string;
};

const OWNER_MAGIC_BYTES = UTF8_ENCODER.encode(PS_OWNER_RECORD_MAGIC);

// encodeOwnerRecord encodes `<path>.lock/owner`: magic, record_version, flags, uid, incarnation,
// pid, endpoint, then the CRC-32/IEEE of every preceding byte.
export function encodeOwnerRecord(rec: PsOwnerRecord): Uint8Array {
  const w = new Writer(0);
  w.raw(OWNER_MAGIC_BYTES);
  w.u16(PS_OWNER_RECORD_VERSION);
  w.u8(rec.serving ? PS_OWNER_FLAG_SERVING : 0);
  w.u32(rec.uid);
  w.id16(rec.incarnation);
  w.u32(rec.pid);
  w.string(rec.endpoint);
  const body = w.bytes();
  w.u32(crc32Ieee(body));
  return w.bytes();
}

// decodeOwnerRecord decodes an owner record, throwing 08P01 on a wrong magic or version, reserved
// flag bits, a truncated field, trailing bytes, or a CRC mismatch. A caller treats a record that
// fails to decode as absent (README §5.1).
export function decodeOwnerRecord(bytes: Uint8Array): PsOwnerRecord {
  const r = new Reader(bytes, 0, bytes.length, 0);
  const magic = r.copy(OWNER_MAGIC_BYTES.length);
  for (let i = 0; i < magic.length; i++) {
    if (magic[i] !== OWNER_MAGIC_BYTES[i]) throw violation("owner record magic");
  }
  if (r.u16() !== PS_OWNER_RECORD_VERSION) throw violation("owner record version");
  const flags = r.u8();
  if ((flags & ~PS_OWNER_FLAG_SERVING) !== 0) throw violation(`owner record flags ${flags}`);
  const uid = r.u32();
  const incarnation = r.id16();
  const pid = r.u32();
  const endpoint = r.string();
  const crc = r.u32();
  if (r.remaining() !== 0) throw violation("owner record trailing bytes");
  if (crc !== crc32Ieee(bytes.subarray(0, bytes.length - 4))) throw violation("owner record crc");
  return { serving: flags === PS_OWNER_FLAG_SERVING, uid, incarnation, pid, endpoint };
}
