// Cross-check: the TS date parser/renderer reproduces the byte-exact vectors in
// spec/encoding/dates.toml (CLAUDE.md §8) — identical to the Rust/Go cores. The day count is a
// bigint (the core's uniform-integer discipline). Reuses the timestamp vector scanner.

import assert from "node:assert/strict";
import { test } from "node:test";
import { parseDate, renderDate } from "../src/date.ts";
import { EngineError } from "../src/errors.ts";
import { readTimestampCases, specPath } from "./tomlmini.ts";

test("date vectors (parse/render byte-identical to Rust/Go)", () => {
  const cases = readTimestampCases(specPath("encoding/dates.toml"));
  assert.ok(cases.length > 0, "no date vectors parsed");
  for (const c of cases) {
    assert.equal(c.typ, "date", "unexpected vector type");
    if (c.section === "parse") {
      const got = parseDate(c.fields.input!);
      assert.equal(got, BigInt(c.fields.days!), `parse ${JSON.stringify(c.fields.input)}`);
    } else if (c.section === "parse_error") {
      let code = "";
      try {
        parseDate(c.fields.input!);
      } catch (e) {
        if (e instanceof EngineError) code = e.code();
        else throw e;
      }
      assert.equal(code, c.fields.error, `parse ${JSON.stringify(c.fields.input)} error`);
    } else {
      const got = renderDate(BigInt(c.fields.days!));
      assert.equal(got, c.fields.text, `render ${c.fields.days}`);
    }
  }
});
