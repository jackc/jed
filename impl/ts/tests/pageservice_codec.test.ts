// The page-service wire codec against the shared byte fixtures (spec/pageservice/fixtures.toml) and
// the message catalog (spec/pageservice/messages.toml): byte-level checks the SQL corpus cannot
// express (CLAUDE.md §10).

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { crc32Ieee } from "../src/crc32.ts";
import { EngineError } from "../src/errors.ts";
import {
  decodeFrame,
  decodeOwnerRecord,
  encodeFrame,
  encodeOwnerRecord,
  FrameAccumulator,
  PS_ENDPOINT_MAX_PATH_BYTES,
  PS_HEADER_LENGTH,
  PS_INVALIDATION_WINDOW_COMMITS,
  PS_MAX_FRAME_LENGTH,
  PS_MAX_INVALIDATION_ENTRIES,
  PS_MAX_READ_PAGES_CAP,
  PS_MESSAGE_CODES,
  PS_OWNER_RECORD_MAGIC,
  PS_OWNER_RECORD_VERSION,
  PS_PROTOCOL_VERSION,
  type PsMessage,
  type PsOwnerRecord,
  psMaxReadPages,
} from "../src/pageservice/codec.ts";
import { readTomlTables, specPath, type TomlRow } from "./tomlmini.ts";

const MESSAGES_PATH = specPath("pageservice/messages.toml");
const FIXTURES_PATH = specPath("pageservice/fixtures.toml");
const CATALOG = readTomlTables(MESSAGES_PATH, "message");
const FRAMES = readTomlTables(FIXTURES_PATH, "frame");
const OWNER_RECORDS = readTomlTables(FIXTURES_PATH, "owner_record");
const MALFORMED = readTomlTables(FIXTURES_PATH, "malformed");

// The codec's field names per catalog message, in catalog order. notice's `kind` is `noticeKind`
// in the codec (its `kind` is the union discriminant).
const FIELD_NAMES: Record<string, string> = {
  protocol_min: "protocolMin",
  protocol_max: "protocolMax",
  format_version: "formatVersion",
  client_name: "clientName",
  last_txid: "lastTxid",
  lock_timeout_ms: "lockTimeoutMs",
  manifest_pages: "manifestPages",
  page_size: "pageSize",
  connection_id: "connectionId",
  max_read_pages: "maxReadPages",
  reuse_grant: "reuseGrant",
};

function codecFieldName(message: string, field: string): string {
  if (message === "notice" && field === "kind") return "noticeKind";
  return FIELD_NAMES[field] ?? field;
}

type CatalogField = { name: string; type: string };

function catalogFields(row: TomlRow): CatalogField[] {
  return row.strs("fields").map((f) => {
    const [name, type] = f.split(":");
    return { name: name!, type: type! };
  });
}

function catalogMessage(name: string): TomlRow {
  const row = CATALOG.find((m) => m.str("name") === name);
  if (row === undefined) throw new Error(`no catalog message ${name}`);
  return row;
}

function hexToBytes(hex: string): Uint8Array {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}

function bytesToHex(b: Uint8Array): string {
  return Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
}

// parsePage reads the fixtures' "<prefix hex>/<fill hex>" page text form.
function parsePage(text: string, pageSize: number): Uint8Array {
  const [prefix, fill] = text.split("/");
  const page = new Uint8Array(pageSize).fill(parseInt(fill!, 16));
  page.set(hexToBytes(prefix!));
  return page;
}

// parseValue converts a fixture's f_* text form to the codec's value for a catalog field type.
function parseValue(type: string, text: string, pageSize: number): unknown {
  switch (type) {
    case "u8":
    case "u16":
    case "u32":
      return Number(text);
    case "u64":
      return BigInt(text);
    case "bool":
      return text === "true";
    case "id16":
      return hexToBytes(text);
    case "sqlstate":
    case "string":
      return text;
    case "credential": {
      if (text === "") return null;
      const [kind, rest] = [text.slice(0, text.indexOf(":")), text.slice(text.indexOf(":") + 1)];
      return kind === "passphrase" ? { passphrase: rest } : { key: hexToBytes(rest) };
    }
    case "page":
      return parsePage(text, pageSize);
    case "u32_list":
      return text === "" ? [] : text.split(",").map(Number);
    case "page_list":
      return text === "" ? [] : text.split(";").map((p) => parsePage(p, pageSize));
    case "indexed_page_list":
      return text === ""
        ? []
        : text.split(";").map((item) => {
            const [index, page] = item.split("=");
            return { index: Number(index), page: parsePage(page!, pageSize) };
          });
    default:
      throw new Error(`unknown field type ${type}`);
  }
}

function buildMessage(name: string, row: TomlRow, pageSize: number): PsMessage {
  const m: Record<string, unknown> = { kind: name };
  for (const f of catalogFields(catalogMessage(name))) {
    m[codecFieldName(name, f.name)] = parseValue(f.type, row.str(`f_${f.name}`), pageSize);
  }
  return m as PsMessage;
}

function assertViolation(fn: () => unknown, label: string): void {
  assert.throws(
    fn,
    (e: unknown) => e instanceof EngineError && e.code() === "08P01",
    `${label}: expected 08P01`,
  );
}

test("frame fixtures encode to their bytes and decode back", () => {
  assert.ok(FRAMES.length > 0);
  for (const row of FRAMES) {
    const name = row.str("name");
    const pageSize = row.num("page_size");
    const requestId = row.num("request_id");
    const message = buildMessage(row.str("message"), row, pageSize);
    assert.equal(bytesToHex(encodeFrame(requestId, message)), row.str("hex"), `${name}: encode`);
    if (pageSize > 0) {
      assert.equal(bytesToHex(encodeFrame(requestId, message, pageSize)), row.str("hex"), name);
    }
    const decoded = decodeFrame(hexToBytes(row.str("hex")), pageSize);
    assert.deepEqual(decoded, { requestId, message }, `${name}: decode`);
  }
});

test("owner record fixtures encode to their bytes and decode back", () => {
  assert.ok(OWNER_RECORDS.length > 0);
  for (const row of OWNER_RECORDS) {
    const rec: PsOwnerRecord = {
      serving: row.num("f_flags") === 1,
      uid: row.num("f_uid"),
      incarnation: hexToBytes(row.str("f_incarnation")),
      pid: row.num("f_pid"),
      endpoint: row.str("f_endpoint"),
    };
    assert.equal(bytesToHex(encodeOwnerRecord(rec)), row.str("hex"), row.str("name"));
    assert.deepEqual(decodeOwnerRecord(hexToBytes(row.str("hex"))), rec, row.str("name"));
  }
});

test("owner record decoding rejects reserved flags, bad magic/version/crc, and trailing bytes", () => {
  const good = hexToBytes(OWNER_RECORDS[0]!.str("hex"));
  const mutate = (at: number, xor: number): Uint8Array => {
    const b = good.slice();
    b[at]! ^= xor;
    return b;
  };
  assertViolation(() => decodeOwnerRecord(mutate(0, 0x20)), "magic");
  assertViolation(() => decodeOwnerRecord(mutate(9, 0x02)), "version");
  assertViolation(() => decodeOwnerRecord(mutate(10, 0x02)), "reserved flag bit");
  assertViolation(() => decodeOwnerRecord(mutate(good.length - 1, 0x01)), "crc");
  assertViolation(() => decodeOwnerRecord(good.subarray(0, good.length - 1)), "truncated");
  const trailing = new Uint8Array(good.length + 1);
  trailing.set(good);
  assertViolation(() => decodeOwnerRecord(trailing), "trailing");
  // A reserved flag bit with a matching CRC is still rejected.
  const flagged = good.slice();
  flagged[10] = 0x03;
  const crcAt = flagged.length - 4;
  new DataView(flagged.buffer).setUint32(crcAt, crc32Ieee(flagged.subarray(0, crcAt)));
  assertViolation(() => decodeOwnerRecord(flagged), "reserved flag bit, valid crc");
});

test("malformed fixtures are rejected with 08P01", () => {
  assert.ok(MALFORMED.length > 0);
  for (const row of MALFORMED) {
    assertViolation(
      () => decodeFrame(hexToBytes(row.str("hex")), row.num("page_size")),
      row.str("name"),
    );
  }
});

test("frames beyond or short of their length, and invalid encoder input, are rejected", () => {
  const hex = FRAMES.find((r) => r.str("name") === "end_read")!.str("hex");
  const frame = hexToBytes(hex);
  assertViolation(() => decodeFrame(frame.subarray(0, frame.length - 1), 0), "short");
  assertViolation(() => decodeFrame(hexToBytes(`${hex}00`), 0), "beyond length");
  assertViolation(() => decodeFrame(frame.subarray(0, 3), 0), "no length");
  // A page field with no connection page size.
  const pages = FRAMES.find((r) => r.str("name") === "pages")!;
  assertViolation(() => decodeFrame(hexToBytes(pages.str("hex")), 0), "page size 0");
  // A lone surrogate is not valid UTF-8 text; a leading U+FEFF round-trips as text.
  const err = (sqlstate: string, message: string): PsMessage => ({
    kind: "error",
    sqlstate,
    message,
    detail: "",
    flags: 0,
  });
  assertViolation(() => encodeFrame(1, err("08P01", "\ud800")), "lone surrogate");
  assertViolation(() => encodeFrame(1, err("08p01", "")), "lower-case sqlstate");
  assert.deepEqual(decodeFrame(encodeFrame(1, err("08P01", "﻿x")), 0).message, err("08P01", "﻿x"));
  assertViolation(() => encodeFrame(0x1_0000_0000, { kind: "abort" }), "request id range");
  assertViolation(() => encodeFrame(1, { kind: "end_read", txid: -1n }), "u64 range");
  assertViolation(
    () => encodeFrame(1, { kind: "pages", pages: [new Uint8Array(4), new Uint8Array(8)] }),
    "mixed page lengths",
  );
  assertViolation(
    () => encodeFrame(1, { kind: "pages", pages: [new Uint8Array(4)] }, 8),
    "page size mismatch",
  );
});

test("FrameAccumulator reassembles frames delivered a byte at a time", () => {
  const a = FRAMES.find((r) => r.str("name") === "read_begun_list")!;
  const b = FRAMES.find((r) => r.str("name") === "pages")!;
  const bytes = hexToBytes(a.str("hex") + b.str("hex"));
  const acc = new FrameAccumulator();
  const got: unknown[] = [];
  for (const byte of bytes) {
    acc.push(Uint8Array.of(byte));
    for (;;) {
      const f = acc.next(256);
      if (f === null) break;
      got.push(f);
    }
  }
  assert.deepEqual(got, [
    decodeFrame(hexToBytes(a.str("hex")), 256),
    decodeFrame(hexToBytes(b.str("hex")), 256),
  ]);
  assert.equal(acc.buffered(), 0);
});

test("FrameAccumulator rejects a bad length as soon as its 4 bytes arrive", () => {
  const acc = new FrameAccumulator();
  acc.push(Uint8Array.of(0x01, 0x00, 0x00));
  assert.equal(acc.next(0), null);
  assertViolation(() => acc.push(Uint8Array.of(0x01)), "length above max");
  const small = new FrameAccumulator();
  assertViolation(() => small.push(Uint8Array.of(0, 0, 0, 4)), "length below minimum");
});

// readTopLevel reads the `key = value` lines before a TOML file's first table header.
function readTopLevel(path: string): Map<string, string> {
  const out = new Map<string, string>();
  for (const raw of readFileSync(path, "utf8").split("\n")) {
    const line = raw.trim();
    if (line.startsWith("[")) break;
    if (line === "" || line.startsWith("#")) continue;
    const idx = line.indexOf("=");
    out.set(
      line.slice(0, idx).trim(),
      line
        .slice(idx + 1)
        .trim()
        .replace(/^"|"$/g, ""),
    );
  }
  return out;
}

test("codec constants match messages.toml", () => {
  const top = readTopLevel(MESSAGES_PATH);
  const expect: Record<string, string | number> = {
    protocol_version: PS_PROTOCOL_VERSION,
    max_frame_length: PS_MAX_FRAME_LENGTH,
    header_length: PS_HEADER_LENGTH,
    max_read_pages_cap: PS_MAX_READ_PAGES_CAP,
    max_invalidation_entries: PS_MAX_INVALIDATION_ENTRIES,
    invalidation_window_commits: PS_INVALIDATION_WINDOW_COMMITS,
    endpoint_max_path_bytes: PS_ENDPOINT_MAX_PATH_BYTES,
    owner_record_magic: PS_OWNER_RECORD_MAGIC,
    owner_record_version: PS_OWNER_RECORD_VERSION,
  };
  for (const [key, value] of Object.entries(expect)) {
    const raw = top.get(key);
    assert.ok(raw !== undefined, `messages.toml has ${key}`);
    assert.equal(typeof value === "number" ? Number(raw) : raw, value, key);
  }
  assert.equal(psMaxReadPages(4096), 1024);
  assert.equal(psMaxReadPages(65536), 255);
});

// zeroValue is a placeholder value of a catalog field type, for the shape check below.
function zeroValue(type: string, pageSize: number): unknown {
  switch (type) {
    case "u64":
      return 7n;
    case "bool":
      return true;
    case "id16":
      return new Uint8Array(16).fill(3);
    case "sqlstate":
      return "XX000";
    case "string":
      return "s";
    case "credential":
      return { passphrase: "p" };
    case "page":
      return new Uint8Array(pageSize).fill(9);
    case "u32_list":
      return [1, 2];
    case "page_list":
      return [new Uint8Array(pageSize).fill(1)];
    case "indexed_page_list":
      return [{ index: 5, page: new Uint8Array(pageSize).fill(2) }];
    default:
      return 1;
  }
}

test("every non-reserved catalog message's code and fields match the codec", () => {
  const defined = CATALOG.filter((m) => !(m.has("reply") && m.str("reply") === "reserved"));
  assert.deepEqual(
    defined.map((m) => m.str("name")).sort(),
    Object.keys(PS_MESSAGE_CODES).sort(),
    "the codec defines exactly the non-reserved messages",
  );
  for (const row of defined) {
    const name = row.str("name");
    assert.equal(PS_MESSAGE_CODES[name as PsMessage["kind"]], Number(row.str("code")), name);
    const fields = catalogFields(row);
    const m: Record<string, unknown> = { kind: name };
    for (const f of fields) m[codecFieldName(name, f.name)] = zeroValue(f.type, 32);
    const decoded = decodeFrame(encodeFrame(3, m as PsMessage), 32);
    assert.deepEqual(decoded.message, m, name);
    assert.deepEqual(
      Object.keys(decoded.message),
      ["kind", ...fields.map((f) => codecFieldName(name, f.name))],
      `${name}: field order`,
    );
  }
  // Reserved codes are not decodable.
  for (const row of CATALOG.filter((m) => m.has("reply") && m.str("reply") === "reserved")) {
    const frame = Uint8Array.of(0, 0, 0, 5, Number(row.str("code")), 0, 0, 0, 1);
    assertViolation(() => decodeFrame(frame, 0), row.str("name"));
  }
});

test("the encoder's buffer growth keeps every field across many fixed-width writes", () => {
  const indices = Array.from({ length: 1000 }, (_, i) => i * 7 + 2);
  const m: PsMessage = { kind: "read_pages", txid: 99n, indices };
  const frame = encodeFrame(4, m);
  assert.equal(frame.length, 4 + 5 + 8 + 4 + 4 * indices.length);
  assert.deepEqual(decodeFrame(frame, 0), { requestId: 4, message: m });
});
