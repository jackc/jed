// GIN jsonb_ops term bytes (spec/design/gin.md §4.1) checked byte-for-byte against the shared
// vectors spec/encoding/gin_jsonb_terms.toml — the on-disk byte contract the corpus cannot express
// (CLAUDE.md §10). Mirrors the Ruby reference in spec/encoding/verify.rb.

import assert from "node:assert/strict";
import { test } from "node:test";
import { ginJsonbIndexTerms, ginJsonbKeyTerm } from "../src/gin_jsonb.ts";
import { jsonbIn } from "../src/json.ts";
import { readGinJsonbVectors, specPath } from "./tomlmini.ts";

function hex(b: Uint8Array): string {
  return Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
}

const vectors = readGinJsonbVectors(specPath("encoding/gin_jsonb_terms.toml"));

test("gin jsonb_ops index term sets match the shared vectors", () => {
  assert.ok(vectors.index.length > 10);
  for (const c of vectors.index) {
    const got = ginJsonbIndexTerms(jsonbIn(c.doc)).map(hex);
    assert.deepStrictEqual(got, c.terms, `doc ${c.doc.slice(0, 60)}`);
  }
});

test("gin jsonb_ops query key terms match the shared vectors", () => {
  assert.ok(vectors.key.length > 3);
  for (const c of vectors.key) {
    assert.equal(hex(ginJsonbKeyTerm(c.key)), c.term, `key ${c.key.slice(0, 60)}`);
  }
});
