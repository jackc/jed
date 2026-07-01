package jed

// GiST indexes (spec/design/gist.md) — GX1: CREATE INDEX … USING gist over a range column, its
// maintenance, the planner &&/@> gather (descending the resident R-tree), and file persistence (the
// page-5/6 R-tree, format_version 20, the close/reopen round-trip). Covers what the corpus cannot:
// the deliberate divergences (UNIQUE/multi-column/temp → 0A000), the unknown-method / non-range
// 42704s, and the on-disk round-trip. The lockstep peer of impl/rust/tests/gist_index.rs.

import (
	"path/filepath"
	"testing"
)

func gistIDs(rows [][]Value) []int64 {
	out := make([]int64, len(rows))
	for i, r := range rows {
		out[i] = r[0].Int
	}
	return out
}

func gistRangesDB(t *testing.T) *Session {
	t.Helper()
	db := NewDatabase().Session(SessionOptions{})
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, r i32range)")
	run(t, db, "CREATE INDEX t_r_gist ON t USING gist (r)")
	run(t, db, "INSERT INTO t VALUES (1, '[1,5)'), (2, '[10,20)'), (3, '[3,8)'), (4, '[100,200)'), (5, 'empty'), (6, NULL)")
	return db
}

func TestGistCreateAndQuery(t *testing.T) {
	db := gistRangesDB(t)
	// && (overlap) [4,6): [1,5) and [3,8) overlap; the rest / empty / NULL do not.
	if got := gistIDs(queryRows(t, db, "SELECT id FROM t WHERE r && i32range(4,6) ORDER BY id")); !eqInts(got, 1, 3) {
		t.Errorf("overlap [4,6): got %v, want [1 3]", got)
	}
	// @> (contains) [4,5): [1,5) and [3,8) contain it.
	if got := gistIDs(queryRows(t, db, "SELECT id FROM t WHERE r @> i32range(4,5) ORDER BY id")); !eqInts(got, 1, 3) {
		t.Errorf("contains [4,5): got %v, want [1 3]", got)
	}
	// The high cluster.
	if got := gistIDs(queryRows(t, db, "SELECT id FROM t WHERE r && i32range(150,160) ORDER BY id")); !eqInts(got, 4) {
		t.Errorf("overlap [150,160): got %v, want [4]", got)
	}
	// Maintenance: DELETE drops the row's entry, then a fresh INSERT adds one.
	run(t, db, "DELETE FROM t WHERE id = 3")
	if got := gistIDs(queryRows(t, db, "SELECT id FROM t WHERE r && i32range(4,6) ORDER BY id")); !eqInts(got, 1) {
		t.Errorf("after delete: got %v, want [1]", got)
	}
	run(t, db, "INSERT INTO t VALUES (7, '[5,12)')")
	if got := gistIDs(queryRows(t, db, "SELECT id FROM t WHERE r && i32range(6,7) ORDER BY id")); !eqInts(got, 7) {
		t.Errorf("after insert: got %v, want [7]", got)
	}
}

func TestGistDivergences(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, r i32range, s i32range, f f64, txt text)")
	// A GiST index on a non-keyable, non-range type (float) → 42704 (no GiST opclass at all, §6).
	if got := errCode(t, db, "CREATE INDEX ON t USING gist (f)"); got != "42704" {
		t.Errorf("gist on float: got %s, want 42704", got)
	}
	// A keyable-but-deferred scalar (text) → 0A000 (on the roadmap, the GIN element-staging precedent).
	if got := errCode(t, db, "CREATE INDEX ON t USING gist (txt)"); got != "0A000" {
		t.Errorf("gist on text: got %s, want 0A000", got)
	}
	// An unknown access method → 42704.
	if got := errCode(t, db, "CREATE INDEX ON t USING brin (r)"); got != "42704" {
		t.Errorf("unknown method: got %s, want 42704", got)
	}
	// UNIQUE and multi-column GiST → 0A000.
	if got := errCode(t, db, "CREATE UNIQUE INDEX ON t USING gist (r)"); got != "0A000" {
		t.Errorf("unique gist: got %s, want 0A000", got)
	}
	if got := errCode(t, db, "CREATE INDEX ON t USING gist (r, s)"); got != "0A000" {
		t.Errorf("multi-column gist: got %s, want 0A000", got)
	}
	// A GiST index on a TEMP table → 0A000 (resident tree on the temp snapshot is deferred).
	run(t, db, "CREATE TEMP TABLE tmp (id i32 PRIMARY KEY, r i32range)")
	if got := errCode(t, db, "CREATE INDEX ON tmp USING gist (r)"); got != "0A000" {
		t.Errorf("gist on temp: got %s, want 0A000", got)
	}
}

// TestScalarGistEqualGather: the scalar `=` opclass (GX2, the in-core btree_gist). A GiST index over a
// fixed-width keyable scalar accelerates `=` — the planner descends the resident R-tree and re-applies
// `=` as the residual, identical rows to a full scan (duplicates and all) across INSERT/UPDATE/DELETE.
func TestScalarGistEqualGather(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, room i32)")
	run(t, db, "CREATE INDEX t_room_gist ON t USING gist (room)")
	run(t, db, "INSERT INTO t VALUES (1, 10), (2, 20), (3, 10), (4, 30), (5, 20), (6, 10), (7, NULL)")
	if got := gistIDs(queryRows(t, db, "SELECT id FROM t WHERE room = 10 ORDER BY id")); !eqInts(got, 1, 3, 6) {
		t.Errorf("room = 10: got %v, want [1 3 6]", got)
	}
	if got := gistIDs(queryRows(t, db, "SELECT id FROM t WHERE room = 20 ORDER BY id")); !eqInts(got, 2, 5) {
		t.Errorf("room = 20: got %v, want [2 5]", got)
	}
	// `= NULL` is 3VL-unknown → no rows; a value with no row → no rows.
	if got := gistIDs(queryRows(t, db, "SELECT id FROM t WHERE room = NULL ORDER BY id")); len(got) != 0 {
		t.Errorf("room = NULL: got %v, want []", got)
	}
	if got := gistIDs(queryRows(t, db, "SELECT id FROM t WHERE room = 99 ORDER BY id")); len(got) != 0 {
		t.Errorf("room = 99: got %v, want []", got)
	}
	// Maintenance: DELETE / INSERT / UPDATE the indexed column.
	run(t, db, "DELETE FROM t WHERE id = 3")
	if got := gistIDs(queryRows(t, db, "SELECT id FROM t WHERE room = 10 ORDER BY id")); !eqInts(got, 1, 6) {
		t.Errorf("after delete room = 10: got %v, want [1 6]", got)
	}
	run(t, db, "INSERT INTO t VALUES (8, 10)")
	if got := gistIDs(queryRows(t, db, "SELECT id FROM t WHERE room = 10 ORDER BY id")); !eqInts(got, 1, 6, 8) {
		t.Errorf("after insert room = 10: got %v, want [1 6 8]", got)
	}
	run(t, db, "UPDATE t SET room = 20 WHERE id = 1")
	if got := gistIDs(queryRows(t, db, "SELECT id FROM t WHERE room = 20 ORDER BY id")); !eqInts(got, 1, 2, 5) {
		t.Errorf("after update room = 20: got %v, want [1 2 5]", got)
	}
}

// TestScalarGistFileRoundTrip: a scalar `=` GiST index persists (page-5/6 R-tree, v20 — the bound is a
// [min,max] key blob, distinguished from a range bound by the column's catalog type) and reloads.
func TestScalarGistFileRoundTrip(t *testing.T) {
	path := filepath.Join(t.TempDir(), "gist_scalar_round_trip.jed")
	db, err := create(path, DatabaseOptions{PageSize: 256})
	if err != nil {
		t.Fatal(err)
	}
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, room i32)")
	run(t, db, "CREATE INDEX t_room_gist ON t USING gist (room)")
	run(t, db, "INSERT INTO t VALUES (1, 10), (2, 20), (3, 10), (4, 30), (5, 20), (6, 40), (7, 10), (8, 50)")
	if got := gistIDs(queryRows(t, db, "SELECT id FROM t WHERE room = 10 ORDER BY id")); !eqInts(got, 1, 3, 7) {
		t.Errorf("before close room = 10: got %v, want [1 3 7]", got)
	}

	db2, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := gistIDs(queryRows(t, db2, "SELECT id FROM t WHERE room = 20 ORDER BY id")); !eqInts(got, 2, 5) {
		t.Errorf("after reopen room = 20: got %v, want [2 5]", got)
	}
	run(t, db2, "INSERT INTO t VALUES (9, 20)")
	db3, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := gistIDs(queryRows(t, db3, "SELECT id FROM t WHERE room = 20 ORDER BY id")); !eqInts(got, 2, 5, 9) {
		t.Errorf("after maintenance reopen room = 20: got %v, want [2 5 9]", got)
	}
}

// TestGistFileRoundTrip: a GiST index persists to the page-5/6 R-tree (v20) and reloads correctly —
// the index survives a close/reopen and still accelerates &&/@> to the same rows.
func TestGistFileRoundTrip(t *testing.T) {
	path := filepath.Join(t.TempDir(), "gist_round_trip.jed")
	db, err := create(path, DatabaseOptions{PageSize: 256})
	if err != nil {
		t.Fatal(err)
	}
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, r i32range)")
	run(t, db, "CREATE INDEX t_r_gist ON t USING gist (r)")
	run(t, db, "INSERT INTO t VALUES (1, '[1,5)'), (2, '[10,20)'), (3, '[3,8)'), (4, '[100,200)'), (5, '[50,60)'), (6, '[15,25)'), (7, 'empty'), (8, NULL)")
	if got := gistIDs(queryRows(t, db, "SELECT id FROM t WHERE r && i32range(4,6) ORDER BY id")); !eqInts(got, 1, 3) {
		t.Errorf("before close: got %v, want [1 3]", got)
	}

	db2, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := gistIDs(queryRows(t, db2, "SELECT id FROM t WHERE r && i32range(4,6) ORDER BY id")); !eqInts(got, 1, 3) {
		t.Errorf("after reopen &&: got %v, want [1 3]", got)
	}
	if got := gistIDs(queryRows(t, db2, "SELECT id FROM t WHERE r @> i32range(4,5) ORDER BY id")); !eqInts(got, 1, 3) {
		t.Errorf("after reopen @>: got %v, want [1 3]", got)
	}
	// Maintenance after reopen persists through a second reopen.
	run(t, db2, "INSERT INTO t VALUES (9, '[5,7)')")
	db3, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := gistIDs(queryRows(t, db3, "SELECT id FROM t WHERE r && i32range(6,7) ORDER BY id")); !eqInts(got, 3, 9) {
		t.Errorf("after maintenance reopen: got %v, want [3 9]", got)
	}
}

// ---- GX3: EXCLUDE constraints (spec/design/gist.md §7) -----------------------------------------

func bookingDB(t *testing.T) *Session {
	t.Helper()
	db := NewDatabase().Session(SessionOptions{})
	run(t, db, "CREATE TABLE booking (id i32 PRIMARY KEY, room i32, during i32range, "+
		"EXCLUDE USING gist (room WITH =, during WITH &&))")
	return db
}

// TestExcludeRejectsConflict: the canonical no-double-booking constraint — no two rows may share a
// room AND have overlapping during. Needs the scalar `=` opclass (room) + range_ops (during).
func TestExcludeRejectsConflict(t *testing.T) {
	db := bookingDB(t)
	run(t, db, "INSERT INTO booking VALUES (1, 101, '[10,20)')")
	if got := errCode(t, db, "INSERT INTO booking VALUES (2, 101, '[15,25)')"); got != "23P01" {
		t.Errorf("same room + overlap: got %s, want 23P01", got)
	}
	run(t, db, "INSERT INTO booking VALUES (2, 101, '[20,30)')") // same room, no overlap → ok
	run(t, db, "INSERT INTO booking VALUES (3, 102, '[10,20)')") // diff room, overlap → ok
	if got := gistIDs(queryRows(t, db, "SELECT id FROM booking ORDER BY id")); !eqInts(got, 1, 2, 3) {
		t.Errorf("end state: got %v, want [1 2 3]", got)
	}
}

// TestExcludeNullAndEmptyExempt: a NULL excluded column (the NULL rule) or an empty range (empty &&
// anything is FALSE) makes a row exempt — it never conflicts.
func TestExcludeNullAndEmptyExempt(t *testing.T) {
	db := bookingDB(t)
	run(t, db, "INSERT INTO booking VALUES (1, 101, '[10,20)')")
	run(t, db, "INSERT INTO booking VALUES (2, NULL, '[10,20)')") // NULL room → exempt
	run(t, db, "INSERT INTO booking VALUES (3, NULL, '[10,20)')")
	run(t, db, "INSERT INTO booking VALUES (4, 101, 'empty')") // empty range → exempt
	run(t, db, "INSERT INTO booking VALUES (5, 101, 'empty')")
	if got := gistIDs(queryRows(t, db, "SELECT id FROM booking ORDER BY id")); !eqInts(got, 1, 2, 3, 4, 5) {
		t.Errorf("exempt rows: got %v, want [1 2 3 4 5]", got)
	}
}

// TestExcludeInBatchConflict: two rows in the SAME insert batch that conflict with each other → 23P01.
func TestExcludeInBatchConflict(t *testing.T) {
	db := bookingDB(t)
	if got := errCode(t, db, "INSERT INTO booking VALUES (1, 101, '[10,20)'), (2, 101, '[15,25)')"); got != "23P01" {
		t.Errorf("in-batch conflict: got %s, want 23P01", got)
	}
	if got := gistIDs(queryRows(t, db, "SELECT id FROM booking ORDER BY id")); len(got) != 0 {
		t.Errorf("nothing written: got %v, want []", got)
	}
}

// TestExcludeUpdateEndStateSwap: a swap of rooms succeeds (the per-row transient collides but the END
// STATE is conflict-free); an UPDATE that creates a genuine conflict traps 23P01.
func TestExcludeUpdateEndStateSwap(t *testing.T) {
	db := bookingDB(t)
	run(t, db, "INSERT INTO booking VALUES (1, 101, '[10,20)')")
	run(t, db, "INSERT INTO booking VALUES (2, 102, '[10,20)')")
	run(t, db, "UPDATE booking SET room = CASE WHEN room = 101 THEN 102 ELSE 101 END")
	if got := gistIDs(queryRows(t, db, "SELECT id FROM booking WHERE room = 102 ORDER BY id")); !eqInts(got, 1) {
		t.Errorf("after swap room=102: got %v, want [1]", got)
	}
	// After the swap row1=(102,[10,20)), row2=(101,[10,20)); moving row1 back to 101 collides w/ row2.
	if got := errCode(t, db, "UPDATE booking SET room = 101 WHERE id = 1"); got != "23P01" {
		t.Errorf("conflicting update: got %s, want 23P01", got)
	}
}

// TestSingleColumnRangeExclude: a single-column range exclusion needs only GX1.
func TestSingleColumnRangeExclude(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	run(t, db, "CREATE TABLE rsv (id i32 PRIMARY KEY, during i32range, EXCLUDE USING gist (during WITH &&))")
	run(t, db, "INSERT INTO rsv VALUES (1, '[1,5)')")
	if got := errCode(t, db, "INSERT INTO rsv VALUES (2, '[3,8)')"); got != "23P01" {
		t.Errorf("overlap: got %s, want 23P01", got)
	}
	run(t, db, "INSERT INTO rsv VALUES (2, '[5,10)')") // adjacent, not overlapping → ok
	if got := gistIDs(queryRows(t, db, "SELECT id FROM rsv ORDER BY id")); !eqInts(got, 1, 2) {
		t.Errorf("end state: got %v, want [1 2]", got)
	}
}

// TestExcludeRescheduleViaUpdate: updating a range column on an EXCLUDE-constrained table re-checks
// the constraint over the statement's end state (the GX3 + dml.update_container integration). A
// reschedule to a free slot succeeds; one that newly overlaps a same-room booking traps 23P01;
// moving to a different room clears the conflict. Needs the multi-column GiST index, so it lives here
// rather than the oracle corpus (PG needs btree_gist for the scalar `=` member).
func TestExcludeRescheduleViaUpdate(t *testing.T) {
	db := bookingDB(t)
	run(t, db, "INSERT INTO booking VALUES (1, 101, '[10,20)'), (2, 101, '[30,40)')")
	// reschedule booking 1 to a free slot → ok
	run(t, db, "UPDATE booking SET during = '[50,60)' WHERE id = 1")
	// reschedule booking 1 to overlap booking 2 (same room) → 23P01
	if got := errCode(t, db, "UPDATE booking SET during = '[35,45)' WHERE id = 1"); got != "23P01" {
		t.Errorf("conflicting reschedule: got %s, want 23P01", got)
	}
	// moving to a different room AND the overlapping slot is fine
	run(t, db, "UPDATE booking SET room = 102, during = '[35,45)' WHERE id = 1")
	if got := gistIDs(queryRows(t, db, "SELECT id FROM booking ORDER BY id")); !eqInts(got, 1, 2) {
		t.Errorf("end state: got %v, want [1 2]", got)
	}
}

// TestExcludeTypeErrors: the WITH operator must pair with the column's GiST opclass.
func TestExcludeTypeErrors(t *testing.T) {
	db := NewDatabase().Session(SessionOptions{})
	if got := errCode(t, db, "CREATE TABLE a (id i32 PRIMARY KEY, n i32, EXCLUDE USING gist (n WITH &&))"); got != "42704" {
		t.Errorf("&& on non-range: got %s, want 42704", got)
	}
	if got := errCode(t, db, "CREATE TABLE b (id i32 PRIMARY KEY, s text, EXCLUDE USING gist (s WITH =))"); got != "0A000" {
		t.Errorf("= on deferred text: got %s, want 0A000", got)
	}
	if got := errCode(t, db, "CREATE TABLE c (id i32 PRIMARY KEY, f f64, EXCLUDE USING gist (f WITH =))"); got != "42704" {
		t.Errorf("= on no-opclass f64: got %s, want 42704", got)
	}
	if got := errCode(t, db, "CREATE TABLE d (id i32 PRIMARY KEY, n i32, EXCLUDE USING gist (n WITH <))"); got != "0A000" {
		t.Errorf("unsupported operator: got %s, want 0A000", got)
	}
	if got := errCode(t, db, "CREATE TEMP TABLE e (id i32 PRIMARY KEY, during i32range, EXCLUDE USING gist (during WITH &&))"); got != "0A000" {
		t.Errorf("exclude on temp: got %s, want 0A000", got)
	}
}

// TestExcludeBackingIndexCannotBeDropped: the backing GiST index is owned by the constraint → 2BP01.
func TestExcludeBackingIndexCannotBeDropped(t *testing.T) {
	db := bookingDB(t)
	if got := errCode(t, db, "DROP INDEX booking_room_during_excl"); got != "2BP01" {
		t.Errorf("drop backing index: got %s, want 2BP01", got)
	}
}

// TestExcludeFileRoundTrip: the backing multi-column GiST index persists (v21) and reloads, still
// enforcing the conjunction across a close/reopen.
func TestExcludeFileRoundTrip(t *testing.T) {
	path := filepath.Join(t.TempDir(), "gist_exclude_round_trip.jed")
	db, err := create(path, DatabaseOptions{PageSize: 256})
	if err != nil {
		t.Fatal(err)
	}
	run(t, db, "CREATE TABLE booking (id i32 PRIMARY KEY, room i32, during i32range, "+
		"EXCLUDE USING gist (room WITH =, during WITH &&))")
	run(t, db, "INSERT INTO booking VALUES (1, 101, '[10,20)'), (2, 101, '[20,30)'), (3, 102, '[10,20)')")

	db2, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := errCode(t, db2, "INSERT INTO booking VALUES (4, 101, '[15,25)')"); got != "23P01" {
		t.Errorf("after reopen, conflict: got %s, want 23P01", got)
	}
	run(t, db2, "INSERT INTO booking VALUES (4, 103, '[10,20)')")
	if got := gistIDs(queryRows(t, db2, "SELECT id FROM booking ORDER BY id")); !eqInts(got, 1, 2, 3, 4) {
		t.Errorf("after reopen end state: got %v, want [1 2 3 4]", got)
	}
}
