// Cross-check: the TS interval parser/renderer reproduces the byte-exact vectors in
// spec/encoding/intervals.toml (CLAUDE.md §8) — identical to the Rust/Go cores. All cascade/span
// math is bigint (number loses int64 precision).

import assert from "node:assert/strict";
import { test } from "node:test";
import { EngineError } from "../src/errors.ts";
import { type Interval, intervalCmp, parseInterval, renderInterval } from "../src/interval.ts";
import { readIntervalCases, specPath } from "./tomlmini.ts";

test("interval vectors (parse/render byte-identical to Rust/Go)", () => {
  const cases = readIntervalCases(specPath("encoding/intervals.toml"));
  assert.ok(cases.length > 0, "no interval vectors parsed");
  for (const c of cases) {
    if (c.section === "parse") {
      const got = parseInterval(c.fields.input!);
      assert.equal(got.months, Number(c.fields.months), `parse ${JSON.stringify(c.fields.input)} months`);
      assert.equal(got.days, Number(c.fields.days), `parse ${JSON.stringify(c.fields.input)} days`);
      assert.equal(got.micros, BigInt(c.fields.micros!), `parse ${JSON.stringify(c.fields.input)} micros`);
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
