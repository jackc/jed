package abide

// Step 6: DELETE — predicate-matched removal, no-WHERE clears, three-valued logic,
// and the no-PK monotonic-rowid regression (DELETE then INSERT must not collide).

import "testing"

func setupDelete(t *testing.T) *Database {
	return dbWith(
		t,
		"CREATE TABLE t (id int32 PRIMARY KEY, v int16)",
		"INSERT INTO t VALUES (1, 10)",
		"INSERT INTO t VALUES (2, 20)",
		"INSERT INTO t VALUES (3, 30)",
		"INSERT INTO t VALUES (4, NULL)",
	)
}

func TestDeleteByPredicate(t *testing.T) {
	db := setupDelete(t)
	mustCreate(t, db, "DELETE FROM t WHERE id = 2")
	if got := queryIDs(t, db, "SELECT id FROM t ORDER BY id"); !eqInts(got, 1, 3, 4) {
		t.Errorf("got %v want [1 3 4]", got)
	}
}

func TestDeleteNoWhereClearsAll(t *testing.T) {
	db := setupDelete(t)
	mustCreate(t, db, "DELETE FROM t")
	if got := query(t, db, "SELECT id FROM t ORDER BY id"); len(got) != 0 {
		t.Errorf("expected empty table, got %v", got)
	}
}

func TestDeleteThreeValuedOnlyTrueMatches(t *testing.T) {
	db := setupDelete(t)
	// FALSE for present rows, UNKNOWN for the NULL row — nothing deleted.
	mustCreate(t, db, "DELETE FROM t WHERE v > 100")
	if got := query(t, db, "SELECT id FROM t"); len(got) != 4 {
		t.Errorf("nothing should be deleted, got %d rows", len(got))
	}
	mustCreate(t, db, "DELETE FROM t WHERE v IS NULL")
	if got := queryIDs(t, db, "SELECT id FROM t ORDER BY id"); !eqInts(got, 1, 2, 3) {
		t.Errorf("got %v want [1 2 3]", got)
	}
}

func TestDeleteThenInsertNoPKNoCollision(t *testing.T) {
	// The bug this fixes: a no-PK table keyed rows on store.Len(), so after a delete
	// the next insert reused a rowid and tripped a spurious 23505.
	db := dbWith(
		t,
		"CREATE TABLE log (n int32)",
		"INSERT INTO log VALUES (100)",
		"INSERT INTO log VALUES (200)",
		"INSERT INTO log VALUES (300)",
		"DELETE FROM log WHERE n = 200",
	)
	if _, err := Execute(db, "INSERT INTO log VALUES (400)"); err != nil {
		t.Fatalf("insert after delete should not collide: %v", err)
	}
	if got := queryIDs(t, db, "SELECT n FROM log ORDER BY n"); !eqInts(got, 100, 300, 400) {
		t.Errorf("got %v want [100 300 400]", got)
	}
}

func TestDeleteMissingTable(t *testing.T) {
	wantErr(t, NewDatabase(), "DELETE FROM nope", "42P01")
}

func TestDeleteUnknownColumn(t *testing.T) {
	wantErr(t, setupDelete(t), "DELETE FROM t WHERE nope = 1", "42703")
}
