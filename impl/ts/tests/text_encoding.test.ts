// Host-API behavior the corpus cannot express (CLAUDE.md §10): text the engine receives from
// outside — SQL text, a text parameter — must be valid UTF-8 without NUL (spec/design/types.md
// §11). A JS string can carry a NUL or a lone surrogate (no UTF-8 form); either is 22021 with
// PostgreSQL's message naming the bytes, the same report the Go and Rust cores give for the same
// text. The bind-parameter index cap ($65535, api.md §5) is pinned here too, beside the corpus.

import assert from "node:assert/strict";
import { test } from "node:test";
import { EngineError, queryOutcome } from "../src/tooling.ts";
import { lex, lexWithPositions } from "../src/lexer.ts";
import { memDb } from "./mem_db.ts";

function errorOf(fn: () => unknown): EngineError {
  try {
    fn();
  } catch (e) {
    if (e instanceof EngineError) return e;
    throw e;
  }
  throw new Error("expected an EngineError");
}

test("a NUL or a lone surrogate in SQL text is 22021 without a position", () => {
  for (const sql of ["SELECT 'a\u0000b'", 'SELECT "a\u0000b"', "SELECT 1 -- \u0000"]) {
    const err = errorOf(() => lexWithPositions(sql));
    assert.equal(err.code(), "22021", sql);
    assert.equal(err.message, '22021: invalid byte sequence for encoding "UTF8": 0x00');
    assert.equal(err.position, undefined);
  }
  assert.equal(errorOf(() => lex("SELECT 'a\u0000b'")).code(), "22021");
  // A lone high surrogate: the three bytes its naive UTF-8 encoding would have.
  const lone = errorOf(() => lexWithPositions("SELECT 'a\ud800b'"));
  assert.equal(lone.code(), "22021");
  assert.equal(lone.message, '22021: invalid byte sequence for encoding "UTF8": 0xed 0xa0 0x80');
  // A lone low surrogate, and a pair (a code point above U+FFFF) which is fine.
  assert.equal(errorOf(() => lex("SELECT '\udc00'")).code(), "22021");
  assert.doesNotThrow(() => lex("SELECT 'a\u{1F600}b'"));
});

test("a NUL in a text parameter is 22021 and stores nothing", () => {
  const db = memDb();
  queryOutcome(db, "CREATE TABLE t (id i32 PRIMARY KEY, v text)");
  const err = errorOf(() =>
    queryOutcome(db, "INSERT INTO t VALUES (1, $1)", [{ kind: "text", text: "a\u0000b" }]),
  );
  assert.equal(err.code(), "22021");
  assert.equal(err.message, '22021: invalid byte sequence for encoding "UTF8": 0x00');
  assert.equal(
    errorOf(() =>
      queryOutcome(db, "INSERT INTO t VALUES (1, $1)", [{ kind: "text", text: "\ud800" }]),
    ).code(),
    "22021",
  );
  const count = queryOutcome(db, "SELECT count(*) FROM t");
  assert.equal(count.kind, "query");
  assert.deepEqual(count.rows[0]![0], { kind: "int", int: 0n });
  queryOutcome(db, "INSERT INTO t VALUES (1, $1)", [{ kind: "text", text: "hé\u{1F600}" }]);
});

test("the bind-parameter index is capped at $65535", () => {
  assert.doesNotThrow(() => lex("SELECT $65535"));
  const err = errorOf(() => lex("SELECT $65536"));
  assert.equal(err.code(), "42601");
  assert.equal(err.message, "42601: parameter number out of range: $65536 (the maximum is $65535)");
  assert.equal(errorOf(() => lex("SELECT $4294967295")).code(), "42601");
  assert.equal(errorOf(() => lex("SELECT $99999999999999999999999")).code(), "42601");
});
