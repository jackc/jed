// Version 1 typed worker transport. No engine object, pager reference, or prototype crosses
// the port. Preflight the entire payload before constructing its independent wire copy.
import { Decimal, MAX_INT_DIGITS, MAX_SCALE } from "../decimal.ts";
import type { Value } from "../value.ts";
import { TransportError } from "./errors.ts";

export const WIRE_VERSION = 1;
export const DEFAULT_WIRE_BYTES = 8 * 1024 * 1024;
const MAX_DEPTH = 256;
const I64_MIN = -(1n << 63n);
const I64_MAX = (1n << 63n) - 1n;

type WireDecimal = { neg: boolean; scale: number; limbs: number[] };
type WireJson =
  | { kind: "null" }
  | { kind: "bool"; value: boolean }
  | { kind: "number"; dec: WireDecimal }
  | { kind: "string"; value: string }
  | { kind: "array"; elements: WireJson[] }
  | { kind: "object"; members: { key: string; value: WireJson }[] };

type WireDatum =
  | Exclude<
      Value,
      { kind: "unfetched" | "decimal" | "jsonb" | "array" | "composite" | "range" | "f32" | "f64" }
    >
  | { kind: "f32" | "f64"; bits: bigint }
  | { kind: "decimal"; dec: WireDecimal }
  | { kind: "jsonb"; node: WireJson }
  | { kind: "array"; dims: number[]; lbounds: number[]; elements: WireDatum[] }
  | { kind: "composite"; fields: WireDatum[] }
  | {
      kind: "range";
      empty: boolean;
      lower: WireDatum | null;
      upper: WireDatum | null;
      lowerInc: boolean;
      upperInc: boolean;
    };

export type WireValue = { v: typeof WIRE_VERSION; value: WireDatum };

// A conservative transport accounting unit: 32 bytes per record, 8 per array slot/number,
// two per UTF-16 code unit, and the full length of dedicated byte buffers. These host bytes
// bound copying and traversal; they are neither SQL cost nor a JS heap-size guarantee.
export class WireBudget {
  readonly maxBytes: number;
  used = 0;

  constructor(maxBytes = DEFAULT_WIRE_BYTES) {
    if (!Number.isSafeInteger(maxBytes) || maxBytes < 0) {
      throw new TransportError("PROTOCOL", "wire byte limit must be a non-negative safe integer");
    }
    this.maxBytes = maxBytes;
  }

  get remaining(): number {
    return this.maxBytes - this.used;
  }

  reserve(bytes: number): void {
    if (!Number.isSafeInteger(bytes) || bytes < 0 || bytes > this.remaining) {
      throw new TransportError("RESULT_TOO_LARGE", "typed worker payload exceeds its byte limit");
    }
    this.used += bytes;
  }
}

function invalid(): never {
  throw new TransportError("PROTOCOL", "invalid typed worker value");
}
function object(value: unknown): void {
  if (value === null || typeof value !== "object") invalid();
}
function bool(value: unknown): void {
  if (typeof value !== "boolean") invalid();
}
function integer(value: unknown): void {
  if (!Number.isSafeInteger(value)) invalid();
}
function int64(value: unknown): void {
  if (typeof value !== "bigint" || value < I64_MIN || value > I64_MAX) invalid();
}
function string(value: unknown, budget: WireBudget): void {
  if (typeof value !== "string") invalid();
  budget.reserve(value.length * 2);
}
function array(value: unknown, budget: WireBudget): void {
  if (!Array.isArray(value)) invalid();
  budget.reserve(32 + value.length * 8);
}
function node(value: unknown, budget: WireBudget, depth: number): void {
  if (depth > MAX_DEPTH)
    throw new TransportError("PROTOCOL", "typed worker value nesting exceeds 256");
  object(value);
  budget.reserve(32);
}

function measureDecimal(value: WireDecimal, budget: WireBudget): void {
  object(value);
  bool(value.neg);
  integer(value.scale);
  if (value.scale < 0 || value.scale > MAX_SCALE) invalid();
  array(value.limbs, budget);
  if (value.limbs.length > Math.ceil((MAX_INT_DIGITS + MAX_SCALE) / 4)) invalid();
  for (const limb of value.limbs) {
    if (!Number.isInteger(limb) || limb < 0 || limb >= 10_000) invalid();
  }
  if (value.limbs.length === 0 ? value.neg : value.limbs[value.limbs.length - 1] === 0) invalid();
  budget.reserve(40);
}

function measureJson(value: WireJson, budget: WireBudget, depth: number): void {
  node(value, budget, depth);
  switch (value.kind) {
    case "null":
      return;
    case "bool":
      bool(value.value);
      return;
    case "number":
      measureDecimal(value.dec, budget);
      return;
    case "string":
      string(value.value, budget);
      return;
    case "array":
      array(value.elements, budget);
      for (const element of value.elements) measureJson(element, budget, depth + 1);
      return;
    case "object":
      array(value.members, budget);
      for (const member of value.members) {
        object(member);
        budget.reserve(32);
        string(member.key, budget);
        measureJson(member.value, budget, depth + 1);
      }
      return;
    default:
      invalid();
  }
}

function measureDatum(value: WireDatum, budget: WireBudget, depth: number, decode: boolean): void {
  node(value, budget, depth);
  switch (value.kind) {
    case "null":
      return;
    case "int":
      int64(value.int);
      budget.reserve(8);
      return;
    case "bool":
      bool(value.value);
      return;
    case "date":
      int64(value.days);
      budget.reserve(8);
      return;
    case "timestamp":
    case "timestamptz":
      int64(value.micros);
      budget.reserve(8);
      return;
    case "interval":
      object(value.iv);
      integer(value.iv.months);
      integer(value.iv.days);
      int64(value.iv.micros);
      budget.reserve(56);
      return;
    case "text":
    case "json":
    case "jsonpath":
      string(value.text, budget);
      return;
    case "decimal":
      measureDecimal(value.dec, budget);
      return;
    case "f32":
    case "f64": {
      if (decode) {
        const max = value.kind === "f32" ? 0xffffffffn : 0xffffffffffffffffn;
        if (typeof value.bits !== "bigint" || value.bits < 0n || value.bits > max) invalid();
      } else {
        const number = (value as unknown as { value: number }).value;
        if (typeof number !== "number") invalid();
        if (value.kind === "f32" && !Object.is(Math.fround(number), number)) invalid();
      }
      budget.reserve(8);
      return;
    }
    case "bytea":
    case "uuid":
      if (!(value.bytes instanceof Uint8Array)) invalid();
      if (value.kind === "uuid" && value.bytes.length !== 16) invalid();
      budget.reserve(32 + value.bytes.byteLength);
      return;
    case "composite":
      array(value.fields, budget);
      for (const field of value.fields) measureDatum(field, budget, depth + 1, decode);
      return;
    case "array": {
      array(value.dims, budget);
      array(value.lbounds, budget);
      array(value.elements, budget);
      if (value.dims.length !== value.lbounds.length || value.dims.length > 6) invalid();
      let count = value.dims.length === 0 ? 0 : 1;
      for (let i = 0; i < value.dims.length; i++) {
        const dimension = value.dims[i];
        integer(dimension);
        integer(value.lbounds[i]);
        if (dimension <= 0) invalid();
        count *= dimension;
        if (!Number.isSafeInteger(count)) invalid();
      }
      if (count !== value.elements.length) invalid();
      for (const element of value.elements) measureDatum(element, budget, depth + 1, decode);
      return;
    }
    case "range":
      bool(value.empty);
      bool(value.lowerInc);
      bool(value.upperInc);
      if (value.lower !== null) measureDatum(value.lower, budget, depth + 1, decode);
      if (value.upper !== null) measureDatum(value.upper, budget, depth + 1, decode);
      return;
    case "jsonb":
      measureJson(value.node, budget, depth + 1);
      return;
    default:
      invalid(); // Includes poisoned lazy engine values; never clone their pager handles.
  }
}

function copyDecimal(value: WireDecimal, decode: boolean): WireDecimal {
  const limbs = value.limbs.slice();
  return decode
    ? Decimal.fromParts(value.neg, value.scale, limbs)
    : { neg: value.neg, scale: value.scale, limbs };
}
function copyJson(value: WireJson, decode: boolean): WireJson {
  switch (value.kind) {
    case "null":
      return { kind: "null" };
    case "bool":
      return { kind: "bool", value: value.value };
    case "number":
      return { kind: "number", dec: copyDecimal(value.dec, decode) };
    case "string":
      return { kind: "string", value: value.value };
    case "array":
      return { kind: "array", elements: value.elements.map((v) => copyJson(v, decode)) };
    case "object":
      return {
        kind: "object",
        members: value.members.map((m) => ({ key: m.key, value: copyJson(m.value, decode) })),
      };
  }
}
function copyDatum(value: WireDatum, decode: boolean): WireDatum {
  switch (value.kind) {
    case "null":
      return { kind: "null" };
    case "int":
      return { kind: "int", int: value.int };
    case "bool":
      return { kind: "bool", value: value.value };
    case "date":
      return { kind: "date", days: value.days };
    case "timestamp":
    case "timestamptz":
      return { kind: value.kind, micros: value.micros };
    case "interval":
      return {
        kind: "interval",
        iv: { months: value.iv.months, days: value.iv.days, micros: value.iv.micros },
      };
    case "text":
    case "json":
    case "jsonpath":
      return { kind: value.kind, text: value.text };
    case "decimal":
      return { kind: "decimal", dec: copyDecimal(value.dec, decode) };
    case "f32":
    case "f64": {
      // Structured clone canonicalizes NaN payloads. Transport their IEEE bits explicitly,
      // preserving signed zero and the stored float bit pattern as well as ordinary values.
      const bits = new DataView(new ArrayBuffer(8));
      if (decode) {
        if (value.kind === "f32") bits.setUint32(0, Number(value.bits));
        else bits.setBigUint64(0, value.bits);
        const number = value.kind === "f32" ? bits.getFloat32(0) : bits.getFloat64(0);
        return { kind: value.kind, value: number } as unknown as WireDatum;
      }
      const number = (value as unknown as { value: number }).value;
      if (value.kind === "f32") bits.setFloat32(0, number);
      else bits.setFloat64(0, number);
      return {
        kind: value.kind,
        bits: value.kind === "f32" ? BigInt(bits.getUint32(0)) : bits.getBigUint64(0),
      };
    }
    case "bytea":
    case "uuid":
      return { kind: value.kind, bytes: value.bytes.slice() };
    case "composite":
      return { kind: "composite", fields: value.fields.map((v) => copyDatum(v, decode)) };
    case "array":
      return {
        kind: "array",
        dims: value.dims.slice(),
        lbounds: value.lbounds.slice(),
        elements: value.elements.map((v) => copyDatum(v, decode)),
      };
    case "range":
      return {
        kind: "range",
        empty: value.empty,
        lowerInc: value.lowerInc,
        upperInc: value.upperInc,
        lower: value.lower === null ? null : copyDatum(value.lower, decode),
        upper: value.upper === null ? null : copyDatum(value.upper, decode),
      };
    case "jsonb":
      return { kind: "jsonb", node: copyJson(value.node, decode) };
  }
}

export function measureValues(values: Value[], budget = new WireBudget()): number {
  const before = budget.used;
  array(values, budget);
  for (const value of values) {
    budget.reserve(32); // Version envelope.
    measureDatum(value as WireDatum, budget, 0, false);
  }
  return budget.used - before;
}
export function measureWireValues(values: WireValue[], budget = new WireBudget()): number {
  const before = budget.used;
  array(values, budget);
  for (const value of values) {
    node(value, budget, 0);
    if (value.v !== WIRE_VERSION) invalid();
    measureDatum(value.value, budget, 0, true);
  }
  return budget.used - before;
}
export function encodeValue(value: Value, budget = new WireBudget()): WireValue {
  budget.reserve(32);
  measureDatum(value as WireDatum, budget, 0, false);
  return { v: WIRE_VERSION, value: copyDatum(value as WireDatum, false) };
}
export function decodeValue(value: WireValue, budget = new WireBudget()): Value {
  node(value, budget, 0);
  if (value.v !== WIRE_VERSION) invalid();
  measureDatum(value.value, budget, 0, true);
  return copyDatum(value.value, true) as Value;
}
export function encodeValues(values: Value[], budget = new WireBudget()): WireValue[] {
  measureValues(values, budget);
  return values.map((value) => ({ v: WIRE_VERSION, value: copyDatum(value as WireDatum, false) }));
}
export function decodeValues(values: WireValue[], budget = new WireBudget()): Value[] {
  measureWireValues(values, budget);
  return values.map((value) => copyDatum(value.value, true) as Value);
}
