import assert from "node:assert/strict";
import { test } from "node:test";
import { Decimal } from "../src/decimal.ts";
import type { Value } from "../src/value.ts";
import { PackedRowsWriter, decodePackedRows, type PackedRows } from "../src/async/packed.ts";
import { TransportError } from "../src/async/errors.ts";
import { measureValues, WireBudget } from "../src/async/wire.ts";
import { decimal, values } from "./async_wire_values.ts";

function pack(rows: Value[][], limit = 1024 * 1024): PackedRows {
  const writer = new PackedRowsWriter(limit);
  for (const row of rows) assert.equal(writer.append(row), true);
  return writer.finish();
}
function protocol(error: unknown): boolean {
  return error instanceof TransportError && error.code === "PROTOCOL";
}
function oversized(error: unknown): boolean {
  return error instanceof TransportError && error.code === "RESULT_TOO_LARGE";
}
function raw(bytes: number[], rows = 1): PackedRows {
  const data = Uint8Array.from(bytes).buffer;
  return { version: 1, length: rows, data };
}

// Row and field counts are u32 LE. These fixtures exercise the binary format independently of
// the encoder, including cases a valid Value cannot represent.
const ROW_ONE = [1, 0, 0, 0, 1, 0, 0, 0];

test("packed batches round-trip every value kind and preserve wire accounting", () => {
  const rows = [values, [], values.slice(0, 3)];
  const packet = pack(rows);
  const budget = new WireBudget(128 + rows.reduce((sum, row) => sum + measureValues(row), 0));
  budget.reserve(128);
  const decoded = decodePackedRows(structuredClone(packet), budget);
  assert.deepEqual(decoded.rows, rows);
  assert.deepEqual(
    decoded.rowBytes,
    rows.map((row) => measureValues(row)),
  );
  assert.equal(
    decoded.bytes,
    decoded.rowBytes.reduce((sum, n) => sum + n, 0),
  );
  assert.equal(budget.remaining, 0);
  const exact = decoded.rows[0][9];
  assert.equal(exact.kind, "decimal");
  if (exact.kind !== "decimal") throw new Error("decimal expected");
  assert.ok(exact.dec instanceof Decimal);
  assert.equal(exact.dec.render(), decimal.render());
  const string: Value = { kind: "text", text: "\ud800𝄞\u0000\udfff".repeat(10_000) };
  assert.deepEqual(decodePackedRows(pack([[string]])).rows, [[string]]);
});

test("packed row admission has exactly the old logical byte boundaries and no source copies on refusal", () => {
  const bytes = Uint8Array.of(7, 8, 9);
  const row: Value[] = [{ kind: "bytea", bytes }];
  const size = measureValues(row);
  const writer = new PackedRowsWriter(128 + size);
  assert.equal(writer.append(row), true);
  // A refused row is only measured, not copied. An invalid first element on an oversized row
  // must still fail preflight rather than allocating a buffer for its already visited prefix.
  assert.equal(writer.append(row), false);
  bytes.fill(0);
  assert.deepEqual(decodePackedRows(writer.finish()).rows, [
    [{ kind: "bytea", bytes: Uint8Array.of(7, 8, 9) }],
  ]);
  const small = new PackedRowsWriter(128 + size - 1);
  assert.throws(
    () => small.append(row),
    (error) => oversized(error) && (error as TransportError).outcome === "unknown",
  );
  assert.deepEqual(decodePackedRows(small.finish()).rows, []);
  assert.throws(
    () => new PackedRowsWriter(127),
    (error) => oversized(error) && (error as TransportError).outcome === "unknown",
  );
});

test("writer snapshots rows immediately and decoded bytes do not alias the transferred packet", () => {
  const value: Value = {
    kind: "array",
    dims: [1],
    lbounds: [-3],
    elements: [{ kind: "bytea", bytes: Uint8Array.of(1, 2) }],
  };
  const writer = new PackedRowsWriter(4096);
  writer.append([value, { kind: "decimal", dec: Decimal.fromBigInt(42n) }]);
  value.dims[0] = 99;
  if (value.elements[0].kind === "bytea") value.elements[0].bytes.fill(9);
  const packet = writer.finish();
  assert.throws(() => writer.append([]), TransportError);
  assert.throws(() => writer.finish(), TransportError);
  const transferred = structuredClone(packet, { transfer: [packet.data] });
  assert.equal(packet.data.byteLength, 0);
  const decoded = decodePackedRows(transferred).rows[0];
  new Uint8Array(transferred.data).fill(0);
  assert.deepEqual(decoded, [
    {
      kind: "array",
      dims: [1],
      lbounds: [-3],
      elements: [{ kind: "bytea", bytes: Uint8Array.of(1, 2) }],
    },
    { kind: "decimal", dec: Decimal.fromBigInt(42n) },
  ]);
});

test("packed float payloads preserve both widths, NaN payloads, infinities and signed zero", () => {
  const scratch = new DataView(new ArrayBuffer(8));
  for (const bits of [
    0x7ff8000000000001n,
    0xfff8123400000000n,
    0x8000000000000000n,
    0x7ff0000000000000n,
    1n,
  ]) {
    scratch.setBigUint64(0, bits, true);
    const packet = pack([[{ kind: "f64", value: scratch.getFloat64(0, true) }]]);
    const value = decodePackedRows(packet).rows[0][0];
    assert.equal(value.kind, "f64");
    if (value.kind !== "f64") throw new Error("f64 expected");
    scratch.setFloat64(0, value.value, true);
    assert.equal(scratch.getBigUint64(0, true), bits);
  }
  for (const bits of [0x7fc12345, 0xffc12345, 0x80000000, 0x7f800000, 1]) {
    scratch.setUint32(0, bits, true);
    const packet = pack([[{ kind: "f32", value: scratch.getFloat32(0, true) }]]);
    const value = decodePackedRows(packet).rows[0][0];
    assert.equal(value.kind, "f32");
    if (value.kind !== "f32") throw new Error("f32 expected");
    scratch.setFloat32(0, value.value, true);
    assert.equal(scratch.getUint32(0, true), bits);
  }
});

test("packed preflight rejects all truncations, inconsistent row counts, unknown tags and padding", () => {
  const packet = pack([values]);
  for (let n = 0; n < packet.data.byteLength; n++) {
    assert.throws(() => decodePackedRows({ ...packet, data: packet.data.slice(0, n) }), protocol);
  }
  assert.throws(() => decodePackedRows({ ...packet, length: 2 }), protocol);
  assert.throws(
    () => decodePackedRows({ ...packet, version: 2 } as unknown as PackedRows),
    protocol,
  );
  assert.throws(() => decodePackedRows(raw([...ROW_ONE, 255])), protocol);
  assert.throws(() => decodePackedRows(raw([...ROW_ONE, 0, 0])), protocol);
  assert.throws(() => decodePackedRows(raw([...ROW_ONE, 2, 2])), protocol); // Invalid boolean.
  assert.throws(() => decodePackedRows(raw([...ROW_ONE, 17, 32])), protocol); // Invalid range flags.
  const shared = new SharedArrayBuffer(4);
  assert.throws(
    () => decodePackedRows({ version: 1, length: 0, data: shared } as unknown as PackedRows),
    protocol,
  );
});

test("tiny packed inputs cannot expand unchecked counts into large arrays, strings or byte buffers", () => {
  const huge = [255, 255, 255, 127];
  for (const bytes of [
    [1, 0, 0, 0, ...huge], // Row columns.
    [...ROW_ONE, 15, ...huge], // Composite fields.
    [...ROW_ONE, 7, ...huge], // UTF-16 length.
    [...ROW_ONE, 13, ...huge], // Bytea length.
    [...ROW_ONE, 18, 4, ...huge], // JSON array.
    [...ROW_ONE, 18, 5, ...huge], // JSON object.
  ])
    assert.throws(() => decodePackedRows(raw(bytes), new WireBudget(1024)), oversized);
  const packet = pack([values]);
  assert.throws(
    () => decodePackedRows(packet, new WireBudget(measureValues(values) - 1)),
    oversized,
  );
});

test("packed decimal and array shape validation runs before any decoded decimal construction", () => {
  for (const decimal of [
    [1, 0, 0, 0, 0, 0, 0, 0, 0], // Negative zero.
    [0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0], // High zero limb.
    [0, 0, 0, 0, 0, 1, 0, 0, 0, 0x10, 0x27], // Limb 10000.
    [0, 0xff, 0xff, 0, 0, 0, 0, 0, 0], // Excessive scale.
  ])
    assert.throws(() => decodePackedRows(raw([...ROW_ONE, 10, ...decimal])), protocol);
  assert.throws(() => decodePackedRows(raw([...ROW_ONE, 16, 7])), protocol); // More than six dimensions.
  const array = new Uint8Array(8 + 2 + 16);
  array.set([...ROW_ONE, 16, 1]);
  new DataView(array.buffer).setFloat64(10, 0, true); // Nonpositive dimension.
  new DataView(array.buffer).setFloat64(18, 1, true);
  assert.throws(() => decodePackedRows({ version: 1, length: 1, data: array.buffer }), protocol);
  const valid = pack([[{ kind: "decimal", dec: decimal }], [{ kind: "null" }]]);
  new Uint8Array(valid.data)[valid.data.byteLength - 1] = 255;
  const original = Decimal.fromParts;
  let allocations = 0;
  Decimal.fromParts = (...args) => {
    allocations++;
    return original(...args);
  };
  try {
    assert.throws(() => decodePackedRows(valid), protocol);
  } finally {
    Decimal.fromParts = original;
  }
  assert.equal(allocations, 0);
});

test("packed nesting uses the same 256-node boundary and writer rejects cycles and lazy values", () => {
  let value: Value = { kind: "null" };
  for (let i = 0; i < 256; i++) value = { kind: "composite", fields: [value] };
  assert.deepEqual(decodePackedRows(pack([[value]])).rows, [[value]]);
  const tooDeep: Value = { kind: "composite", fields: [value] };
  assert.throws(() => pack([[tooDeep]]), protocol);
  const cycle: Value = { kind: "composite", fields: [] };
  cycle.fields.push(cycle);
  assert.throws(() => pack([[cycle]]), protocol);
  assert.throws(() => pack([[{ kind: "unfetched" } as Value]]), protocol);
  // Exercise decoder depth independently, without asking the validating writer to create it.
  const body = [...ROW_ONE];
  for (let i = 0; i < 257; i++) body.push(15, 1, 0, 0, 0);
  body.push(0);
  assert.throws(() => decodePackedRows(raw(body)), protocol);
});

test("packed decoding captures custom transport envelope accessors once", () => {
  const source = pack([[{ kind: "int", int: 42n }]]);
  let lengths = 0;
  let buffers = 0;
  const packet: PackedRows = {
    version: 1,
    get length() {
      return ++lengths === 1 ? 1 : 0x7fffffff;
    },
    get data() {
      buffers++;
      return buffers === 1 ? source.data : new ArrayBuffer(0);
    },
  };
  assert.deepEqual(decodePackedRows(packet).rows, [[{ kind: "int", int: 42n }]]);
  assert.equal(lengths, 1);
  assert.equal(buffers, 1);
});
