package abide

// Phase C: INSERT ... VALUES — positional type-checking, overflow trap (22003),
// NOT NULL (23502) and unique-PK (23505) enforcement, storage in PK order.

import "testing"

func dbWith(t *testing.T, stmts ...string) *Database {
	t.Helper()
	db := NewDatabase()
	for _, s := range stmts {
		if _, err := Execute(db, s); err != nil {
			t.Fatalf("setup %q: %v", s, err)
		}
	}
	return db
}

func ids(rows []Row) []int64 {
	out := make([]int64, len(rows))
	for i, r := range rows {
		out[i] = r[0].Int
	}
	return out
}

func eqInts(a []int64, b ...int64) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

func TestInsertsRowsInPrimaryKeyOrder(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)")
	mustCreate(t, db, "INSERT INTO t VALUES (3, 30)")
	mustCreate(t, db, "INSERT INTO t VALUES (1, 10)")
	mustCreate(t, db, "INSERT INTO t VALUES (2, 20)")
	if got := ids(db.RowsInKeyOrder("t")); !eqInts(got, 1, 2, 3) {
		t.Errorf("key order got %v want [1 2 3]", got)
	}
}

func TestNegativeKeysSortBeforePositive(t *testing.T) {
	// Exercises the sign-flip in the order-preserving key encoding.
	db := dbWith(t, "CREATE TABLE t (id int32 PRIMARY KEY)")
	for _, s := range []string{"INSERT INTO t VALUES (1)", "INSERT INTO t VALUES (-1)", "INSERT INTO t VALUES (0)"} {
		mustCreate(t, db, s)
	}
	if got := ids(db.RowsInKeyOrder("t")); !eqInts(got, -1, 0, 1) {
		t.Errorf("key order got %v want [-1 0 1]", got)
	}
}

func TestBoundaryValuesRoundTrip(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id int32 PRIMARY KEY, s int16, b int64)")
	mustCreate(t, db, "INSERT INTO t VALUES (1, 32767, 9223372036854775807)")
	mustCreate(t, db, "INSERT INTO t VALUES (2, -32768, -9223372036854775808)")
	rows := db.RowsInKeyOrder("t")
	if rows[0][1].Int != 32767 || rows[0][2].Int != 9223372036854775807 {
		t.Errorf("row 0 wrong: %+v", rows[0])
	}
	if rows[1][1].Int != -32768 || rows[1][2].Int != -9223372036854775808 {
		t.Errorf("row 1 wrong: %+v", rows[1])
	}
}

func TestOverflowTrapsAndRowIsNotStored(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id int32 PRIMARY KEY, s int16)")
	mustCreate(t, db, "INSERT INTO t VALUES (1, 32767)")
	wantErr(t, db, "INSERT INTO t VALUES (2, 32768)", "22003")
	wantErr(t, db, "INSERT INTO t VALUES (3, -32769)", "22003")
	if n := len(db.RowsInKeyOrder("t")); n != 1 {
		t.Errorf("expected 1 row stored, got %d", n)
	}
}

func TestInt32OverflowBoundary(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id int32 PRIMARY KEY, n int32)")
	wantErr(t, db, "INSERT INTO t VALUES (1, 2147483648)", "22003")
	mustCreate(t, db, "INSERT INTO t VALUES (2, 2147483647)")
}

func TestNullIntoNullableColumnIsStored(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)")
	mustCreate(t, db, "INSERT INTO t VALUES (1, NULL)")
	rows := db.RowsInKeyOrder("t")
	if !rows[0][1].IsNull() {
		t.Errorf("expected NULL stored, got %+v", rows[0][1])
	}
}

func TestNullIntoPrimaryKeyTraps(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)")
	wantErr(t, db, "INSERT INTO t VALUES (NULL, 1)", "23502")
}

func TestDuplicatePrimaryKeyTraps(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id int32 PRIMARY KEY)")
	mustCreate(t, db, "INSERT INTO t VALUES (1)")
	wantErr(t, db, "INSERT INTO t VALUES (1)", "23505")
}

func TestWrongValueCountIsRejected(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)")
	wantErr(t, db, "INSERT INTO t VALUES (1)", "42601")
	wantErr(t, db, "INSERT INTO t VALUES (1, 2, 3)", "42601")
}

func TestInsertIntoMissingTableTraps(t *testing.T) {
	db := NewDatabase()
	wantErr(t, db, "INSERT INTO nope VALUES (1)", "42P01")
}

// --- multi-row INSERT (spec/design/grammar.md §12) --------------------------------

func TestMultiRowInsertStoresAllRowsInKeyOrder(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)")
	// One statement, rows out of key order; storage must yield them in PK order.
	mustCreate(t, db, "INSERT INTO t VALUES (3, 30), (1, 10), (2, 20)")
	if got := ids(db.RowsInKeyOrder("t")); !eqInts(got, 1, 2, 3) {
		t.Errorf("key order got %v want [1 2 3]", got)
	}
}

func TestMultiRowInsertAllOrNothingOnOverflow(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id int32 PRIMARY KEY, s int16)")
	// The second row overflows int16 — the whole statement fails, storing nothing.
	wantErr(t, db, "INSERT INTO t VALUES (1, 10), (2, 99999)", "22003")
	if n := len(db.RowsInKeyOrder("t")); n != 0 {
		t.Errorf("expected 0 rows stored, got %d", n)
	}
}

func TestMultiRowInsertDuplicateWithinBatchTraps(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id int32 PRIMARY KEY)")
	wantErr(t, db, "INSERT INTO t VALUES (1), (1)", "23505")
	if n := len(db.RowsInKeyOrder("t")); n != 0 {
		t.Errorf("expected 0 rows stored, got %d", n)
	}
}

func TestMultiRowInsertDuplicateAgainstStoredTraps(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id int32 PRIMARY KEY)")
	mustCreate(t, db, "INSERT INTO t VALUES (1)")
	// The batch's second row collides with stored row 1; the new row 2 must not land.
	wantErr(t, db, "INSERT INTO t VALUES (2), (1)", "23505")
	if got := ids(db.RowsInKeyOrder("t")); !eqInts(got, 1) {
		t.Errorf("got %v want [1]", got)
	}
}

func TestMultiRowInsertWrongArityInOneRowIsRejected(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)")
	wantErr(t, db, "INSERT INTO t VALUES (1, 10), (2)", "42601")
	if n := len(db.RowsInKeyOrder("t")); n != 0 {
		t.Errorf("expected 0 rows stored, got %d", n)
	}
}

func TestNoPKMultiRowInsertKeepsInsertionOrder(t *testing.T) {
	db := dbWith(t, "CREATE TABLE log (a int32)")
	// No PK ⇒ monotonic synthetic rowids, allocated left-to-right; key order = insertion order.
	mustCreate(t, db, "INSERT INTO log VALUES (30), (10), (20)")
	if got := ids(db.RowsInKeyOrder("log")); !eqInts(got, 30, 10, 20) {
		t.Errorf("got %v want [30 10 20]", got)
	}
}

func TestNoPKMultiRowInsertIsAllOrNothing(t *testing.T) {
	db := dbWith(t, "CREATE TABLE log (a int16)")
	mustCreate(t, db, "INSERT INTO log VALUES (1)")
	// The batch fails validation (second row overflows), so its first row (2) is not stored.
	wantErr(t, db, "INSERT INTO log VALUES (2), (99999)", "22003")
	mustCreate(t, db, "INSERT INTO log VALUES (3), (4)")
	if got := ids(db.RowsInKeyOrder("log")); !eqInts(got, 1, 3, 4) {
		t.Errorf("got %v want [1 3 4]", got)
	}
}
