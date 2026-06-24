// GiST indexes (spec/design/gist.md) — GX1: CREATE INDEX … USING gist over a range column, its
// maintenance, the planner &&/@> gather (descending the resident R-tree), and whole-image
// persistence (the page-5/6 R-tree, format_version 20, the toImage→loadDatabase round-trip). Covers
// what the corpus cannot: the deliberate divergences (UNIQUE/multi-column/temp → 0A000), the
// unknown-method / non-range 42704s, and the on-disk round-trip. Lockstep peer of the Rust/Go tests.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, execute, loadDatabase, toImage } from "../src/lib.ts";
import { dbWith, errCode, query } from "./util.ts";

function rangesDb(): Database {
  return dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, r i32range)",
    "CREATE INDEX t_r_gist ON t USING gist (r)",
    "INSERT INTO t VALUES (1, '[1,5)'), (2, '[10,20)'), (3, '[3,8)'), (4, '[100,200)'), (5, 'empty'), (6, NULL)",
  ]);
}

test("gist create and query (overlap / contains / maintenance)", () => {
  const db = rangesDb();
  // && [4,6): [1,5) and [3,8) overlap; the rest / empty / NULL do not.
  assert.deepEqual(query(db, "SELECT id FROM t WHERE r && i32range(4,6) ORDER BY id"), [
    ["1"],
    ["3"],
  ]);
  // @> [4,5): [1,5) and [3,8) contain it.
  assert.deepEqual(query(db, "SELECT id FROM t WHERE r @> i32range(4,5) ORDER BY id"), [
    ["1"],
    ["3"],
  ]);
  // The high cluster.
  assert.deepEqual(query(db, "SELECT id FROM t WHERE r && i32range(150,160) ORDER BY id"), [["4"]]);
  // Maintenance: DELETE drops the entry, then a fresh INSERT adds one.
  execute(db, "DELETE FROM t WHERE id = 3");
  assert.deepEqual(query(db, "SELECT id FROM t WHERE r && i32range(4,6) ORDER BY id"), [["1"]]);
  execute(db, "INSERT INTO t VALUES (7, '[5,12)')");
  assert.deepEqual(query(db, "SELECT id FROM t WHERE r && i32range(6,7) ORDER BY id"), [["7"]]);
});

test("gist divergences (42704 / 0A000)", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, r i32range, s i32range, n i32)"]);
  // A GiST index on a non-range column → 42704.
  assert.equal(
    errCode(() => execute(db, "CREATE INDEX ON t USING gist (n)")),
    "42704",
  );
  // An unknown access method → 42704.
  assert.equal(
    errCode(() => execute(db, "CREATE INDEX ON t USING brin (r)")),
    "42704",
  );
  // UNIQUE and multi-column GiST → 0A000.
  assert.equal(
    errCode(() => execute(db, "CREATE UNIQUE INDEX ON t USING gist (r)")),
    "0A000",
  );
  assert.equal(
    errCode(() => execute(db, "CREATE INDEX ON t USING gist (r, s)")),
    "0A000",
  );
  // A GiST index on a TEMP table → 0A000 (resident tree on the temp snapshot is deferred).
  execute(db, "CREATE TEMP TABLE tmp (id i32 PRIMARY KEY, r i32range)");
  assert.equal(
    errCode(() => execute(db, "CREATE INDEX ON tmp USING gist (r)")),
    "0A000",
  );
});

test("gist whole-image roundtrip persists the R-tree", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, r i32range)",
    "CREATE INDEX t_r_gist ON t USING gist (r)",
    "INSERT INTO t VALUES (1, '[1,5)'), (2, '[10,20)'), (3, '[3,8)'), (4, '[100,200)'), (5, '[50,60)'), (6, '[15,25)'), (7, 'empty'), (8, NULL)",
  ]);
  const loaded = loadDatabase(toImage(db, 256, 1n));
  // The persisted R-tree loads, the resident tree is rebuilt, the gather still works.
  assert.deepEqual(query(loaded, "SELECT id FROM t WHERE r && i32range(4,6) ORDER BY id"), [
    ["1"],
    ["3"],
  ]);
  assert.deepEqual(query(loaded, "SELECT id FROM t WHERE r @> i32range(4,5) ORDER BY id"), [
    ["1"],
    ["3"],
  ]);
  // Maintenance after reload, then a second round-trip.
  execute(loaded, "INSERT INTO t VALUES (9, '[5,7)')");
  const loaded2 = loadDatabase(toImage(loaded, 256, 1n));
  assert.deepEqual(query(loaded2, "SELECT id FROM t WHERE r && i32range(6,7) ORDER BY id"), [
    ["3"],
    ["9"],
  ]);
});
