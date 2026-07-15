import assert from "node:assert/strict";
import { test } from "node:test";
import { crc32 as zlibCrc32 } from "node:zlib";

// The supported Node tooling/public graph reaches file.ts, which selects zlib before exposing a
// database operation. Browser/OPFS entry paths never import that module and retain slicing-by-8.
import "../src/tooling.ts";
import { crc32Ieee, crc32SlicingBy8, selectedCrc32Backend } from "../src/crc32.ts";
import { pageCrc } from "../src/format.ts";

function nodeZlibCrc32(previous: number, data: Uint8Array): number {
  return zlibCrc32(data, previous) >>> 0;
}

function slowCrc32(previous: number, data: Uint8Array): number {
  let crc = (previous ^ 0xffffffff) >>> 0;
  for (const byte of data) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit++) {
      const mask = -(crc & 1);
      crc = ((crc >>> 1) ^ (0xedb88320 & mask)) >>> 0;
    }
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function deterministicBytes(length: number): Uint8Array {
  const bytes = new Uint8Array(length);
  for (let i = 0; i < bytes.length; i++) bytes[i] = (i * 37 + Math.floor(i / 7) + 11) & 0xff;
  return bytes;
}

test("CRC-32 backends match the slow oracle across lengths and alignments", () => {
  assert.equal(selectedCrc32Backend(), "node:zlib");
  const backing = deterministicBytes(8 + 8192);
  const lengths = [
    ...Array.from({ length: 65 }, (_, i) => i),
    127,
    128,
    129,
    255,
    256,
    257,
    1023,
    4096,
    8192,
  ];
  for (let offset = 0; offset < 8; offset++) {
    for (const length of lengths) {
      if (offset + length > backing.length) continue;
      const data = backing.subarray(offset, offset + length);
      const expected = slowCrc32(0, data);
      assert.equal(crc32SlicingBy8(0, data), expected, `slicing offset=${offset} len=${length}`);
      assert.equal(nodeZlibCrc32(0, data), expected, `zlib offset=${offset} len=${length}`);
      assert.equal(crc32Ieee(data), expected, `selected offset=${offset} len=${length}`);
    }
  }
});

test("CRC-32 backends compose at every page split", () => {
  const data = deterministicBytes(8192);
  const expected = slowCrc32(0, data);
  for (let split = 0; split <= data.length; split++) {
    const first = data.subarray(0, split);
    const second = data.subarray(split);
    assert.equal(
      crc32SlicingBy8(crc32SlicingBy8(0, first), second),
      expected,
      `slicing split=${split}`,
    );
    assert.equal(nodeZlibCrc32(nodeZlibCrc32(0, first), second), expected, `zlib split=${split}`);
  }
});

test("page CRC covers both spans and excludes its stored field", () => {
  const page = deterministicBytes(256);
  const covered = new Uint8Array(page.length - 4);
  covered.set(page.subarray(0, 12), 0);
  covered.set(page.subarray(16), 12);
  assert.equal(pageCrc(page), slowCrc32(0, covered));

  const original = pageCrc(page);
  for (let i = 12; i < 16; i++) page[i] ^= 0xff;
  assert.equal(pageCrc(page), original, "stored checksum field must be excluded");

  for (const offset of [0, 11, 16, page.length - 1]) {
    const before = pageCrc(page);
    page[offset] ^= 0x80;
    assert.notEqual(pageCrc(page), before, `protected offset ${offset}`);
    page[offset] ^= 0x80;
  }
});
