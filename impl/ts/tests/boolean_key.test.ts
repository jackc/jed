// Boolean as a key (spec/design/types.md §9, encoding.md §2.9) — boolean is the second
// non-integer key type after uuid. Its bool-byte key (0x00 false < 0x01 true) drives a
// boolean PRIMARY KEY, a boolean member of a composite key, and a secondary index on a
// boolean column. The byte-exact stored key is pinned cross-core by bool_pk_table.jed
// (tests/fileformat_golden.test.ts); these are the behavioral checks. Mirrors
// impl/rust/tests/boolean_key.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database } from "../src/tooling.ts";
import { dbWith, errCode, query } from "./util.ts";

// A boolean PRIMARY KEY is accepted (the gate lifted) and CRUD works.
test("boolean primary key CRUD + point lookup", () => {
  const db = dbWith([
    "CREATE TABLE t (k boolean PRIMARY KEY, v i32)",
    "INSERT INTO t VALUES (FALSE, 10), (TRUE, 20)",
  ]);

  // Point lookup on the boolean PK resolves to the right row.
  assert.deepEqual(query(db, "SELECT v FROM t WHERE k = TRUE"), [["20"]]);
  assert.deepEqual(query(db, "SELECT v FROM t WHERE k = FALSE"), [["10"]]);

  // A full scan iterates in key (byte) order: false (0x00) before true (0x01).
  assert.deepEqual(query(db, "SELECT k FROM t"), [["false"], ["true"]]);
});

// A boolean member of a COMPOSITE primary key concatenates with the other component.
test("boolean composite primary key", () => {
  const db = dbWith([
    "CREATE TABLE t (a i32, b boolean, v i32, PRIMARY KEY (a, b))",
    "INSERT INTO t VALUES (1, TRUE, 10), (1, FALSE, 20), (2, FALSE, 30)",
  ]);
  // (1,FALSE) and (1,TRUE) are distinct keys; the same (a,b) again conflicts.
  assert.equal(
    errCode(() => db.execute("INSERT INTO t VALUES (1, TRUE, 99)")),
    "23505",
  );
  // Key order: a ascending, then b false<true within an a-group.
  assert.deepEqual(query(db, "SELECT a, b FROM t"), [
    ["1", "false"],
    ["1", "true"],
    ["2", "false"],
  ]);
});

// A secondary index on a (nullable) boolean column is accepted and serves equality.
test("boolean secondary index", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, flag boolean)",
    "INSERT INTO t VALUES (1, TRUE), (2, FALSE), (3, NULL), (4, TRUE)",
    "CREATE INDEX i ON t (flag)",
  ]);
  const ids = query(db, "SELECT id FROM t WHERE flag = TRUE")
    .map((r) => Number(r[0]))
    .sort((a, b) => a - b);
  assert.deepEqual(ids, [1, 4]);
});
