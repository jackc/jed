package jed

// Step 6: DELETE — predicate-matched removal, no-WHERE clears, three-valued logic,
// and the no-PK monotonic-rowid regression (DELETE then INSERT must not collide).

import "testing"

func setupDelete(t *testing.T) *Session {
	return dbWith(
		t,
		"CREATE TABLE t (id i32 PRIMARY KEY, v i16)",
		"INSERT INTO t VALUES (1, 10)",
		"INSERT INTO t VALUES (2, 20)",
		"INSERT INTO t VALUES (3, 30)",
		"INSERT INTO t VALUES (4, NULL)",
	)
}

func TestDeleteMissingTable(t *testing.T) {
	wantErr(t, memDB().Session(SessionOptions{}), "DELETE FROM nope", "42P01")
}

func TestDeleteUnknownColumn(t *testing.T) {
	wantErr(t, setupDelete(t), "DELETE FROM t WHERE nope = 1", "42703")
}
