// SQL comments are lexer whitespace (spec/design/grammar.md §33): `--` line comments
// run to end of line (and ALWAYS start outside a string, even abutting a token —
// `1--2` is `1`); `/* */` block comments NEST per PG / the SQL standard; an
// unterminated block is 42601; comment openers inside a string literal are text.

import assert from "node:assert/strict";
import { test } from "node:test";
import type { Session } from "../src/tooling.ts";
import { type Handle, dbWith, errCode, query } from "./util.ts";

function setup(): Session {
  return dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, v i32, s text)",
    "INSERT INTO t VALUES (1, 10, '--x /*y*/')",
  ]);
}

// one runs a query expected to produce exactly one value; returns it rendered.
function one(db: Handle, sql: string): string {
  const rows = query(db, sql);
  assert.equal(rows.length, 1, sql);
  assert.equal(rows[0]!.length, 1, sql);
  return rows[0]![0]!;
}

test("line comments run to end of line", () => {
  const db = setup();
  // Trailing comment; the statement continues on the next line.
  assert.equal(one(db, "SELECT v -- trailing\nFROM t WHERE id = 1"), "10");
  // Leading comment line.
  assert.equal(one(db, "-- leading\nSELECT v FROM t WHERE id = 1"), "10");
  // A comment at the very end of input (no newline) is fine.
  assert.equal(one(db, "SELECT v FROM t WHERE id = 1 -- done"), "10");
});

test("two hyphens start a comment even abutting a token", () => {
  const db = setup();
  // `v--1` is `v` then a comment (PG) — NOT `v - (-1)`.
  assert.equal(one(db, "SELECT v--1\nFROM t WHERE id = 1"), "10");
  // Separated operators still mean double negation.
  assert.equal(one(db, "SELECT v - -1 FROM t WHERE id = 1"), "11");
});

test("block comments separate tokens and nest", () => {
  const db = setup();
  // A block comment is a token separator.
  assert.equal(one(db, "SELECT/*c*/v/*c*/FROM t WHERE id = 1"), "10");
  // Blocks nest: the comment ends only when the depth returns to zero.
  assert.equal(one(db, "SELECT /* a /* b */ still comment */ v FROM t WHERE id = 1"), "10");
  // A quote inside a block comment is ordinary comment text.
  assert.equal(one(db, "SELECT /* it's fine */ v FROM t WHERE id = 1"), "10");
});

test("comment openers inside a string are text", () => {
  const db = setup();
  assert.equal(one(db, "SELECT s FROM t WHERE id = 1"), "--x /*y*/");
});

test("an unterminated block comment is 42601", () => {
  const db = setup();
  for (const sql of [
    "SELECT v FROM t /* unterminated",
    "SELECT v FROM t /* outer /* inner */ still open",
    "SELECT v FROM t /*/", // the close cannot overlap the open
  ]) {
    assert.equal(
      errCode(() => db.execute(sql)),
      "42601",
      sql,
    );
  }
});

test("a stray close is not comment syntax", () => {
  const db = setup();
  // `*/` with no opener lexes as `*` `/` and fails at parse.
  assert.equal(
    errCode(() => db.execute("SELECT v */ 1 FROM t")),
    "42601",
  );
});

test("comment-only input is no statement", () => {
  const db = setup();
  for (const sql of ["-- nothing here", "/* nothing here */", "  /* a */ -- b"]) {
    assert.equal(
      errCode(() => db.execute(sql)),
      "42601",
      sql,
    );
  }
});
