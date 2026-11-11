// The file-backed top-k heap over a bounded decimal (spec/design/spill.md §4.1): a single-table ORDER
// BY … LIMIT whose touched columns are fixed-width or numeric(p,s) keeps only its K best rows in a
// heap when they fit work_mem; any other touched type keeps the external sorter. Mirrors Rust
// (tests/varwidth_lanes.rs) and Go (varwidth_lanes_test.go).

import assert from "node:assert/strict";
import { test } from "node:test";
import { DEFAULT_WORK_MEM } from "../src/spill.ts";
import type { Session } from "../src/tooling.ts";
import { GT_SEED, outcome, show, withDb } from "./aggregate_lanes_util.ts";

function ordered(s: Session, sql: string): string[] {
  const got = outcome(s, sql);
  assert.ok(!("error" in got), `${sql}: ${show(got)}`);
  return got.rows === "" ? [] : got.rows.split("\n");
}

// Every LIMIT/OFFSET window must be that slice of the full sort, and the full sort must agree with the
// same query over a derived table, which the eager executor sorts — across fixed-width,
// bounded-decimal, unbounded and text keys (on a file the last two take the external sorter).
function checkTopN(s: Session): void {
  const KEYS = [
    "d DESC, id",
    "n, d DESC NULLS LAST, id",
    "big DESC NULLS FIRST, id",
    "wide DESC NULLS LAST, id",
    "s, id",
  ];
  const WINDOWS: [number, number][] = [
    [10, 0],
    [7, 5],
    [5000, 0],
    [0, 0],
    [3, 2999],
  ];
  // Every key over a projection that also touches text and an unbounded decimal (on a file: the
  // external sorter), and the keys that need neither over a fixed-width and bounded-decimal projection
  // (on a file: the top-k heap).
  const cases: [string, string][] = [
    ...KEYS.map((k): [string, string] => [k, "id, d, s, big, n, wide, f, m"]),
    ...KEYS.filter((k) => !k.includes("wide") && !k.startsWith("s,")).map((k): [string, string] => [
      k,
      "id, d, big, n, f, m",
    ]),
  ];
  for (const [key, cols] of cases) {
    for (const filter of ["", " WHERE n > 1 AND d <> 0"]) {
      const base = `SELECT ${cols} FROM gt${filter}`;
      const full = ordered(s, `${base} ORDER BY ${key}`);
      const eager = ordered(s, `SELECT * FROM (${base}) q ORDER BY ${key}`);
      assert.deepEqual(full, eager, `full sort vs eager sort: ${base} ORDER BY ${key}`);
      for (const [limit, offset] of WINDOWS) {
        const sql = `${base} ORDER BY ${key} LIMIT ${limit} OFFSET ${offset}`;
        assert.deepEqual(ordered(s, sql), full.slice(offset, offset + limit), sql);
      }
    }
  }
}

test("top-N windows match the full sort", () => {
  for (const file of [true, false]) {
    withDb(file, GT_SEED, (s) => {
      // The heap-or-sorter choice exists only on a file (spill.md §4.1).
      for (const workMem of file ? [3_000, DEFAULT_WORK_MEM] : [DEFAULT_WORK_MEM]) {
        s.setWorkMem(workMem);
        checkTopN(s);
      }
    });
  }
});
