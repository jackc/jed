// Cross-check: the TS key encoder must reproduce the byte-exact vectors in
// spec/encoding/integers.toml (CLAUDE.md §8). This is what guarantees the Rust, Go, and
// TS cores iterate keys in the same order.

import assert from "node:assert/strict";
import { test } from "node:test";
import { decodeInt, encodeInt, encodeNullable } from "../src/encoding.ts";
import { scalarTypeFromName } from "../src/types.ts";
import { readEncodingCases, specPath } from "./tomlmini.ts";
import { bytesToHex } from "./util.ts";

function invertBytes(b: Uint8Array): Uint8Array {
  const out = new Uint8Array(b.length);
  for (let i = 0; i < b.length; i++) out[i] = b[i]! ^ 0xff;
  return out;
}

test("encoding vectors match spec/encoding/integers.toml", () => {
  const cases = readEncodingCases(specPath("encoding/integers.toml"));
  let checked = 0;
  for (const c of cases) {
    const st = scalarTypeFromName(c.typ);
    assert.notEqual(st, undefined, `unknown type ${c.typ}`);
    let got: Uint8Array;
    switch (c.kind) {
      case "bare":
        got = encodeInt(st!, c.value);
        assert.equal(decodeInt(st!, got), c.value, `bare ${c.typ} ${c.value}: round-trip`);
        break;
      case "nullable":
        got = encodeNullable(st!, c.isNull ? null : c.value);
        break;
      case "descending":
        got = invertBytes(encodeNullable(st!, c.isNull ? null : c.value));
        break;
    }
    assert.equal(
      bytesToHex(got),
      c.bytes,
      `${c.kind} ${c.typ} value=${c.value} null=${c.isNull}`,
    );
    checked++;
  }
  assert.ok(checked > 0, "no encoding cases parsed");
});
