// Cross-check: the TS timestamp parser/renderer reproduces the byte-exact vectors in
// spec/encoding/timestamps.toml (CLAUDE.md §8) — identical to the Rust/Go cores. All µs math
// is bigint (number loses precision past 2^53).

import assert from "node:assert/strict";
import { test } from "node:test";
import { EngineError } from "../src/errors.ts";
import {
  parseTimestamp,
  parseTimestamptz,
  renderTimestamp,
  renderTimestamptz,
} from "../src/timestamp.ts";
import { readTimestampCases, specPath } from "./tomlmini.ts";

function tsParse(typ: string, input: string): bigint {
  return typ === "timestamp" ? parseTimestamp(input) : parseTimestamptz(input);
}

function tsRender(typ: string, m: bigint): string {
  return typ === "timestamp" ? renderTimestamp(m) : renderTimestamptz(m);
}

test("timestamp vectors (parse/render byte-identical to Rust/Go)", () => {
  const cases = readTimestampCases(specPath("encoding/timestamps.toml"));
  assert.ok(cases.length > 0, "no timestamp vectors parsed");
  for (const c of cases) {
    if (c.section === "parse") {
      const got = tsParse(c.typ, c.fields.input!);
      assert.equal(got, BigInt(c.fields.micros!), `${c.typ} parse ${JSON.stringify(c.fields.input)}`);
    } else if (c.section === "parse_error") {
      let code = "";
      try {
        tsParse(c.typ, c.fields.input!);
      } catch (e) {
        if (e instanceof EngineError) code = e.code();
        else throw e;
      }
      assert.equal(code, c.fields.error, `${c.typ} parse ${JSON.stringify(c.fields.input)} error`);
    } else {
      const got = tsRender(c.typ, BigInt(c.fields.micros!));
      assert.equal(got, c.fields.text, `${c.typ} render ${c.fields.micros}`);
    }
  }
});
