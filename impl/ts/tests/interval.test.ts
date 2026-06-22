// Cross-check: the TS interval parser/renderer reproduces the byte-exact vectors in
// spec/encoding/intervals.toml (CLAUDE.md §8) — identical to the Rust/Go cores. All cascade/span
// math is bigint (number loses i64 precision).

import assert from "node:assert/strict";
import { test } from "node:test";
import { EngineError } from "../src/errors.ts";
import {
  type Interval,
  intervalCmp,
  intervalEncodeKey,
  intervalSpan,
  parseInterval,
  renderInterval,
} from "../src/interval.ts";
import { readIntervalCases, specPath } from "./tomlmini.ts";

const hex = (b: Uint8Array) => Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
const cmpBytes = (a: Uint8Array, b: Uint8Array) => {
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) if (a[i] !== b[i]) return a[i]! - b[i]!;
  return a.length - b.length;
};

test("interval vectors (parse/render byte-identical to Rust/Go)", () => {
  const cases = readIntervalCases(specPath("encoding/intervals.toml"));
  assert.ok(cases.length > 0, "no interval vectors parsed");
  for (const c of cases) {
    if (c.section === "parse") {
      const got = parseInterval(c.fields.input!);
      assert.equal(
        got.months,
        Number(c.fields.months),
        `parse ${JSON.stringify(c.fields.input)} months`,
      );
      assert.equal(got.days, Number(c.fields.days), `parse ${JSON.stringify(c.fields.input)} days`);
      assert.equal(
        got.micros,
        BigInt(c.fields.micros!),
        `parse ${JSON.stringify(c.fields.input)} micros`,
      );
    } else if (c.section === "parse_error") {
      let code = "";
      try {
        parseInterval(c.fields.input!);
      } catch (e) {
        if (e instanceof EngineError) code = e.code();
        else throw e;
      }
      assert.equal(code, c.fields.error, `parse ${JSON.stringify(c.fields.input)} error`);
    } else {
      const iv: Interval = {
        months: Number(c.fields.months),
        days: Number(c.fields.days),
        micros: BigInt(c.fields.micros!),
      };
      assert.equal(renderInterval(iv), c.fields.text, `render ${JSON.stringify(c.fields)}`);
    }
  }
});

test("interval span is canonical (1 mon == 30 days == 720:00:00)", () => {
  const oneMonth = parseInterval("1 mon");
  const thirtyDays = parseInterval("30 days");
  const hours = parseInterval("720:00:00");
  assert.equal(intervalCmp(oneMonth, thirtyDays), 0);
  assert.equal(intervalCmp(oneMonth, hours), 0);
  // span-equal but render distinctly
  assert.notEqual(renderInterval(oneMonth), renderInterval(thirtyDays));
  assert.ok(intervalCmp(parseInterval("1 day"), parseInterval("2 days")) < 0);
  assert.ok(intervalCmp(parseInterval("-1 day"), parseInterval("1 day")) < 0);
});

// The order-preserving KEY body (interval-span-i128, encoding.md §2.10): the 16-byte i128 span
// (bias 2^127 + big-endian). Sorting by intervalEncodeKey must equal span order; span-equal
// intervals share a key (the "equal but not identical" UNIQUE wrinkle, decimal's 1.5/1.50);
// byte-exact against the canonical vectors (spec/encoding/interval.toml).
test("interval encodeKey is order-preserving and byte-exact", () => {
  const iv = (months: number, days: number, micros: bigint): Interval => ({
    months,
    days,
    micros,
  });
  // Ascending by span — sorting by key must reproduce this order (sign boundary, zero, ±µs).
  const ordered = [
    iv(-1200, 0, 0n),
    iv(-1, 0, 0n),
    iv(0, -1, 0n),
    iv(0, 0, -1_000_000n),
    iv(0, 0, -1n),
    iv(0, 0, 0n),
    iv(0, 0, 1n),
    iv(0, 0, 1_000_000n),
    iv(0, 1, 0n),
    iv(1, 0, 0n),
    iv(1200, 0, 0n),
  ];
  const byKey = [...ordered].sort((a, b) => cmpBytes(intervalEncodeKey(a), intervalEncodeKey(b)));
  for (let i = 0; i < ordered.length; i++) {
    assert.equal(
      intervalSpan(byKey[i]!),
      intervalSpan(ordered[i]!),
      `encode_key order must equal span order at ${i}`,
    );
  }
  // Span-equal intervals share a key (1 mon == 30 days == 720:00:00) — the UNIQUE wrinkle.
  assert.equal(hex(intervalEncodeKey(iv(1, 0, 0n))), hex(intervalEncodeKey(iv(0, 30, 0n))));
  assert.equal(
    hex(intervalEncodeKey(iv(1, 0, 0n))),
    hex(intervalEncodeKey(iv(0, 0, 30n * 86_400_000_000n))),
  );
  // Byte-exact canonical vectors (the §2.10 worked-bytes table).
  assert.equal(hex(intervalEncodeKey(iv(0, 0, 0n))), "80000000000000000000000000000000");
  assert.equal(hex(intervalEncodeKey(iv(0, 0, 1n))), "80000000000000000000000000000001");
  assert.equal(hex(intervalEncodeKey(iv(0, 0, -1n))), "7fffffffffffffffffffffffffffffff");
  assert.equal(hex(intervalEncodeKey(iv(0, 1, 0n))), "8000000000000000000000141dd76000");
  assert.equal(hex(intervalEncodeKey(iv(1, 0, 0n))), "80000000000000000000025b7f3d4000");
  assert.equal(hex(intervalEncodeKey(iv(0, -1, 0n))), "7fffffffffffffffffffffebe228a000");
});
