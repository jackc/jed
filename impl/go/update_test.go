package jed

// Step 6: UPDATE — in-place value replacement, old-row assignment semantics, the
// two-phase all-or-nothing guarantee, and the rejected cases (PK column, duplicate
// target, overflow).

import "testing"

func setupUpdate(t *testing.T) *Database {
	return dbWith(
		t,
		"CREATE TABLE t (id int32 PRIMARY KEY, a int16, b int16)",
		"INSERT INTO t VALUES (1, 10, 11)",
		"INSERT INTO t VALUES (2, 20, 22)",
		"INSERT INTO t VALUES (3, 30, 33)",
	)
}

func TestUpdateMissingTable(t *testing.T) {
	wantErr(t, NewDatabase(), "UPDATE nope SET a = 1", "42P01")
}

func TestUpdateUnknownColumn(t *testing.T) {
	wantErr(t, setupUpdate(t), "UPDATE t SET nope = 1", "42703")
}
