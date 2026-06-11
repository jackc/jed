// Cross-check: the TS LZ4-block encoder must reproduce the byte-exact vectors in
// spec/fileformat/lz4_vectors.toml (CLAUDE.md §8; spec/fileformat/lz4.md §4). The encoder is
// pinned — a library would diverge (large-values.md §6) — so these vectors are what guarantee
// the Rust, Go, TS, and Ruby codecs emit identical compressed bytes (which the goldens and the
// deterministic cost both depend on). The decoder is checked by round-tripping each vector.
// Mirrors impl/rust/tests/lz4_vectors.rs and impl/go/lz4_test.go.

import { test } from "node:test";
import assert from "node:assert/strict";
import { lz4Compress, lz4Decompress } from "../src/lz4.ts";
import { readTomlTables, specPath } from "./tomlmini.ts";
import { bytesEqual, bytesToHex } from "./util.ts";

function unhex(s: string): Uint8Array {
  assert.equal(s.length % 2, 0, "odd hex length");
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(s.slice(i * 2, i * 2 + 2), 16);
  return out;
}

test("LZ4 encoder matches the pinned vectors", () => {
  const rows = readTomlTables(specPath("fileformat/lz4_vectors.toml"), "vector");
  assert.ok(rows.length >= 10, "vector corpus unexpectedly small");
  for (const row of rows) {
    const name = row.str("name");
    const input = unhex(row.str("input_hex"));
    const comp = lz4Compress(input);
    assert.equal(bytesToHex(comp), row.str("compressed_hex"), `${name}: compressed bytes`);
    assert.ok(bytesEqual(lz4Decompress(comp, input.length), input), `${name}: round-trip`);
  }
});

test("malformed LZ4 blocks are data_corrupted", () => {
  const cases: [string, number[], number][] = [
    ["truncated literals", [0x50], 5],
    ["zero offset", [0x14, 0x61, 0x00, 0x00, 0x00], 10],
    ["offset beyond prefix", [0x14, 0x61, 0x05, 0x00, 0x00], 10],
    ["output overflow", [0x1f, 0x61, 0x01, 0x00, 0xff, 0xff, 0x00], 4],
    ["length mismatch", [0x10, 0x61], 2],
  ];
  for (const [name, comp, rawLen] of cases) {
    assert.throws(
      () => lz4Decompress(Uint8Array.from(comp), rawLen),
      (e: Error) => e.message.includes("XX001") || /corrupt/.test(String(e)),
      name,
    );
  }
});
