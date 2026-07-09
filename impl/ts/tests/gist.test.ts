// GiST indexes (spec/design/gist.md) — GX1: CREATE INDEX … USING gist over a range column, its
// maintenance, the planner &&/@> gather (descending the resident R-tree), and whole-image
// persistence (the page-5/6 R-tree, format_version 20, the toImage→loadEngine round-trip). Covers
// what the corpus cannot: the deliberate divergences (UNIQUE/multi-column/temp → 0A000), the
// unknown-method / non-range 42704s, and the on-disk round-trip. Lockstep peer of the Rust/Go tests.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, type Session } from "../src/tooling.ts";
import { dbWith, errCode, query } from "./util.ts";

function rangesDb(): Session {
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
  db.execute("DELETE FROM t WHERE id = 3");
  assert.deepEqual(query(db, "SELECT id FROM t WHERE r && i32range(4,6) ORDER BY id"), [["1"]]);
  db.execute("INSERT INTO t VALUES (7, '[5,12)')");
  assert.deepEqual(query(db, "SELECT id FROM t WHERE r && i32range(6,7) ORDER BY id"), [["7"]]);
});

test("gist divergences (42704 / 0A000)", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, r i32range, s i32range, f f64, txt text)",
  ]);
  // A GiST index on a non-keyable, non-range type (float) → 42704 (no GiST opclass at all, §6).
  assert.equal(
    errCode(() => db.execute("CREATE INDEX ON t USING gist (f)")),
    "42704",
  );
  // A keyable-but-deferred scalar (text) → 0A000 (on the roadmap, the GIN element-staging precedent).
  assert.equal(
    errCode(() => db.execute("CREATE INDEX ON t USING gist (txt)")),
    "0A000",
  );
  // An unknown access method → 42704.
  assert.equal(
    errCode(() => db.execute("CREATE INDEX ON t USING brin (r)")),
    "42704",
  );
  // UNIQUE and multi-column GiST → 0A000.
  assert.equal(
    errCode(() => db.execute("CREATE UNIQUE INDEX ON t USING gist (r)")),
    "0A000",
  );
  assert.equal(
    errCode(() => db.execute("CREATE INDEX ON t USING gist (r, s)")),
    "0A000",
  );
  // A GiST index on a TEMP table → 0A000 (resident tree on the temp snapshot is deferred).
  db.execute("CREATE TEMP TABLE tmp (id i32 PRIMARY KEY, r i32range)");
  assert.equal(
    errCode(() => db.execute("CREATE INDEX ON tmp USING gist (r)")),
    "0A000",
  );
});

test("gist whole-image roundtrip persists the R-tree", () => {
  // Build at page 256 (the round-trip target) so the in-memory tree splits for that page size —
  // a PAX leaf's directory overhead (format.md v23) makes an 8-record leaf packed for 8192 overflow
  // a 256-byte page. Matches how the Rust/Go gist round-trip tests create the DB (page_size: 256).
  const db = dbWith(
    [
      "CREATE TABLE t (id i32 PRIMARY KEY, r i32range)",
      "CREATE INDEX t_r_gist ON t USING gist (r)",
      "INSERT INTO t VALUES (1, '[1,5)'), (2, '[10,20)'), (3, '[3,8)'), (4, '[100,200)'), (5, '[50,60)'), (6, '[15,25)'), (7, 'empty'), (8, NULL)",
    ],
    256,
  );
  const loaded = Database.fromImage(db.toImage(256, 1n));
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
  loaded.execute("INSERT INTO t VALUES (9, '[5,7)')");
  const loaded2 = Database.fromImage(loaded.toImage(256, 1n));
  assert.deepEqual(query(loaded2, "SELECT id FROM t WHERE r && i32range(6,7) ORDER BY id"), [
    ["3"],
    ["9"],
  ]);
});

// GX2: the scalar `=` opclass (the in-core btree_gist). A GiST index over a fixed-width keyable scalar
// accelerates `=` — the planner descends the resident R-tree and re-applies `=` as the residual,
// identical rows to a full scan (duplicates and all) across INSERT/UPDATE/DELETE.
test("scalar gist `=` gather and maintenance", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, room i32)",
    "CREATE INDEX t_room_gist ON t USING gist (room)",
    "INSERT INTO t VALUES (1, 10), (2, 20), (3, 10), (4, 30), (5, 20), (6, 10), (7, NULL)",
  ]);
  assert.deepEqual(query(db, "SELECT id FROM t WHERE room = 10 ORDER BY id"), [
    ["1"],
    ["3"],
    ["6"],
  ]);
  assert.deepEqual(query(db, "SELECT id FROM t WHERE room = 20 ORDER BY id"), [["2"], ["5"]]);
  // `= NULL` is 3VL-unknown → no rows; a value with no row → no rows.
  assert.deepEqual(query(db, "SELECT id FROM t WHERE room = NULL ORDER BY id"), []);
  assert.deepEqual(query(db, "SELECT id FROM t WHERE room = 99 ORDER BY id"), []);
  // Maintenance: DELETE / INSERT / UPDATE the indexed column.
  db.execute("DELETE FROM t WHERE id = 3");
  assert.deepEqual(query(db, "SELECT id FROM t WHERE room = 10 ORDER BY id"), [["1"], ["6"]]);
  db.execute("INSERT INTO t VALUES (8, 10)");
  assert.deepEqual(query(db, "SELECT id FROM t WHERE room = 10 ORDER BY id"), [
    ["1"],
    ["6"],
    ["8"],
  ]);
  db.execute("UPDATE t SET room = 20 WHERE id = 1");
  assert.deepEqual(query(db, "SELECT id FROM t WHERE room = 20 ORDER BY id"), [
    ["1"],
    ["2"],
    ["5"],
  ]);
});

// GX2: a scalar `=` GiST index persists (page-5/6 R-tree, v20 — the bound is a [min,max] key blob,
// distinguished from a range bound by the column's catalog type) and reloads.
test("scalar gist whole-image roundtrip persists the R-tree", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, room i32)",
    "CREATE INDEX t_room_gist ON t USING gist (room)",
    "INSERT INTO t VALUES (1, 10), (2, 20), (3, 10), (4, 30), (5, 20), (6, 40), (7, 10), (8, 50)",
  ]);
  const loaded = Database.fromImage(db.toImage(256, 1n));
  assert.deepEqual(query(loaded, "SELECT id FROM t WHERE room = 10 ORDER BY id"), [
    ["1"],
    ["3"],
    ["7"],
  ]);
  assert.deepEqual(query(loaded, "SELECT id FROM t WHERE room = 20 ORDER BY id"), [["2"], ["5"]]);
  loaded.execute("INSERT INTO t VALUES (9, 20)");
  const loaded2 = Database.fromImage(loaded.toImage(256, 1n));
  assert.deepEqual(query(loaded2, "SELECT id FROM t WHERE room = 20 ORDER BY id"), [
    ["2"],
    ["5"],
    ["9"],
  ]);
});

// ---- GX3: EXCLUDE constraints (spec/design/gist.md §7) -----------------------------------------

function bookingDb(): Session {
  return dbWith([
    "CREATE TABLE booking (id i32 PRIMARY KEY, room i32, during i32range, " +
      "EXCLUDE USING gist (room WITH =, during WITH &&))",
  ]);
}

// The canonical no-double-booking constraint — no two rows may share a room AND have overlapping
// during. Needs the scalar `=` opclass (room) + range_ops (during).
test("exclude rejects conflict, admits compatible", () => {
  const db = bookingDb();
  db.execute("INSERT INTO booking VALUES (1, 101, '[10,20)')");
  assert.equal(
    errCode(() => db.execute("INSERT INTO booking VALUES (2, 101, '[15,25)')")),
    "23P01",
  );
  db.execute("INSERT INTO booking VALUES (2, 101, '[20,30)')"); // same room, no overlap → ok
  db.execute("INSERT INTO booking VALUES (3, 102, '[10,20)')"); // diff room, overlap → ok
  assert.deepEqual(query(db, "SELECT id FROM booking ORDER BY id"), [["1"], ["2"], ["3"]]);
});

// Updating a range column on an EXCLUDE-constrained table re-checks the constraint over the
// statement's end state (the GX3 + dml.update_container integration): a reschedule to a free slot
// succeeds; one that newly overlaps a same-room booking traps 23P01; moving to a different room
// clears the conflict. Needs the multi-column GiST index (PG needs btree_gist), so it lives here.
test("exclude reschedule via update", () => {
  const db = bookingDb();
  db.execute("INSERT INTO booking VALUES (1, 101, '[10,20)'), (2, 101, '[30,40)')");
  db.execute("UPDATE booking SET during = '[50,60)' WHERE id = 1");
  assert.equal(
    errCode(() => db.execute("UPDATE booking SET during = '[35,45)' WHERE id = 1")),
    "23P01",
  );
  db.execute("UPDATE booking SET room = 102, during = '[35,45)' WHERE id = 1");
  assert.deepEqual(query(db, "SELECT id FROM booking ORDER BY id"), [["1"], ["2"]]);
});

// A NULL excluded column (the NULL rule) or an empty range (empty && anything is FALSE) exempts a row.
test("exclude null and empty range are exempt", () => {
  const db = bookingDb();
  db.execute("INSERT INTO booking VALUES (1, 101, '[10,20)')");
  db.execute("INSERT INTO booking VALUES (2, NULL, '[10,20)')"); // NULL room → exempt
  db.execute("INSERT INTO booking VALUES (3, NULL, '[10,20)')");
  db.execute("INSERT INTO booking VALUES (4, 101, 'empty')"); // empty range → exempt
  db.execute("INSERT INTO booking VALUES (5, 101, 'empty')");
  assert.deepEqual(query(db, "SELECT id FROM booking ORDER BY id"), [
    ["1"],
    ["2"],
    ["3"],
    ["4"],
    ["5"],
  ]);
});

// Two rows in the SAME insert batch that conflict with each other → 23P01, nothing written.
test("exclude in-batch insert conflict", () => {
  const db = bookingDb();
  assert.equal(
    errCode(() =>
      db.execute("INSERT INTO booking VALUES (1, 101, '[10,20)'), (2, 101, '[15,25)')"),
    ),
    "23P01",
  );
  assert.deepEqual(query(db, "SELECT id FROM booking ORDER BY id"), []);
});

// A swap of rooms succeeds (the per-row transient collides but the END STATE is conflict-free); a
// genuine conflict traps 23P01.
test("exclude update end-state swap succeeds", () => {
  const db = bookingDb();
  db.execute("INSERT INTO booking VALUES (1, 101, '[10,20)')");
  db.execute("INSERT INTO booking VALUES (2, 102, '[10,20)')");
  db.execute("UPDATE booking SET room = CASE WHEN room = 101 THEN 102 ELSE 101 END");
  assert.deepEqual(query(db, "SELECT id FROM booking WHERE room = 102 ORDER BY id"), [["1"]]);
  // After the swap row1=(102,[10,20)), row2=(101,[10,20)); moving row1 back to 101 collides w/ row2.
  assert.equal(
    errCode(() => db.execute("UPDATE booking SET room = 101 WHERE id = 1")),
    "23P01",
  );
});

// A single-column range exclusion needs only GX1.
test("single-column range exclude", () => {
  const db = dbWith([
    "CREATE TABLE rsv (id i32 PRIMARY KEY, during i32range, EXCLUDE USING gist (during WITH &&))",
  ]);
  db.execute("INSERT INTO rsv VALUES (1, '[1,5)')");
  assert.equal(
    errCode(() => db.execute("INSERT INTO rsv VALUES (2, '[3,8)')")),
    "23P01",
  );
  db.execute("INSERT INTO rsv VALUES (2, '[5,10)')"); // adjacent, not overlapping → ok
  assert.deepEqual(query(db, "SELECT id FROM rsv ORDER BY id"), [["1"], ["2"]]);
});

// The WITH operator must pair with the column's GiST opclass.
test("exclude type errors", () => {
  const db = dbWith(["CREATE TABLE z (id i32 PRIMARY KEY)"]);
  assert.equal(
    errCode(() =>
      db.execute("CREATE TABLE a (id i32 PRIMARY KEY, n i32, EXCLUDE USING gist (n WITH &&))"),
    ),
    "42704",
  );
  assert.equal(
    errCode(() =>
      db.execute("CREATE TABLE b (id i32 PRIMARY KEY, s text, EXCLUDE USING gist (s WITH =))"),
    ),
    "0A000",
  );
  assert.equal(
    errCode(() =>
      db.execute("CREATE TABLE c (id i32 PRIMARY KEY, f f64, EXCLUDE USING gist (f WITH =))"),
    ),
    "42704",
  );
  assert.equal(
    errCode(() =>
      db.execute("CREATE TABLE d (id i32 PRIMARY KEY, n i32, EXCLUDE USING gist (n WITH <))"),
    ),
    "0A000",
  );
  assert.equal(
    errCode(() =>
      db.execute(
        "CREATE TEMP TABLE e (id i32 PRIMARY KEY, during i32range, EXCLUDE USING gist (during WITH &&))",
      ),
    ),
    "0A000",
  );
});

// The backing GiST index is owned by the constraint → 2BP01.
test("exclude backing index cannot be dropped", () => {
  const db = bookingDb();
  assert.equal(
    errCode(() => db.execute("DROP INDEX booking_room_during_excl")),
    "2BP01",
  );
});

// The backing multi-column GiST index persists (v21) and reloads, still enforcing the conjunction.
test("exclude whole-image roundtrip persists the constraint", () => {
  const db = dbWith([
    "CREATE TABLE booking (id i32 PRIMARY KEY, room i32, during i32range, " +
      "EXCLUDE USING gist (room WITH =, during WITH &&))",
    "INSERT INTO booking VALUES (1, 101, '[10,20)'), (2, 101, '[20,30)'), (3, 102, '[10,20)')",
  ]);
  const loaded = Database.fromImage(db.toImage(256, 1n));
  assert.equal(
    errCode(() => loaded.execute("INSERT INTO booking VALUES (4, 101, '[15,25)')")),
    "23P01",
  );
  loaded.execute("INSERT INTO booking VALUES (4, 103, '[10,20)')");
  assert.deepEqual(query(loaded, "SELECT id FROM booking ORDER BY id"), [
    ["1"],
    ["2"],
    ["3"],
    ["4"],
  ]);
});
