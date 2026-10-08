// JSON nesting-depth limit (CLAUDE.md §13; spec/design/json.md §6.4). Every producer bounds a
// json/jsonb document at MAX_JSON_DEPTH = 256 containers, so every recursive walk is stack-safe. The
// conformance corpus (spec/conformance/suites/resource/json_depth_limit.test) pins the parser,
// constructor, and jsonpath boundaries cross-core; the one gate it cannot reach is the on-disk jsonb
// decoder (gate 3), because a conformant engine never writes a body deeper than the limit. This
// crafts such a body directly.

import assert from "node:assert/strict";
import { test } from "node:test";
import type { ColType } from "../src/catalog.ts";
import { EngineError } from "../src/errors.ts";
import { inlineBodySpan, readInlineBody } from "../src/format.ts";
import { MAX_JSON_DEPTH } from "../src/json.ts";

const JSONB: ColType = { kind: "scalar", scalar: "jsonb" };

// nestedBody is a jsonb body of `depth` nested one-element arrays around a JSON null: `06 01`
// (NTAG_ARRAY, count 1) per level, then `00` (NTAG_NULL) — spec/fileformat/format.md.
function nestedBody(depth: number): Uint8Array {
  const out = new Uint8Array(depth * 2 + 1);
  for (let i = 0; i < depth; i++) {
    out[2 * i] = 0x06;
    out[2 * i + 1] = 0x01;
  }
  out[depth * 2] = 0x00;
  return out;
}

test("the json depth limit is 256", () => {
  assert.strictEqual(MAX_JSON_DEPTH, 256);
});

test("the jsonb decoder accepts a body exactly at the limit, in both decode modes", () => {
  const ok = nestedBody(MAX_JSON_DEPTH);
  const cur = { pos: 0 };
  const v = readInlineBody(JSONB, ok, cur, "construct");
  assert.strictEqual(v.kind, "jsonb");
  assert.strictEqual(cur.pos, ok.length);
  assert.strictEqual(inlineBodySpan(JSONB, ok, { pos: 0 }).length, ok.length);
});

test("the jsonb decoder rejects an over-deep body XX001 before recursing, in both decode modes", () => {
  // 200 000 levels would overflow V8's call stack as an uncoded RangeError without the gate.
  for (const depth of [MAX_JSON_DEPTH + 1, 200_000]) {
    const body = nestedBody(depth);
    const decoders: [string, () => unknown][] = [
      ["construct", () => readInlineBody(JSONB, body, { pos: 0 }, "construct")],
      ["skip", () => inlineBodySpan(JSONB, body, { pos: 0 })],
    ];
    for (const [mode, decode] of decoders) {
      assert.throws(
        decode,
        (e: unknown) =>
          e instanceof EngineError &&
          e.code() === "XX001" &&
          e.message === "XX001: jsonb nesting depth exceeds the maximum of 256",
        `depth ${depth}, ${mode} mode`,
      );
    }
  }
});
