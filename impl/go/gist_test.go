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

func gistRangesDB(t *testing.T) *Database {
	t.Helper()
	db := NewDatabase()
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
	db := NewDatabase()
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, r i32range, s i32range, n i32)")
	// A GiST index on a non-range column → 42704 (no default operator class).
	if got := errCode(t, db, "CREATE INDEX ON t USING gist (n)"); got != "42704" {
		t.Errorf("gist on non-range: got %s, want 42704", got)
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

// TestGistFileRoundTrip: a GiST index persists to the page-5/6 R-tree (v20) and reloads correctly —
// the index survives a close/reopen and still accelerates &&/@> to the same rows.
func TestGistFileRoundTrip(t *testing.T) {
	path := filepath.Join(t.TempDir(), "gist_round_trip.jed")
	db, err := Create(path, DatabaseOptions{PageSize: 256})
	if err != nil {
		t.Fatal(err)
	}
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, r i32range)")
	run(t, db, "CREATE INDEX t_r_gist ON t USING gist (r)")
	run(t, db, "INSERT INTO t VALUES (1, '[1,5)'), (2, '[10,20)'), (3, '[3,8)'), (4, '[100,200)'), (5, '[50,60)'), (6, '[15,25)'), (7, 'empty'), (8, NULL)")
	if got := gistIDs(queryRows(t, db, "SELECT id FROM t WHERE r && i32range(4,6) ORDER BY id")); !eqInts(got, 1, 3) {
		t.Errorf("before close: got %v, want [1 3]", got)
	}

	db2, err := Open(path)
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
	db3, err := Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if got := gistIDs(queryRows(t, db3, "SELECT id FROM t WHERE r && i32range(6,7) ORDER BY id")); !eqInts(got, 3, 9) {
		t.Errorf("after maintenance reopen: got %v, want [3 9]", got)
	}
}
