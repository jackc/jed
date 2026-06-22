// The pinned cross-language vectors from spec/design/benchmarks.md §4/§6.
import assert from "node:assert/strict";
import { test } from "node:test";

import { Checksum, Prng } from "../src/lib.ts";

test("splitmix64 pinned vectors", () => {
  const cases: [bigint, bigint[]][] = [
    [
      1n,
      [
        0x910a2dec89025cc1n,
        0xbeeb8da1658eec67n,
        0xf893a2eefb32555en,
        0x71c18690ee42c90bn,
        0x71bb54d8d101b5b9n,
      ],
    ],
    [
      1234567n,
      [
        0x599ed017fb08fc85n,
        0x2c73f08458540fa5n,
        0x883ebce5a3f27c77n,
        0x3fbef740e9177b3fn,
        0xe3b8346708cb5ecdn,
      ],
    ],
  ];
  for (const [seed, want] of cases) {
    const p = new Prng(seed);
    for (const w of want) {
      assert.equal(p.next(), w);
    }
  }
});

test("checksum pinned vector", () => {
  const c = new Checksum();
  c.int(1n);
  c.null();
  c.text("abc");
  c.endRow();
  c.int(-7n);
  c.endRow();
  assert.equal(c.hex(), "dd6e60407d30d28b");
});

test("text draw stays in contract", () => {
  const p = new Prng(1n);
  const s = p.text(8n, 32n);
  assert.ok(s.length >= 8 && s.length <= 32);
  assert.match(s, /^[a-z]+$/);
});
