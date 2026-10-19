import assert from "node:assert/strict";
import { test } from "node:test";
import { EngineError, engineError } from "../src/errors.ts";
import { Decimal } from "../src/decimal.ts";
import type { Value } from "../src/value.ts";
import { jsonbOut } from "../src/json.ts";
import { encodeError, decodeError, TransportError } from "../src/async/errors.ts";
import {
  WireBudget,
  encodeValue,
  decodeValue,
  encodeValues,
  decodeValues,
  measureValues,
  measureWireValues,
  type WireValue,
} from "../src/async/wire.ts";

import { decimal, values } from "./async_wire_values.ts";

test("typed wire round-trips every public value kind through structured clone", () => {
  const wire = structuredClone(encodeValues(values));
  const decoded = decodeValues(wire);
  assert.deepEqual(decoded, values);
  const dec = decoded[9];
  assert.equal(dec.kind, "decimal");
  if (dec.kind !== "decimal") throw new Error("decimal expected");
  assert.ok(dec.dec instanceof Decimal);
  assert.equal(dec.dec.render(), decimal.render());
  const json = decoded.at(-1)!;
  assert.equal(json.kind, "jsonb");
  if (json.kind !== "jsonb") throw new Error("jsonb expected");
  assert.ok(jsonbOut(json.node).includes(decimal.render()));
  assert.ok(Object.is((decoded[12] as { value: number }).value, -0));
  assert.equal(measureValues(values), measureWireValues(wire));
});

test("wire owns all mutable bytes, decimal limbs, and container metadata", () => {
  const original: Value = {
    kind: "array",
    dims: [1],
    lbounds: [2],
    elements: [{ kind: "bytea", bytes: Uint8Array.of(1, 2, 3) }],
  };
  const wire = encodeValue(original);
  const decoded = decodeValue(wire);
  assert.equal(decoded.kind, "array");
  original.dims[0] = 99;
  if (original.elements[0].kind === "bytea") original.elements[0].bytes[0] = 99;
  if (decoded.kind !== "array" || decoded.elements[0].kind !== "bytea")
    throw new Error("array expected");
  assert.deepEqual(decoded.dims, [1]);
  assert.deepEqual(decoded.elements[0].bytes, Uint8Array.of(1, 2, 3));
  const d = decodeValue(encodeValue({ kind: "decimal", dec: decimal }));
  assert.equal(d.kind, "decimal");
  if (d.kind !== "decimal") throw new Error("decimal expected");
  assert.notEqual(d.dec.limbs, decimal.limbs);
});

test("typed float transport preserves NaN payload bits that structured clone otherwise loses", () => {
  const buffer = new DataView(new ArrayBuffer(8));
  for (const bits of [0x7ff8000000000001n, 0xfff8123400000000n]) {
    buffer.setBigUint64(0, bits);
    const original: Value = { kind: "f64", value: buffer.getFloat64(0) };
    const decoded = decodeValue(structuredClone(encodeValue(original)));
    assert.equal(decoded.kind, "f64");
    if (decoded.kind !== "f64") throw new Error("float expected");
    buffer.setFloat64(0, decoded.value);
    assert.equal(buffer.getBigUint64(0), bits);
  }
});

test("wire preflight bounds allocations and rejects cycles, lazy values, malformed shape and version", () => {
  const bytes = Uint8Array.of(1, 2, 3);
  bytes.slice = () => {
    throw new Error("copy must not occur before admission");
  };
  assert.throws(
    () =>
      encodeValues(
        [
          { kind: "bytea", bytes },
          { kind: "text", text: "x".repeat(1000) },
        ],
        new WireBudget(500),
      ),
    (error) => error instanceof TransportError && error.code === "RESULT_TOO_LARGE",
  );
  const cyclic: Value = { kind: "composite", fields: [] };
  cyclic.fields.push(cyclic);
  assert.throws(() => encodeValue(cyclic), TransportError);
  assert.throws(() => encodeValue({ kind: "unfetched" } as Value), TransportError);
  assert.throws(
    () => encodeValue({ kind: "array", dims: [2], lbounds: [1], elements: [] }),
    TransportError,
  );
  assert.throws(
    () => decodeValue({ v: 2, value: { kind: "null" } } as unknown as WireValue),
    TransportError,
  );
  const budget = new WireBudget(measureValues(values));
  encodeValues(values, budget);
  assert.equal(budget.remaining, 0);
  assert.throws(() => encodeValue({ kind: "null" }, budget), TransportError);
});

test("SQL diagnostics and transport uncertainty survive the worker boundary", () => {
  const original = engineError("unique_violation", "duplicate key")
    .withConstraint("t_pkey")
    .withTable("t")
    .withDatabase("main")
    .withColumn("id")
    .withDataType("i64")
    .withDetail("Key (id)=(1) already exists.")
    .withPosition(13)
    .withHint("choose another key");
  original.located = true;
  const wire = structuredClone(encodeError(original));
  assert.equal("located" in wire, false);
  const decoded = decodeError(wire);
  assert.ok(decoded instanceof EngineError);
  assert.equal(decoded.code(), "23505");
  assert.equal(decoded.message, original.message);
  for (const key of [
    "constraintName",
    "tableName",
    "databaseName",
    "columnName",
    "dataTypeName",
    "detail",
    "position",
    "hint",
  ] as const) {
    assert.equal(decoded[key], original[key]);
  }
  const transport = decodeError(
    encodeError(new TransportError("WORKER_FAILED", "reply lost", "unknown")),
  );
  assert.ok(transport instanceof TransportError);
  assert.equal(transport.code, "WORKER_FAILED");
  assert.equal(transport.outcome, "unknown");
  assert.ok(encodeError(engineError("syntax_error", "x".repeat(100_000))).message.length <= 16_384);
});
