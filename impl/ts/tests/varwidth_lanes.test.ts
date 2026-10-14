// A4 — variable-width columnar lanes + encoded-span predicates (spec/design/packed-leaf.md §11 Track
// A4). On a paged database a single-table aggregate or bare-column projection reads its touched
// variable-width columns in place: IS [NOT] NULL from the leaf's null directory, text/bytea =/<> as a
// byte compare against the stored span, array @>/<@/&& against the stored element bodies, and
// COUNT(col) from nullness alone; every other predicate (and every value a consumer needs) goes through
// the ordinary evaluator over resolved values. The lanes run only on the unmetered lane, so the same
// session with a (never-reached) max_cost runs the row path — the oracle. For every query shape both
// must agree on rows AND cost, over NULLs, empty values, embedded 0x00 bytes, multibyte text, NULL
// array elements, compressed and out-of-line values (which fall back to the evaluator), and a writer's
// uncommitted (Decoded) leaves. Mirrors Rust (tests/varwidth_lanes.rs) and Go
// (varwidth_lanes_test.go).

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import type { ColType } from "../src/catalog.ts";
import { encodeValue } from "../src/format.ts";
import { arrayElementSpans, utf8CodePoints } from "../src/span_filter.ts";
import { createDatabase, openDatabase, type Session } from "../src/tooling.ts";
import { render, textValue, type Value } from "../src/value.ts";
import { queryOutcome } from "./util.ts";

const SEED = [
  "CREATE TABLE vt (id i32 PRIMARY KEY, n i32, s text, b bytea, tags text[], nums i32[], d decimal)",
  "INSERT INTO vt VALUES " +
    "(1, 1, 'a', '\\x00'::bytea, '{a,b}', '{1,2}', 1.5), " +
    "(2, 2, '', '\\x'::bytea, '{}', '{}', NULL), " +
    "(3, 3, NULL, NULL, NULL, NULL, 2.25), " +
    "(4, 4, 'é', '\\x6100'::bytea, '{é,NULL}', '{NULL,3}', 10), " +
    "(5, 5, 'ñandú', '\\x0000'::bytea, '{NULL}', '{-1}', -3.125), " +
    "(6, 6, 'a', '\\x61'::bytea, '{b,a,a}', '{3,3}', 0), " +
    "(7, 7, 'ab', '\\x6162'::bytea, '{{a,b},{c,NULL}}', '{{1,2},{3,4}}', 7)",
  // A compressed value set (large-values.md §13): a highly compressible text, bytea, and text array —
  // each falls back to the evaluator. Row 9 (incompressible, out of line) is built by the test.
  "INSERT INTO vt VALUES (8, 8, repeat('x', 30000), decode(repeat('ab', 9000), 'hex'), ARRAY[repeat('a', 9000), 'a'], '{5}', 1)",
  "INSERT INTO vt SELECT g, g % 7, 'v' || (g % 13)::text, NULL, ARRAY['t' || (g % 5)::text], ARRAY[(g % 4)::i32], NULL FROM generate_series(10, 400) g",
];

const QUERIES = [
  // Nullness atoms, COUNT(col).
  "SELECT count(*) FROM vt WHERE s IS NULL",
  "SELECT count(*) FROM vt WHERE s IS NOT NULL",
  "SELECT count(*) FROM vt WHERE tags IS NULL",
  "SELECT count(*) FROM vt WHERE d IS NOT NULL",
  "SELECT count(s), count(b), count(tags), count(nums), count(d) FROM vt",
  // Text / bytea equality atoms (either operand order, empty, multibyte, embedded zero bytes).
  "SELECT count(*) FROM vt WHERE s = 'a'",
  "SELECT count(*) FROM vt WHERE 'a' = s",
  "SELECT count(*) FROM vt WHERE s <> 'a'",
  "SELECT count(*) FROM vt WHERE s = ''",
  "SELECT count(*) FROM vt WHERE s = 'é'",
  "SELECT count(*) FROM vt WHERE s = 'ñandú'",
  "SELECT count(*) FROM vt WHERE s = 'v3'",
  "SELECT count(*) FROM vt WHERE s <> 'v3'",
  "SELECT count(*) FROM vt WHERE b = '\\x00'::bytea",
  "SELECT count(*) FROM vt WHERE b = '\\x'::bytea",
  "SELECT count(*) FROM vt WHERE b = '\\x0000'::bytea",
  "SELECT count(*) FROM vt WHERE b <> '\\x6100'::bytea",
  // Array atoms (constant on either side; NULL elements; empty; multidimensional; ints).
  "SELECT count(*) FROM vt WHERE tags @> '{a}'::text[]",
  "SELECT count(*) FROM vt WHERE tags @> ARRAY['a']",
  "SELECT count(*) FROM vt WHERE tags @> ARRAY['a', 'b']",
  "SELECT count(*) FROM vt WHERE tags @> '{}'::text[]",
  "SELECT count(*) FROM vt WHERE tags @> ARRAY[NULL]::text[]",
  "SELECT count(*) FROM vt WHERE tags <@ '{a,b,é}'::text[]",
  "SELECT count(*) FROM vt WHERE tags && '{b,é,t1}'::text[]",
  "SELECT count(*) FROM vt WHERE '{a}'::text[] <@ tags",
  "SELECT count(*) FROM vt WHERE '{a,b,c}'::text[] @> tags",
  "SELECT count(*) FROM vt WHERE tags @> '{c}'::text[]",
  "SELECT count(*) FROM vt WHERE nums @> '{3}'::i32[]",
  "SELECT count(*) FROM vt WHERE nums && '{1,-1}'::i32[]",
  "SELECT count(*) FROM vt WHERE nums <@ '{1,2,3}'::i32[]",
  // Mixed: atoms beside generic predicates, OR / NOT (generic), short-circuit order.
  "SELECT count(*) FROM vt WHERE s = 'a' AND n > 1",
  "SELECT count(*) FROM vt WHERE n > 1 AND s = 'a' AND tags @> '{a}'::text[]",
  "SELECT count(*) FROM vt WHERE s IS NULL OR n = 2",
  "SELECT count(*) FROM vt WHERE NOT (s = 'a')",
  "SELECT count(*) FROM vt WHERE s LIKE 'v1%' AND b IS NULL",
  "SELECT count(*) FROM vt WHERE length(s) > 100",
  // Aggregates that resolve values.
  "SELECT max(s), min(s) FROM vt",
  "SELECT max(s) FROM vt WHERE tags @> '{t2}'::text[]",
  "SELECT min(b), max(d) FROM vt",
  "SELECT n, count(tags) FROM vt GROUP BY n",
  "SELECT n, max(s) FROM vt WHERE s <> 'v1' GROUP BY n",
  // Projection lane: survivors only, variable-width projected columns, filter-only columns.
  "SELECT id, s FROM vt",
  "SELECT id, s, tags FROM vt WHERE s = 'a'",
  "SELECT tags, b FROM vt WHERE n = 4",
  "SELECT id FROM vt WHERE tags && '{a,é}'::text[]",
  "SELECT s, s FROM vt WHERE s IS NOT NULL AND id < 12",
  "SELECT id, s FROM vt WHERE length(s) > 100",
  "SELECT id, d FROM vt WHERE d IS NOT NULL",
  "SELECT id FROM vt WHERE id >= 5 AND s = 'v5'",
];

// run returns one query's rows (rendered, sorted — no query here has an ORDER BY) and cost.
function run(s: Session, sql: string, params: Value[]): { rows: string; cost: bigint } {
  const o = queryOutcome(s, sql, params);
  if (o.kind !== "query") throw new Error(`expected a query: ${sql}`);
  const rows = o.rows.map((r) => r.map((v) => render(v)).join("\x1f")).sort();
  return { rows: rows.join("\n"), cost: o.cost };
}

// check asserts the lane (unmetered) and the row path (a ceiling it never reaches) agree on rows and
// cost.
function check(s: Session, sql: string, params: Value[] = []): void {
  s.setMaxCost(0n);
  const lane = run(s, sql, params);
  s.setMaxCost(1n << 60n);
  const row = run(s, sql, params);
  s.setMaxCost(0n);
  assert.deepEqual(lane, row, `lane vs row path: ${sql}`);
}

function checkAll(s: Session): void {
  for (const q of QUERIES) check(s, q);
  // A bind parameter is a row-independent constant too.
  check(s, "SELECT count(*) FROM vt WHERE s = $1", [textValue("v4")]);
  check(s, "SELECT id FROM vt WHERE s <> $1 AND n = 3", [textValue("v3")]);
}

test("variable-width lanes match the row path", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-varwidth-lanes-"));
  try {
    const path = join(dir, "vt.jed");
    const filedb = createDatabase({ path, skipFsync: true });
    const seed = filedb.session();
    for (const sql of SEED) seed.execute(sql);
    // Incompressible text and a large text array, stored out of line (large-values.md §12).
    let x = 0x9e3779b97f4a7c15n;
    const mask = (1n << 64n) - 1n;
    const noise = (n: number): string => {
      let out = "";
      for (let i = 0; i < n; i++) {
        x ^= (x << 13n) & mask;
        x ^= x >> 7n;
        x ^= (x << 17n) & mask;
        out += String.fromCharCode(97 + Number(x % 26n));
      }
      return out;
    };
    const long = noise(30000);
    const elems = Array.from({ length: 700 }, () => noise(40));
    seed.execute(
      `INSERT INTO vt VALUES (9, 9, '${long}', NULL, '{${elems.join(",")}}', NULL, NULL)`,
    );
    filedb.close();

    const paged = openDatabase(path, { skipFsync: true });
    try {
      const s = paged.session();
      // Clean (Packed) leaves faulted from the file.
      checkAll(s);
      // A writer's uncommitted changes live in Decoded leaves: the atoms fall back to the evaluator.
      s.execute("BEGIN");
      s.execute("INSERT INTO vt VALUES (1000, 1, 'a', '\\x00'::bytea, '{a}', '{3}', 1)");
      s.execute("UPDATE vt SET s = 'é' WHERE id = 6");
      checkAll(s);
      s.execute("ROLLBACK");
    } finally {
      paged.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

const TEXT_ARRAY: ColType = { kind: "array", elem: { kind: "scalar", scalar: "text" } };

function textArrayBody(elems: (string | null)[]): Uint8Array {
  const elements: Value[] = elems.map((e) => (e === null ? { kind: "null" } : textValue(e)));
  const v: Value =
    elements.length === 0
      ? { kind: "array", dims: [], lbounds: [], elements }
      : { kind: "array", dims: [elements.length], lbounds: [1], elements };
  return encodeValue(TEXT_ARRAY, v).subarray(1);
}

test("utf8CodePoints counts well-formed UTF-8 and rejects the rest", () => {
  const enc = new TextEncoder();
  for (const s of ["", "a", "é", "ñandú", "😀豈", "﻿a"]) {
    const b = enc.encode(s);
    assert.equal(utf8CodePoints(b, 0, b.length), [...s].length, s);
  }
  for (const bad of [
    [0xff],
    [0xc0, 0x80],
    [0xed, 0xa0, 0x80],
    [0xf4, 0x90, 0x80, 0x80],
    [0xe2, 0x82],
  ]) {
    const b = Uint8Array.from(bad);
    assert.equal(utf8CodePoints(b, 0, b.length), -1, bad.join(","));
  }
});

test("arrayElementSpans walks well-formed bodies and rejects malformed ones", () => {
  const spans: number[] = [];
  const text = { width: 0, text: true };
  const body = textArrayBody(["a", null, "é", ""]);
  assert.equal(arrayElementSpans(body, 0, body.length, text, spans), true);
  const elems = [];
  for (let s = 0; s < spans.length; s += 2)
    elems.push([...body.subarray(spans[s]!, spans[s + 1]!)]);
  assert.deepEqual(elems, [
    [0, 1, 0x61],
    [0, 2, 0xc3, 0xa9],
    [0, 0],
  ]);
  const empty = textArrayBody([]);
  assert.equal(arrayElementSpans(empty, 0, empty.length, text, spans), false);
  assert.equal(spans.length, 0);
  // Truncated, trailing bytes, a reserved flag bit, ndim over the maximum.
  const ab = textArrayBody(["ab", "c"]);
  assert.equal(arrayElementSpans(ab, 0, ab.length - 1, text, spans), null);
  const trailing = Uint8Array.from([...ab, 0]);
  assert.equal(arrayElementSpans(trailing, 0, trailing.length, text, spans), null);
  const flags = Uint8Array.from(ab);
  flags[1] = 0x02;
  assert.equal(arrayElementSpans(flags, 0, flags.length, text, spans), null);
  const ndim = Uint8Array.from(ab);
  ndim[0] = 7;
  assert.equal(arrayElementSpans(ndim, 0, ndim.length, text, spans), null);
  // An element that is not valid UTF-8 leaves the decoder to report it; as bytea it is well-formed.
  const inv = Uint8Array.from(textArrayBody(["ab"]));
  inv[inv.length - 1] = 0xff;
  assert.equal(arrayElementSpans(inv, 0, inv.length, text, spans), null);
  assert.equal(arrayElementSpans(inv, 0, inv.length, { width: 0, text: false }, spans), false);
});
