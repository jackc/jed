// Cross-check the query-memory logical size schedule against the shared vectors in
// spec/cost/memory_sizes.toml (spec/design/memory.md §3). Every core runs the same vectors, so the
// measurement — and therefore a 54P05 threshold — is cross-core identical.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { rowBytes, utf8Length, valueBytes } from "../src/memsize.ts";
import type { Value } from "../src/value.ts";
import { memDb } from "./mem_db.ts";
import { specPath } from "./tomlmini.ts";

type Vector = { sql: string; setup: string[]; measure: string; bytes: number };

// readVectors parses the [[vector]] tables. The file's strings are TOML basic strings, whose escapes
// (\" and \\) are JSON's, so each value (and each inline string array) parses with JSON.parse.
function readVectors(): { schemaVersion: number; vectors: Vector[] } {
  const text = readFileSync(specPath("cost/memory_sizes.toml"), "utf8");
  let schemaVersion = 0;
  const vectors: Vector[] = [];
  let cur: Partial<Vector> | null = null;
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (line === "" || line.startsWith("#")) continue;
    if (line === "[[vector]]") {
      cur = { setup: [] };
      vectors.push(cur as Vector);
      continue;
    }
    const eq = line.indexOf("=");
    if (eq < 0) continue;
    const key = line.slice(0, eq).trim();
    const val = line.slice(eq + 1).trim();
    if (cur === null) {
      if (key === "schema_version") schemaVersion = Number(val);
      continue;
    }
    if (key === "sql") cur.sql = JSON.parse(val) as string;
    else if (key === "setup") cur.setup = JSON.parse(val) as string[];
    else if (key === "measure") cur.measure = JSON.parse(val) as string;
    else if (key === "bytes") cur.bytes = Number(val);
  }
  return { schemaVersion, vectors };
}

test("memory size vectors match spec/cost/memory_sizes.toml", () => {
  const { schemaVersion, vectors } = readVectors();
  assert.equal(schemaVersion, 1);
  assert.ok(vectors.length > 0, "expected [[vector]] entries");
  for (const v of vectors) {
    const db = memDb();
    const s = db.session();
    try {
      for (const stmt of v.setup) s.execute(stmt);
      const rows: Value[][] = [...s.query(v.sql)];
      assert.equal(rows.length, 1, `${v.sql}: one row`);
      let got: number;
      if (v.measure === "value") {
        assert.equal(rows[0]!.length, 1, `${v.sql}: one column`);
        got = valueBytes(rows[0]![0]!);
      } else if (v.measure === "row") {
        got = rowBytes(rows[0]!);
      } else {
        throw new Error(`unknown measure ${v.measure}`);
      }
      assert.equal(got, v.bytes, v.sql);
    } finally {
      s.close();
      db.close();
    }
  }
});

test("utf8Length matches TextEncoder without allocating", () => {
  const enc = new TextEncoder();
  for (const s of ["", "abc", "héllo", "🙂x", "\u{10ffff}", "a\ud800b", "\udc00", "z\ud83d"]) {
    assert.equal(utf8Length(s), enc.encode(s).length, JSON.stringify(s));
  }
});
