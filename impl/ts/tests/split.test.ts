// splitStatements boundary correctness (spec/design/session.md §4.1): a `;` inside a string
// literal, dollar-quoted string, or line/block comment is not a statement boundary; empty spans are
// skipped. Per-core unit tested — the splitter adds no SQL semantics, so it is not in the shared
// corpus (CLAUDE.md §10). Mirrors impl/rust/src/split.rs tests.

import assert from "node:assert/strict";
import { test } from "node:test";
import { splitStatements, type StatementSpan } from "../src/lib.ts";

function texts(sql: string): string[] {
  return [...splitStatements(sql)].map((s) => s.text);
}
function spans(sql: string): StatementSpan[] {
  return [...splitStatements(sql)];
}

test("basic split and offsets", () => {
  assert.deepStrictEqual(spans("SELECT 1; SELECT 2"), [
    { text: "SELECT 1", offset: 0 },
    { text: "SELECT 2", offset: 10 },
  ]);
});

test("empty spans are skipped", () => {
  assert.deepStrictEqual(texts("SELECT 1;"), ["SELECT 1"]);
  assert.deepStrictEqual(texts(";;; SELECT 1 ;;;"), ["SELECT 1"]);
  assert.deepStrictEqual(texts(""), []);
  assert.deepStrictEqual(texts("   \n\t  "), []);
  assert.deepStrictEqual(texts(";"), []);
  assert.deepStrictEqual(texts("-- just a comment\n"), []);
  assert.deepStrictEqual(texts("/* block only */"), []);
});

test("semicolon in a string is not a boundary", () => {
  assert.deepStrictEqual(texts("INSERT INTO t VALUES ('a;b'); SELECT 1"), [
    "INSERT INTO t VALUES ('a;b')",
    "SELECT 1",
  ]);
  assert.deepStrictEqual(texts("SELECT 'it''s; ok'"), ["SELECT 'it''s; ok'"]);
});

test("semicolon in a comment is not a boundary", () => {
  assert.deepStrictEqual(texts("SELECT 1 -- a; b\n; SELECT 2"), ["SELECT 1", "SELECT 2"]);
  assert.deepStrictEqual(texts("SELECT /* a; b */ 1; SELECT 2"), [
    "SELECT /* a; b */ 1",
    "SELECT 2",
  ]);
  assert.deepStrictEqual(texts("SELECT /* /* ; */ */ 1"), ["SELECT /* /* ; */ */ 1"]);
});

test("semicolon in a dollar-quote is not a boundary", () => {
  assert.deepStrictEqual(texts("SELECT $$a;b$$; SELECT 2"), ["SELECT $$a;b$$", "SELECT 2"]);
  assert.deepStrictEqual(texts("SELECT $tag$a;$$;b$tag$; SELECT 2"), [
    "SELECT $tag$a;$$;b$tag$",
    "SELECT 2",
  ]);
  // `$1` is a bind parameter, not a dollar-quote — the `;` after it splits.
  assert.deepStrictEqual(texts("SELECT $1; SELECT 2"), ["SELECT $1", "SELECT 2"]);
});

test("trailing whitespace trimmed, interior comment kept", () => {
  const parts = spans("  SELECT 1  ;  SELECT /* x */ 2  ");
  assert.deepStrictEqual(parts[0], { text: "SELECT 1", offset: 2 });
  assert.deepStrictEqual(parts[1], { text: "SELECT /* x */ 2", offset: 15 });
});

test("no trailing semicolon still yields the last statement", () => {
  assert.deepStrictEqual(texts("SELECT 1; SELECT 2; SELECT 3"), [
    "SELECT 1",
    "SELECT 2",
    "SELECT 3",
  ]);
});
