// Cross-check: the TS range key codec (encodeRangeKey, spec/design/encoding.md §2.11) must produce
// the byte-exact, order-preserving vectors the Rust/Go cores and the Ruby reference reproduce
// (CLAUDE.md §8). Range is the first container key — empty/±∞/inclusivity framing around the
// element's own key. The behavioral side (a range PRIMARY KEY/index/UNIQUE/FK works) lives in
// types/range.test; this is the encoding contract.

import { strict as assert } from "node:assert";
import { test } from "node:test";
import { encodeRangeKey } from "../src/range.ts";
import { Decimal } from "../src/decimal.ts";
import { decimalValue, emptyRangeValue, intValue, rangeValue, type Value } from "../src/value.ts";

// the range variant of Value, the shape encodeRangeKey accepts.
type RangeValue = Extract<Value, { kind: "range" }>;

function hex(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

// a canonical i32range from optional finite bounds (discrete [) form: lower inclusive, upper
// exclusive — what the engine stores); null is an infinite bound.
function i32r(lo: number | null, hi: number | null): RangeValue {
  return rangeValue(
    lo === null ? null : intValue(BigInt(lo)),
    hi === null ? null : intValue(BigInt(hi)),
    lo !== null, // lower inclusive when finite
    false, // upper exclusive (canonical [) )
  ) as RangeValue;
}

function encI32(v: RangeValue): Uint8Array {
  return encodeRangeKey("i32", v);
}

test("encodeRangeKey i32range byte-exact", () => {
  assert.equal(hex(encI32(emptyRangeValue() as RangeValue)), "00");
  assert.equal(hex(encI32(i32r(null, 5))), "0100018000000500"); // (,5)
  assert.equal(hex(encI32(i32r(null, null))), "010002"); // (,)
  assert.equal(hex(encI32(i32r(1, 5))), "01018000000100018000000500"); // [1,5) — §2.11 worked example
  assert.equal(hex(encI32(i32r(2, null))), "0101800000020002"); // [2,)
});

test("encodeRangeKey is order-preserving", () => {
  // a strictly ascending sequence under rangeTotalCmp
  const ranges: RangeValue[] = [
    emptyRangeValue() as RangeValue,
    i32r(null, 5), // (,5)
    i32r(null, null), // (,)
    i32r(1, 5), // [1,5)
    i32r(1, 10),
    i32r(2, 4),
    i32r(2, null), // [2,)
  ];
  for (let i = 1; i < ranges.length; i++) {
    const a = hex(encI32(ranges[i - 1]!));
    const b = hex(encI32(ranges[i]!));
    assert.ok(a < b, `keys must be strictly ascending: ${a} !< ${b}`);
  }
});

test("encodeRangeKey inclusivity tie-break + decimal scale-independence", () => {
  const dec = (digits: string, scale: number): Value =>
    decimalValue(Decimal.fromDigitsScale(false, digits, scale));
  const numr = (loInc: boolean, hiInc: boolean): RangeValue =>
    rangeValue(dec("1", 0), dec("2", 0), loInc, hiInc) as RangeValue;
  const encN = (v: RangeValue) => hex(encodeRangeKey("decimal", v));
  // [1,2) < (1,2)  (inclusive lower before exclusive lower)
  assert.ok(encN(numr(true, false)) < encN(numr(false, false)));
  // (1,2) < (1,2]  (exclusive upper before inclusive upper)
  assert.ok(encN(numr(false, false)) < encN(numr(false, true)));
  // decimal scale-independence: [1.5,2) and [1.50,2) share a key (§2.5 wrinkle)
  const a = encN(rangeValue(dec("15", 1), dec("2", 0), true, false) as RangeValue);
  const b = encN(rangeValue(dec("150", 2), dec("2", 0), true, false) as RangeValue);
  assert.equal(a, b);
});
