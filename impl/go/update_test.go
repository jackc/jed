package jed

// Step 6: UPDATE — value replacement, old-row assignment semantics, the two-phase
// all-or-nothing guarantee, the rejected cases (duplicate target, overflow), and PRIMARY
// KEY re-keying (§11 step 6). The PG-divergent re-keying cases (an end-state-valid key swap
// / cascade that PG rejects on the per-row transient) live here rather than the oracle
// corpus, the same divergence UNIQUE carries (indexes.md §8).

import (
	"fmt"
	"testing"
)

func setupUpdate(t *testing.T) *engine {
	return dbWith(
		t,
		"CREATE TABLE t (id i32 PRIMARY KEY, a i16, b i16)",
		"INSERT INTO t VALUES (1, 10, 11)",
		"INSERT INTO t VALUES (2, 20, 22)",
		"INSERT INTO t VALUES (3, 30, 33)",
	)
}

func TestUpdateMissingTable(t *testing.T) {
	wantErr(t, newEngine(), "UPDATE nope SET a = 1", "42P01")
}

func TestUpdateUnknownColumn(t *testing.T) {
	wantErr(t, setupUpdate(t), "UPDATE t SET nope = 1", "42703")
}

// idsABC is the (id, a, b) rows of t in storage-key order, "id/a/b" strings, for end-state
// assertions.
func idsABC(db *engine) []string {
	rows := db.RowsInKeyOrder("t")
	out := make([]string, len(rows))
	for i, r := range rows {
		out[i] = fmt.Sprintf("%d/%d/%d", r[0].Int, r[1].Int, r[2].Int)
	}
	return out
}

// Re-keying validates against the statement's END STATE (like UNIQUE, indexes.md §8): a
// swap of two primary keys keeps both keys present, so jed accepts it — where PostgreSQL's
// per-row check fails on the transient collision. Each row's non-key columns move with it.
func TestUpdatePkSwapIsEndStateValid(t *testing.T) {
	db := setupUpdate(t)
	if _, err := execute(db, "UPDATE t SET id = 3 - id WHERE id <= 2"); err != nil {
		t.Fatalf("swap: %v", err)
	}
	got := idsABC(db)
	want := []string{"1/20/22", "2/10/11", "3/30/33"}
	if fmt.Sprint(got) != fmt.Sprint(want) {
		t.Fatalf("after swap: got %v, want %v", got, want)
	}
}

// A cascade that shifts every key up by one is likewise end-state-valid, so jed re-keys all
// three rows — where PostgreSQL rejects the per-row transient (id 1 → 2 while 2 still exists).
func TestUpdatePkIncrementCascadeSucceeds(t *testing.T) {
	db := setupUpdate(t)
	if _, err := execute(db, "UPDATE t SET id = id + 1"); err != nil {
		t.Fatalf("cascade: %v", err)
	}
	got := idsABC(db)
	want := []string{"2/10/11", "3/20/22", "4/30/33"}
	if fmt.Sprint(got) != fmt.Sprint(want) {
		t.Fatalf("after cascade: got %v, want %v", got, want)
	}
}

// Re-keying onto a DISTINCT existing (non-updated) row's key collides — 23505, all-or-nothing.
func TestUpdatePkCollisionWithExisting(t *testing.T) {
	db := setupUpdate(t)
	wantErr(t, db, "UPDATE t SET id = 3 WHERE id = 1", "23505")
	got := idsABC(db)
	want := []string{"1/10/11", "2/20/22", "3/30/33"}
	if fmt.Sprint(got) != fmt.Sprint(want) {
		t.Fatalf("after failed collision: got %v, want %v", got, want)
	}
}
