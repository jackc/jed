// GIN posting runs (spec/design/gin.md §4.5) checked byte-for-byte against the shared vectors
// spec/encoding/gin_posting_runs.toml: the run codec, the build's greedy packing, and run maintenance
// over an in-memory index store — the on-disk byte contract the corpus cannot express (CLAUDE.md §10).
// Mirrors the Ruby reference in spec/encoding/verify.rb.

import assert from "node:assert/strict";
import { test } from "node:test";
import { ginDecodeRun, ginEncodeRun, ginInsert, ginPackRuns, ginRemove } from "../src/gin_runs.ts";
import { TableStore } from "../src/storage.ts";
import { readTomlTables, specPath } from "./tomlmini.ts";

function hex(b: Uint8Array): string {
  return Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
}

function unhex(s: string): Uint8Array {
  const out = new Uint8Array(s.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = Number.parseInt(s.slice(2 * i, 2 * i + 2), 16);
  return out;
}

const file = specPath("encoding/gin_posting_runs.toml");

test("gin posting runs encode and decode as the shared vectors", () => {
  const rows = readTomlTables(file, "run");
  assert.ok(rows.length >= 4);
  for (const r of rows) {
    const term = unhex(r.str("term"));
    const keys = r.strs("keys").map(unhex);
    const run = ginEncodeRun(term, keys);
    assert.equal(hex(run), r.str("run"));
    assert.deepStrictEqual(ginDecodeRun(term.length, run).map(hex), r.strs("keys"));
  }
});

test("gin posting runs pack greedily as the shared vectors", () => {
  const rows = readTomlTables(file, "pack");
  assert.ok(rows.length >= 2);
  for (const r of rows) {
    const term = unhex(r.str("term"));
    assert.deepStrictEqual(ginPackRuns(term, r.strs("keys").map(unhex)).map(hex), r.strs("runs"));
  }
});

test("gin posting run maintenance matches the shared histories", () => {
  const rows = readTomlTables(file, "history");
  assert.ok(rows.length >= 4);
  for (const r of rows) {
    const term = unhex(r.str("term"));
    const store = new TableStore({ c: 4080, tag: 0 }, []);
    for (const op of r.strs("ops")) {
      const key = unhex(op.slice(1));
      if (op.startsWith("+")) ginInsert(store, term, key);
      else ginRemove(store, term, key);
    }
    assert.deepStrictEqual(
      store.entriesInKeyOrder().map((e) => hex(e.key)),
      r.strs("runs"),
      `history ${r.str("name")}`,
    );
  }
});
