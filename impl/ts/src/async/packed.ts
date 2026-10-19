// Result-batch transport v1, independent of request value tags and the database file format.
// Each accepted row is copied immediately into one transferable buffer. Logical admission uses
// the existing wire budget, and decoding validates the complete expanded batch before allocating
// its values. All integers are little-endian; strings preserve raw UTF-16 code units.
import { Decimal, MAX_INT_DIGITS, MAX_SCALE } from "../decimal.ts";
import type { JsonNode } from "../json.ts";
import type { Value } from "../value.ts";
import { TransportError } from "./errors.ts";
import { measureValues, WireBudget } from "./wire.ts";

export type DecodedPackedRows = { rows: Value[][]; rowBytes: number[]; bytes: number };
export type PackedRows = { version: 1; length: number; data: ArrayBuffer };
const MAX_DEPTH = 256;
const HEADER_BYTES = 4;
const ENVELOPE_BYTES = 128;
const arrayBufferByteLength = Object.getOwnPropertyDescriptor(ArrayBuffer.prototype, "byteLength")!
  .get!;
const TAG = {
  null: 0,
  int: 1,
  bool: 2,
  date: 3,
  timestamp: 4,
  timestamptz: 5,
  interval: 6,
  text: 7,
  json: 8,
  jsonpath: 9,
  decimal: 10,
  f32: 11,
  f64: 12,
  bytea: 13,
  uuid: 14,
  composite: 15,
  array: 16,
  range: 17,
  jsonb: 18,
} as const;
const JSON_TAG = { null: 0, bool: 1, number: 2, string: 3, array: 4, object: 5 } as const;

function invalid(): never {
  throw new TransportError("PROTOCOL", "invalid packed worker result batch");
}
function tooLarge(): never {
  throw new TransportError(
    "RESULT_TOO_LARGE",
    "one result row exceeds the worker batch byte budget",
    "unknown",
  );
}

export class PackedRowsWriter {
  private buffer = new ArrayBuffer(0);
  private view = new DataView(this.buffer);
  private bytes = new Uint8Array(this.buffer);
  private offset = HEADER_BYTES;
  private rows = 0;
  private used = ENVELOPE_BYTES;
  private sealed = false;
  private readonly maxBytes: number;

  constructor(maxBatchBytes: number) {
    const budget = new WireBudget(maxBatchBytes);
    if (budget.remaining < ENVELOPE_BYTES) tooLarge();
    this.maxBytes = maxBatchBytes;
  }

  append(row: Value[]): boolean {
    if (this.sealed) throw new TransportError("CLOSED", "packed result writer is finished");
    let size: number;
    try {
      size = measureValues(row, new WireBudget(this.maxBytes - ENVELOPE_BYTES));
    } catch (error) {
      if (error instanceof TransportError && error.code === "RESULT_TOO_LARGE") tooLarge();
      throw error;
    }
    if (size > this.maxBytes - this.used) return false;
    // Every binary field is no larger than its expanded wire charge. This upper bound permits
    // one growth before copying the row; no source buffer is copied before admission succeeds.
    this.grow(this.offset + size);
    const start = this.offset;
    try {
      this.u32(row.length);
      for (const value of row) this.datum(value);
    } catch (error) {
      this.offset = start;
      throw error;
    }
    this.used += size;
    this.rows++;
    return true;
  }

  finish(): PackedRows {
    if (this.sealed) throw new TransportError("CLOSED", "packed result writer is finished");
    this.grow(HEADER_BYTES);
    this.view.setUint32(0, this.rows, true);
    this.sealed = true;
    // Trimming creates a dedicated exact-length buffer, so transferring it exposes no spare
    // capacity or previously rejected row bytes. All source/engine buffers remain independent.
    const data =
      this.buffer.byteLength === this.offset ? this.buffer : this.buffer.slice(0, this.offset);
    return { version: 1, length: this.rows, data };
  }

  private grow(needed: number): void {
    if (!Number.isSafeInteger(needed) || needed > 0xffffffff) tooLarge();
    if (needed <= this.buffer.byteLength) return;
    const capacity = Math.min(
      this.maxBytes,
      Math.max(needed, Math.max(256, this.buffer.byteLength * 2)),
    );
    const next = new ArrayBuffer(capacity);
    new Uint8Array(next).set(this.bytes.subarray(0, Math.min(this.offset, this.bytes.length)));
    this.buffer = next;
    this.view = new DataView(next);
    this.bytes = new Uint8Array(next);
  }
  private u8(value: number): void {
    this.view.setUint8(this.offset++, value);
  }
  private u16(value: number): void {
    this.view.setUint16(this.offset, value, true);
    this.offset += 2;
  }
  private u32(value: number): void {
    if (value > 0xffffffff) tooLarge();
    this.view.setUint32(this.offset, value, true);
    this.offset += 4;
  }
  private i64(value: bigint): void {
    this.view.setBigInt64(this.offset, value, true);
    this.offset += 8;
  }
  private f64(value: number): void {
    this.view.setFloat64(this.offset, value, true);
    this.offset += 8;
  }
  private text(value: string): void {
    this.u32(value.length);
    for (let i = 0; i < value.length; i++) this.u16(value.charCodeAt(i));
  }
  private decimal(value: Decimal): void {
    this.u8(Number(value.neg));
    this.u32(value.scale);
    this.u32(value.limbs.length);
    for (const limb of value.limbs) this.u16(limb);
  }
  private json(value: JsonNode): void {
    this.u8(JSON_TAG[value.kind]);
    switch (value.kind) {
      case "null":
        return;
      case "bool":
        this.u8(Number(value.value));
        return;
      case "number":
        this.decimal(value.dec);
        return;
      case "string":
        this.text(value.value);
        return;
      case "array":
        this.u32(value.elements.length);
        for (const element of value.elements) this.json(element);
        return;
      case "object":
        this.u32(value.members.length);
        for (const member of value.members) {
          this.text(member.key);
          this.json(member.value);
        }
        return;
    }
  }
  private datum(value: Value): void {
    if (value.kind === "unfetched") invalid();
    this.u8(TAG[value.kind]);
    switch (value.kind) {
      case "null":
        return;
      case "int":
        this.i64(value.int);
        return;
      case "bool":
        this.u8(Number(value.value));
        return;
      case "date":
        this.i64(value.days);
        return;
      case "timestamp":
      case "timestamptz":
        this.i64(value.micros);
        return;
      case "interval":
        this.f64(value.iv.months);
        this.f64(value.iv.days);
        this.i64(value.iv.micros);
        return;
      case "text":
      case "json":
      case "jsonpath":
        this.text(value.text);
        return;
      case "decimal":
        this.decimal(value.dec);
        return;
      case "f32":
        this.view.setFloat32(this.offset, value.value, true);
        this.offset += 4;
        return;
      case "f64":
        this.f64(value.value);
        return;
      case "bytea":
        this.u32(value.bytes.length);
        this.bytes.set(value.bytes, this.offset);
        this.offset += value.bytes.length;
        return;
      case "uuid":
        this.bytes.set(value.bytes, this.offset);
        this.offset += 16;
        return;
      case "composite":
        this.u32(value.fields.length);
        for (const field of value.fields) this.datum(field);
        return;
      case "array":
        this.u8(value.dims.length);
        for (let i = 0; i < value.dims.length; i++) {
          this.f64(value.dims[i]!);
          this.f64(value.lbounds[i]!);
        }
        for (const element of value.elements) this.datum(element);
        return;
      case "range":
        this.u8(
          Number(value.empty) |
            (Number(value.lowerInc) << 1) |
            (Number(value.upperInc) << 2) |
            (Number(value.lower !== null) << 3) |
            (Number(value.upper !== null) << 4),
        );
        if (value.lower !== null) this.datum(value.lower);
        if (value.upper !== null) this.datum(value.upper);
        return;
      case "jsonb":
        this.json(value.node);
        return;
    }
  }
}

// The preflight reader has a budget and allocates no result containers, strings, bytes or decimals.
// The construction reader traverses the already checked immutable ArrayBuffer synchronously.
// Both modes use this one layout decoder so validation and reconstruction cannot disagree on offsets.
class PackedReader {
  offset = HEADER_BYTES;
  logicalBytes = 0;
  private readonly view: DataView;
  private readonly build: boolean;
  private readonly data: ArrayBuffer;
  private readonly budget?: WireBudget;
  constructor(data: ArrayBuffer, budget?: WireBudget) {
    this.data = data;
    this.budget = budget;
    this.view = new DataView(data);
    this.build = budget === undefined;
  }
  private take(bytes: number): number {
    const at = this.offset;
    if (
      !this.build &&
      (!Number.isSafeInteger(bytes) || bytes < 0 || bytes > this.data.byteLength - at)
    )
      invalid();
    this.offset += bytes;
    return at;
  }
  private reserve(bytes: number): void {
    this.budget?.reserve(bytes);
    this.logicalBytes += bytes;
  }
  private node(depth: number): void {
    if (!this.build && depth > MAX_DEPTH) invalid();
    this.reserve(32);
  }
  private array(length: number): void {
    this.reserve(32 + length * 8);
  }
  private u8(): number {
    return this.view.getUint8(this.take(1));
  }
  private u16(): number {
    return this.view.getUint16(this.take(2), true);
  }
  private u32(): number {
    return this.view.getUint32(this.take(4), true);
  }
  private i64(): bigint {
    const at = this.take(8);
    return this.build ? this.view.getBigInt64(at, true) : 0n;
  }
  private f64(): number {
    return this.view.getFloat64(this.take(8), true);
  }
  private bool(): boolean {
    const v = this.u8();
    if (!this.build && v > 1) invalid();
    return v === 1;
  }
  private integer(): number {
    const v = this.f64();
    if (!this.build && !Number.isSafeInteger(v)) invalid();
    return v;
  }
  private text(): string {
    const length = this.u32();
    this.reserve(length * 2);
    const start = this.take(length * 2);
    if (!this.build || length === 0) return "";
    let text = "";
    for (let i = 0; i < length; i += 8192) {
      const units = new Array<number>(Math.min(8192, length - i));
      for (let j = 0; j < units.length; j++)
        units[j] = this.view.getUint16(start + (i + j) * 2, true);
      text += String.fromCharCode(...units);
    }
    return text;
  }
  private decimal(): Decimal | undefined {
    const neg = this.bool();
    const scale = this.u32();
    const count = this.u32();
    this.reserve(72 + count * 8);
    if (!this.build && (scale > MAX_SCALE || count > Math.ceil((MAX_INT_DIGITS + MAX_SCALE) / 4)))
      invalid();
    const limbs = this.build ? new Array<number>(count) : undefined;
    let last = 0;
    for (let i = 0; i < count; i++) {
      last = this.u16();
      if (!this.build && last >= 10000) invalid();
      if (limbs !== undefined) limbs[i] = last;
    }
    if (!this.build && (count === 0 ? neg : last === 0)) invalid();
    return limbs === undefined ? undefined : Decimal.fromParts(neg, scale, limbs);
  }
  private json(depth: number): JsonNode | undefined {
    this.node(depth);
    switch (this.u8()) {
      case JSON_TAG.null:
        return this.build ? { kind: "null" } : undefined;
      case JSON_TAG.bool: {
        const value = this.bool();
        return this.build ? { kind: "bool", value } : undefined;
      }
      case JSON_TAG.number: {
        const dec = this.decimal();
        return this.build ? { kind: "number", dec: dec! } : undefined;
      }
      case JSON_TAG.string: {
        const value = this.text();
        return this.build ? { kind: "string", value } : undefined;
      }
      case JSON_TAG.array: {
        const count = this.u32();
        this.array(count);
        const elements = this.build ? new Array<JsonNode>(count) : undefined;
        for (let i = 0; i < count; i++) {
          const value = this.json(depth + 1);
          if (elements !== undefined) elements[i] = value!;
        }
        return elements === undefined ? undefined : { kind: "array", elements };
      }
      case JSON_TAG.object: {
        const count = this.u32();
        this.array(count);
        const members = this.build ? new Array<{ key: string; value: JsonNode }>(count) : undefined;
        for (let i = 0; i < count; i++) {
          this.reserve(32);
          const key = this.text();
          const value = this.json(depth + 1);
          if (members !== undefined) members[i] = { key, value: value! };
        }
        return members === undefined ? undefined : { kind: "object", members };
      }
      default:
        return invalid();
    }
  }
  private datum(depth: number): Value | undefined {
    this.node(depth);
    switch (this.u8()) {
      case TAG.null:
        return this.build ? { kind: "null" } : undefined;
      case TAG.int: {
        this.reserve(8);
        const int = this.i64();
        return this.build ? { kind: "int", int } : undefined;
      }
      case TAG.bool: {
        const value = this.bool();
        return this.build ? { kind: "bool", value } : undefined;
      }
      case TAG.date: {
        this.reserve(8);
        const days = this.i64();
        return this.build ? { kind: "date", days } : undefined;
      }
      case TAG.timestamp:
      case TAG.timestamptz: {
        const tag = this.view.getUint8(this.offset - 1);
        this.reserve(8);
        const micros = this.i64();
        return this.build
          ? { kind: tag === TAG.timestamp ? "timestamp" : "timestamptz", micros }
          : undefined;
      }
      case TAG.interval: {
        this.reserve(56);
        const months = this.integer();
        const days = this.integer();
        const micros = this.i64();
        return this.build ? { kind: "interval", iv: { months, days, micros } } : undefined;
      }
      case TAG.text:
      case TAG.json:
      case TAG.jsonpath: {
        const tag = this.view.getUint8(this.offset - 1);
        const text = this.text();
        return this.build
          ? { kind: tag === TAG.text ? "text" : tag === TAG.json ? "json" : "jsonpath", text }
          : undefined;
      }
      case TAG.decimal: {
        const dec = this.decimal();
        return this.build ? { kind: "decimal", dec: dec! } : undefined;
      }
      case TAG.f32: {
        this.reserve(8);
        const at = this.take(4);
        return this.build ? { kind: "f32", value: this.view.getFloat32(at, true) } : undefined;
      }
      case TAG.f64: {
        this.reserve(8);
        const at = this.take(8);
        return this.build ? { kind: "f64", value: this.view.getFloat64(at, true) } : undefined;
      }
      case TAG.bytea:
      case TAG.uuid: {
        const tag = this.view.getUint8(this.offset - 1);
        const length = tag === TAG.bytea ? this.u32() : 16;
        this.reserve(32 + length);
        const at = this.take(length);
        return this.build
          ? {
              kind: tag === TAG.bytea ? "bytea" : "uuid",
              bytes: new Uint8Array(this.data, at, length).slice(),
            }
          : undefined;
      }
      case TAG.composite: {
        const count = this.u32();
        this.array(count);
        const fields = this.build ? new Array<Value>(count) : undefined;
        for (let i = 0; i < count; i++) {
          const value = this.datum(depth + 1);
          if (fields !== undefined) fields[i] = value!;
        }
        return fields === undefined ? undefined : { kind: "composite", fields };
      }
      case TAG.array: {
        const dimensions = this.u8();
        if (!this.build && dimensions > 6) invalid();
        this.array(dimensions);
        this.array(dimensions);
        const dims = this.build ? new Array<number>(dimensions) : undefined;
        const lbounds = this.build ? new Array<number>(dimensions) : undefined;
        let count = dimensions === 0 ? 0 : 1;
        for (let i = 0; i < dimensions; i++) {
          const dimension = this.integer();
          const lower = this.integer();
          count *= dimension;
          if (!this.build && (dimension <= 0 || !Number.isSafeInteger(count))) invalid();
          if (dims !== undefined) {
            dims[i] = dimension;
            lbounds![i] = lower;
          }
        }
        this.array(count);
        const elements = this.build ? new Array<Value>(count) : undefined;
        for (let i = 0; i < count; i++) {
          const value = this.datum(depth + 1);
          if (elements !== undefined) elements[i] = value!;
        }
        return elements === undefined
          ? undefined
          : { kind: "array", dims: dims!, lbounds: lbounds!, elements };
      }
      case TAG.range: {
        const flags = this.u8();
        if (!this.build && flags > 31) invalid();
        const lower = flags & 8 ? this.datum(depth + 1) : null;
        const upper = flags & 16 ? this.datum(depth + 1) : null;
        return this.build
          ? {
              kind: "range",
              empty: !!(flags & 1),
              lowerInc: !!(flags & 2),
              upperInc: !!(flags & 4),
              lower: lower!,
              upper: upper!,
            }
          : undefined;
      }
      case TAG.jsonb: {
        const node = this.json(depth + 1);
        return this.build ? { kind: "jsonb", node: node! } : undefined;
      }
      default:
        return invalid();
    }
  }
  row(): Value[] | undefined {
    const count = this.u32();
    this.array(count);
    const values = this.build ? new Array<Value>(count) : undefined;
    for (let i = 0; i < count; i++) {
      this.reserve(32); // The versioned value envelope in the logical wire accounting.
      const value = this.datum(0);
      if (values !== undefined) values[i] = value!;
    }
    return values;
  }
}

export function decodePackedRows(packet: PackedRows, budget = new WireBudget()): DecodedPackedRows {
  if (packet === null || typeof packet !== "object") invalid();
  // Real ports provide data properties; custom transports cannot change the envelope between
  // the validation and construction passes.
  const { version, length, data } = packet;
  if (version !== 1 || !Number.isSafeInteger(length) || length < 0 || length > 0xffffffff)
    invalid();
  // Native brand checking accepts buffers delivered by workers from another realm,
  // while rejecting shared buffers and impostors without copying their contents.
  let bytes: number;
  try {
    bytes = arrayBufferByteLength.call(data);
  } catch {
    invalid();
  }
  if (bytes < HEADER_BYTES) invalid();
  // Physical transport cannot evade the logical reservation by appending unparsed padding.
  if (bytes - HEADER_BYTES > budget.remaining)
    throw new TransportError("RESULT_TOO_LARGE", "packed worker result exceeds its byte limit");
  if (new DataView(data).getUint32(0, true) !== length) invalid();
  const checked = new PackedReader(data, budget);
  for (let i = 0; i < length; i++) checked.row();
  if (checked.offset !== bytes) invalid();
  const reader = new PackedReader(data);
  const rows = new Array<Value[]>(length);
  const rowBytes = new Array<number>(length);
  for (let i = 0; i < length; i++) {
    const before = reader.logicalBytes;
    rows[i] = reader.row()!;
    rowBytes[i] = reader.logicalBytes - before;
  }
  return { rows, rowBytes, bytes: checked.logicalBytes };
}
