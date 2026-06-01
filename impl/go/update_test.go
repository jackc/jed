package abide

// Step 6: UPDATE — in-place value replacement, old-row assignment semantics, the
// two-phase all-or-nothing guarantee, and the rejected cases (PK column, duplicate
// target, overflow).

import "testing"

func setupUpdate(t *testing.T) *Database {
	return dbWith(t,
		"CREATE TABLE t (id int32 PRIMARY KEY, a int16, b int16)",
		"INSERT INTO t VALUES (1, 10, 11)",
		"INSERT INTO t VALUES (2, 20, 22)",
		"INSERT INTO t VALUES (3, 30, 33)",
	)
}

func TestUpdateOneRowByKey(t *testing.T) {
	db := setupUpdate(t)
	mustCreate(t, db, "UPDATE t SET a = 99 WHERE id = 2")
	if got := query(t, db, "SELECT a FROM t WHERE id = 2"); got[0][0].Int != 99 {
		t.Errorf("id 2 a = %v, want 99", got)
	}
	if got := query(t, db, "SELECT a FROM t WHERE id = 1"); got[0][0].Int != 10 {
		t.Errorf("id 1 untouched expected 10, got %v", got)
	}
}

func TestUpdateSwapReadsOldRow(t *testing.T) {
	db := setupUpdate(t)
	mustCreate(t, db, "UPDATE t SET a = b, b = a WHERE id = 1")
	got := query(t, db, "SELECT a, b FROM t WHERE id = 1")
	if got[0][0].Int != 11 || got[0][1].Int != 10 {
		t.Errorf("swap got %v, want [11 10]", got)
	}
}

func TestUpdateNoWhereTouchesEveryRow(t *testing.T) {
	db := setupUpdate(t)
	mustCreate(t, db, "UPDATE t SET b = 0")
	for _, r := range query(t, db, "SELECT b FROM t ORDER BY id") {
		if r[0].Int != 0 {
			t.Errorf("expected all b = 0, got %v", r)
		}
	}
}

func TestUpdateToNullInNullableColumn(t *testing.T) {
	db := setupUpdate(t)
	mustCreate(t, db, "UPDATE t SET a = NULL WHERE id = 3")
	if got := query(t, db, "SELECT a FROM t WHERE id = 3"); !got[0][0].Null {
		t.Errorf("expected NULL, got %v", got)
	}
}

func TestUpdatePrimaryKeyColumnUnsupported(t *testing.T) {
	db := setupUpdate(t)
	wantErr(t, db, "UPDATE t SET id = 5 WHERE id = 2", "0A000")
	if got := query(t, db, "SELECT id FROM t WHERE id = 2"); got[0][0].Int != 2 {
		t.Errorf("id 2 should be unchanged, got %v", got)
	}
}

func TestUpdateDuplicateTargetColumn(t *testing.T) {
	wantErr(t, setupUpdate(t), "UPDATE t SET a = 1, a = 2 WHERE id = 1", "42701")
}

func TestUpdateOverflowTrapsRowUnchanged(t *testing.T) {
	db := setupUpdate(t)
	wantErr(t, db, "UPDATE t SET a = 40000 WHERE id = 2", "22003")
	if got := query(t, db, "SELECT a FROM t WHERE id = 2"); got[0][0].Int != 20 {
		t.Errorf("id 2 a should still be 20, got %v", got)
	}
}

func TestUpdateColumnSourceRechecksTargetRange(t *testing.T) {
	db := dbWith(t,
		"CREATE TABLE w (id int32 PRIMARY KEY, small int16, big int64)",
		"INSERT INTO w VALUES (1, 5, 100000)",
	)
	wantErr(t, db, "UPDATE w SET small = big WHERE id = 1", "22003")
	if got := query(t, db, "SELECT small FROM w WHERE id = 1"); got[0][0].Int != 5 {
		t.Errorf("small should still be 5, got %v", got)
	}
}

func TestUpdateAllOrNothingAcrossRows(t *testing.T) {
	// Row 2's source overflows int16, so NO row is modified — not even rows 1 and 3.
	db := dbWith(t,
		"CREATE TABLE m (id int32 PRIMARY KEY, n int16, src int64)",
		"INSERT INTO m VALUES (1, 1, 5)",
		"INSERT INTO m VALUES (2, 2, 99999)",
		"INSERT INTO m VALUES (3, 3, 7)",
	)
	wantErr(t, db, "UPDATE m SET n = src", "22003")
	if got := queryIDs(t, db, "SELECT n FROM m ORDER BY id"); !eqInts(got, 1, 2, 3) {
		t.Errorf("no row should change, got %v want [1 2 3]", got)
	}
}

func TestUpdateMissingTable(t *testing.T) {
	wantErr(t, NewDatabase(), "UPDATE nope SET a = 1", "42P01")
}

func TestUpdateUnknownColumn(t *testing.T) {
	wantErr(t, setupUpdate(t), "UPDATE t SET nope = 1", "42703")
}
