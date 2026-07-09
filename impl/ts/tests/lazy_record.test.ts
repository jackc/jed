// L1: the no-construct decode seam (spec/design/lazy-record.md §6/§12). The skip walk advances the
// cursor identically to the eager construct decode, for EVERY value type (scalars incl.
// text/bytea/decimal/json/jsonb, and the array/composite/range containers). inlineBodySpan must land
// cur.pos at exactly the position readInlineBody("construct") reaches and return precisely the body
// bytes — the zero-drift property the lazy-record reshape rests on. A construct decode of those same
// bytes must also still re-encode to the original (the eager path is unchanged).
// Mirrors impl/rust/src/format.rs `inline_body_span_matches_decode` and impl/go's
// TestInlineBodySpanMatchesDecode.

import assert from "node:assert/strict";
import { test } from "node:test";
import type { ColField, ColType } from "../src/catalog.ts";
import { Decimal } from "../src/decimal.ts";
import { encodeValue, inlineBodySpan, readInlineBody } from "../src/format.ts";
import type { JsonNode } from "../src/json.ts";
import type { ScalarType } from "../src/types.ts";
import {
  arrayValue,
  boolValue,
  byteaValue,
  compositeValue,
  dateValue,
  decimalValue,
  emptyArray,
  emptyRangeValue,
  float32Value,
  float64Value,
  intervalValue,
  intValue,
  jsonbValue,
  jsonValue,
  nullValue,
  rangeValue,
  textValue,
  timestamptzValue,
  timestampValue,
  uuidValue,
  type Value,
} from "../src/value.ts";

const sc = (s: ScalarType): ColType => ({ kind: "scalar", scalar: s });
const field = (name: string, type: ColType): ColField => ({
  name,
  type,
  typmod: null,
  varcharLen: null,
  notNull: false,
});

test("inlineBodySpan matches the construct decode advance", () => {
  const i32 = sc("i32");
  const text = sc("text");

  // A jsonb document touching every node kind: object, nested array, number, string, bool, null, and
  // an empty string.
  const doc: JsonNode = {
    kind: "object",
    members: [
      { key: "a", value: { kind: "number", dec: Decimal.fromDigitsScale(false, "1234", 2) } },
      {
        key: "b",
        value: {
          kind: "array",
          elements: [
            { kind: "bool", value: true },
            { kind: "null" },
            { kind: "string", value: "x" },
          ],
        },
      },
      { key: "c", value: { kind: "string", value: "" } },
    ],
  };

  const compTy: ColType = {
    kind: "composite",
    name: "pair",
    fields: [field("a", i32), field("b", text)],
  };
  const rangeTy: ColType = { kind: "range", elem: i32 };
  const arrI32: ColType = { kind: "array", elem: i32 };
  const arrText: ColType = { kind: "array", elem: text };

  const cases: { ty: ColType; v: Value }[] = [
    { ty: sc("i16"), v: intValue(-12345n) },
    { ty: i32, v: intValue(70000n) },
    { ty: sc("i64"), v: intValue(-9223372036854775808n) },
    { ty: text, v: textValue("hello, jed") },
    { ty: text, v: textValue("") }, // empty text
    { ty: sc("boolean"), v: boolValue(true) },
    { ty: sc("boolean"), v: boolValue(false) },
    { ty: sc("decimal"), v: decimalValue(Decimal.fromDigitsScale(true, "9876543210", 4)) },
    { ty: sc("bytea"), v: byteaValue(new Uint8Array([0, 1, 2, 255, 254])) },
    { ty: sc("uuid"), v: uuidValue(new Uint8Array(16).fill(7)) },
    { ty: sc("timestamp"), v: timestampValue(1700000000000000n) },
    { ty: sc("timestamptz"), v: timestamptzValue(-42n) },
    { ty: sc("date"), v: dateValue(-19000n) },
    { ty: sc("interval"), v: intervalValue({ months: 14, days: -3, micros: 123456n }) },
    { ty: sc("f64"), v: float64Value(Math.PI) },
    { ty: sc("f32"), v: float32Value(-2.5) },
    { ty: sc("json"), v: jsonValue('{"k": 1}') },
    { ty: sc("jsonb"), v: jsonbValue(doc) },
    // Array of i32 with a NULL element (exercises the has-nulls bitmap branch).
    { ty: arrI32, v: arrayValue([intValue(1n), nullValue(), intValue(3n)]) },
    // Array of text (variable-length elements recurse through readInlineBody).
    { ty: arrText, v: arrayValue([textValue("a"), textValue("bb")]) },
    // Empty array (ndim 0 short-circuit).
    { ty: arrI32, v: emptyArray() },
    // Composite with a present field and (next) a NULL field.
    { ty: compTy, v: compositeValue([intValue(5n), textValue("hi")]) },
    { ty: compTy, v: compositeValue([nullValue(), textValue("only b")]) },
    // Range: bounded [1,5), the empty range, and unbounded-below (-inf,9).
    { ty: rangeTy, v: rangeValue(intValue(1n), intValue(5n), true, false) },
    { ty: rangeTy, v: emptyRangeValue() },
    { ty: rangeTy, v: rangeValue(null, intValue(9n), false, false) },
  ];

  for (let i = 0; i < cases.length; i++) {
    const { ty, v } = cases[i]!;
    // A bigint-safe label (JSON.stringify throws on bigint values).
    const at = `case ${i} (${ty.kind}/${v.kind})`;
    const enc = encodeValue(ty, v);
    assert.equal(enc[0], 0x00, `present values carry the 0x00 tag: ${at}`);

    // Construct decode: consumes the whole body and re-encodes to the original bytes.
    const cc = { pos: 1 };
    const got = readInlineBody(ty, enc, cc, "construct");
    assert.equal(cc.pos, enc.length, `construct decode consumes the whole body: ${at}`);
    assert.deepEqual([...encodeValue(ty, got)], [...enc], `construct decode round-trips: ${at}`);

    // Skip walk: lands at the identical cursor and returns exactly the body bytes — no value built.
    const cs = { pos: 1 };
    const span = inlineBodySpan(ty, enc, cs);
    assert.equal(cs.pos, cc.pos, `skip advance equals construct advance: ${at}`);
    assert.deepEqual([...span], [...enc.subarray(1)], `the span is exactly the body bytes: ${at}`);
  }
});
