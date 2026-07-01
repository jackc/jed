// uuid ⇄ bytea casts — the deliberate PostgreSQL divergences (spec/types/casts.toml,
// spec/design/types.md §14). PostgreSQL has NO bytea↔uuid cast (bytea::uuid / uuid::bytea is 42846
// cannot_coerce); jed adds both as EXPLICIT casts over the 16 raw bytes, so they SUCCEED where PG
// errors and cannot live in the PG-clean oracle corpus. The text↔uuid casts (which AGREE with PG)
// are oracle-checked in suites/cast/uuid.test and run on every core; a couple of smoke checks here
// run alongside (CLAUDE.md §10). Mirrors impl/rust/tests/cast_uuid.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database } from "../src/tooling.ts";
import { dbWith, errCode, query } from "./util.ts";

const CANON = "550e8400-e29b-41d4-a716-446655440000";
const HEX16 = "\\x550e8400e29b41d4a716446655440000"; // the same 16 bytes as a bytea hex literal
const RENDERED_BYTEA = "\\x550e8400e29b41d4a716446655440000"; // bytea renders \x + lowercase hex

// uuid → bytea is the 16 raw bytes (PG: 42846 — jed adds this cast).
test("uuid → bytea is the 16 raw bytes", () => {
  const db = dbWith([]);
  assert.deepEqual(query(db, `SELECT '${CANON}'::uuid::bytea`), [[RENDERED_BYTEA]]);
});

// bytea → uuid takes the 16 raw bytes (PG: 42846 — jed adds this cast); renders canonical lowercase.
test("bytea → uuid is the 16 raw bytes", () => {
  const db = dbWith([]);
  assert.deepEqual(query(db, `SELECT '${HEX16}'::bytea::uuid`), [[CANON]]);
});

// bytea → uuid requires EXACTLY 16 bytes; any other length traps 22P02 (the wrong-width body —
// there is no PG code to match, so jed reuses invalid_text_representation).
test("bytea → uuid wrong length traps 22P02", () => {
  const db = dbWith([]);
  for (const sql of [
    "SELECT '\\xabcd'::bytea::uuid", // 2 bytes
    "SELECT '\\x'::bytea::uuid", // empty (0 bytes)
    "SELECT '\\x550e8400e29b41d4a71644665544000000'::bytea::uuid", // 17 bytes
  ]) {
    assert.equal(
      errCode(() => db.execute(sql)),
      "22P02",
      sql,
    );
  }
});

// The casts round-trip through real columns (the runtime, non-constant path); NULL adapts.
test("uuid ⇄ bytea round-trip through columns", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, u uuid, b bytea)",
    `INSERT INTO t VALUES (1, '${CANON}', '${HEX16}'), (2, NULL, NULL)`,
  ]);
  assert.deepEqual(query(db, "SELECT u::bytea FROM t WHERE id = 1"), [[RENDERED_BYTEA]]);
  assert.deepEqual(query(db, "SELECT b::uuid FROM t WHERE id = 1"), [[CANON]]);
  assert.deepEqual(query(db, "SELECT u::bytea FROM t WHERE id = 2"), [["NULL"]]);
  assert.deepEqual(query(db, "SELECT b::uuid FROM t WHERE id = 2"), [["NULL"]]);
});

// text → uuid / uuid → text smoke check (the oracle-corpus behavior, run here per core too).
test("text ⇄ uuid smoke", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, s text, u uuid)",
    `INSERT INTO t VALUES (1, '550E8400-E29B-41D4-A716-446655440000', '${CANON}')`,
    "INSERT INTO t VALUES (2, 'not-a-uuid', NULL)",
  ]);
  // an UPPERCASE text value casts to the same uuid (renders lowercase)
  assert.deepEqual(query(db, "SELECT s::uuid FROM t WHERE id = 1"), [[CANON]]);
  assert.deepEqual(query(db, "SELECT u::text FROM t WHERE id = 1"), [[CANON]]);
  // a malformed runtime text → uuid traps 22P02 (the column path, not a literal)
  assert.equal(
    errCode(() => db.execute("SELECT s::uuid FROM t WHERE id = 2")),
    "22P02",
  );
});
