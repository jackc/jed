// Cross-check: the TS key encoder must reproduce the byte-exact vectors in
// spec/encoding/integers.toml (CLAUDE.md §8). This is what guarantees the Rust, Go, and
// TS cores iterate keys in the same order.

import assert from "node:assert/strict";
import { test } from "node:test";
import { decodeInt, encodeBool, encodeInt, encodeNullable } from "../src/encoding.ts";
import { scalarTypeFromName } from "../src/types.ts";
import { parseUuid, render, uuidValue } from "../src/value.ts";
import { type EncCase, readEncodingCases, specPath } from "./tomlmini.ts";
import { bytesToHex } from "./util.ts";

function invertBytes(b: Uint8Array): Uint8Array {
  const out = new Uint8Array(b.length);
  for (let i = 0; i < b.length; i++) out[i] = b[i]! ^ 0xff;
  return out;
}

// uuidBytes parses a uuid case's canonical-string value to its 16 raw key bytes.
function uuidBytes(c: EncCase): Uint8Array {
  const r = parseUuid(c.strValue);
  if ("error" in r) throw new Error(`uuid ${c.strValue}: ${r.error}`);
  return r.bytes;
}

// nullableUuid is the nullable key slot for a uuid case: 0x01 NULL, else 0x00 + 16 bytes.
function nullableUuid(c: EncCase): Uint8Array {
  if (c.isNull) return new Uint8Array([0x01]);
  const b = uuidBytes(c);
  const out = new Uint8Array(1 + b.length);
  out.set(b, 1);
  return out;
}

// nullableBool is the nullable key slot for a boolean case: 0x01 NULL, else 0x00 + the 1-byte body.
function nullableBool(c: EncCase): Uint8Array {
  if (c.isNull) return new Uint8Array([0x01]);
  const b = encodeBool(c.boolValue);
  const out = new Uint8Array(1 + b.length);
  out.set(b, 1);
  return out;
}

test("encoding vectors match spec/encoding/integers.toml", () => {
  const cases = readEncodingCases(specPath("encoding/integers.toml"));
  let checked = 0;
  for (const c of cases) {
    const st = scalarTypeFromName(c.typ);
    assert.notEqual(st, undefined, `unknown type ${c.typ}`);
    let got: Uint8Array;
    if (c.typ === "uuid") {
      // uuid is the first non-integer key: the bare 16 bytes parseUuid produces (encoding.md
      // §2.7); nullable/descending use the shared presence-tag / inversion framing.
      switch (c.kind) {
        case "bare":
          got = uuidBytes(c);
          assert.equal(render(uuidValue(got)), c.strValue, `bare uuid ${c.strValue}: round-trip`);
          break;
        case "nullable":
          got = nullableUuid(c);
          break;
        case "descending":
          got = invertBytes(nullableUuid(c));
          break;
      }
      assert.equal(
        bytesToHex(got!),
        c.bytes,
        `${c.kind} uuid value=${c.strValue} null=${c.isNull}`,
      );
      checked++;
      continue;
    }
    if (c.typ === "boolean") {
      // boolean is the second non-integer key: a single bool-byte (0x00 false / 0x01 true,
      // encoding.md §2.9); nullable/descending use the shared presence-tag / inversion framing.
      switch (c.kind) {
        case "bare":
          got = encodeBool(c.boolValue);
          break;
        case "nullable":
          got = nullableBool(c);
          break;
        case "descending":
          got = invertBytes(nullableBool(c));
          break;
      }
      assert.equal(
        bytesToHex(got!),
        c.bytes,
        `${c.kind} boolean value=${c.boolValue} null=${c.isNull}`,
      );
      checked++;
      continue;
    }
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
    assert.equal(bytesToHex(got!), c.bytes, `${c.kind} ${c.typ} value=${c.value} null=${c.isNull}`);
    checked++;
  }
  assert.ok(checked > 0, "no encoding cases parsed");
});
