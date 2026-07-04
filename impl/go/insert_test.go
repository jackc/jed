package jed

// Phase C: INSERT ... VALUES — positional type-checking, overflow trap (22003),
// NOT NULL (23502) and unique-PK (23505) enforcement, storage in PK order.

import "testing"

func dbWith(t *testing.T, stmts ...string) *Session {
	t.Helper()
	db := memDB().Session(SessionOptions{})
	for _, s := range stmts {
		if _, err := queryOutcome(db, s, nil); err != nil {
			t.Fatalf("setup %q: %v", s, err)
		}
	}
	return db
}

func ids(rows []storedRow) []int64 {
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

func TestNegativeKeysSortBeforePositive(t *testing.T) {
	// Exercises the sign-flip in the order-preserving key encoding.
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY)")
	for _, s := range []string{"INSERT INTO t VALUES (1)", "INSERT INTO t VALUES (-1)", "INSERT INTO t VALUES (0)"} {
		mustCreate(t, db, s)
	}
	if got := ids(db.RowsInKeyOrder("t")); !eqInts(got, -1, 0, 1) {
		t.Errorf("key order got %v want [-1 0 1]", got)
	}
}

func TestBoundaryValuesRoundTrip(t *testing.T) {
	db := dbWith(t, "CREATE TABLE t (id i32 PRIMARY KEY, s i16, b i64)")
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

func TestInsertIntoMissingTableTraps(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	wantErr(t, db, "INSERT INTO nope VALUES (1)", "42P01")
}

// --- multi-row INSERT (spec/design/grammar.md §12) --------------------------------

func TestNoPKMultiRowInsertKeepsInsertionOrder(t *testing.T) {
	db := dbWith(t, "CREATE TABLE log (a i32)")
	// No PK ⇒ monotonic synthetic rowids, allocated left-to-right; key order = insertion order.
	mustCreate(t, db, "INSERT INTO log VALUES (30), (10), (20)")
	if got := ids(db.RowsInKeyOrder("log")); !eqInts(got, 30, 10, 20) {
		t.Errorf("got %v want [30 10 20]", got)
	}
}

func TestNoPKMultiRowInsertIsAllOrNothing(t *testing.T) {
	db := dbWith(t, "CREATE TABLE log (a i16)")
	mustCreate(t, db, "INSERT INTO log VALUES (1)")
	// The batch fails validation (second row overflows), so its first row (2) is not stored.
	wantErr(t, db, "INSERT INTO log VALUES (2), (99999)", "22003")
	mustCreate(t, db, "INSERT INTO log VALUES (3), (4)")
	if got := ids(db.RowsInKeyOrder("log")); !eqInts(got, 1, 3, 4) {
		t.Errorf("got %v want [1 3 4]", got)
	}
}

// --- INSERT ... SELECT (spec/design/grammar.md §24) -----------------------------------
// Most behavior is pinned by the shared corpus (suites/dml/insert_select.test). These cover
// the param-in-source case (the corpus is literal-only) and assert the cost number directly.

func TestInsertSelectParamInSourceWhere(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE src (id i32 PRIMARY KEY, a i16)",
		"INSERT INTO src VALUES (1, 10), (2, 20), (3, 30)",
		"CREATE TABLE dst (id i32 PRIMARY KEY, a i16)",
	)
	// A $1 inside the source SELECT binds through the SELECT's own resolver.
	if _, err := queryOutcome(db, "INSERT INTO dst SELECT id, a FROM src WHERE id >= $1", []Value{IntValue(2)}); err != nil {
		t.Fatalf("INSERT ... SELECT with param: %v", err)
	}
	if got := ids(db.RowsInKeyOrder("dst")); !eqInts(got, 2, 3) {
		t.Errorf("got %v want [2 3]", got)
	}
}

func TestInsertSelectCostIsEmbeddedSelectCost(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE src (id i32 PRIMARY KEY, a i16, b i64)",
		"INSERT INTO src VALUES (1, 10, 100), (2, 20, 200), (3, 30, 300)",
		"CREATE TABLE dst (id i32 PRIMARY KEY, a i16, b i64)",
	)
	// 1 page_read (src is one leaf) + 3 scanned + 3 produced + 0 projection (bare columns) = 7;
	// storing the rows is unmetered.
	out := mustCreate(t, db, "INSERT INTO dst SELECT id, a, b FROM src")
	if out.Kind != outcomeStatement || out.Cost != 7 {
		t.Errorf("got kind=%v cost=%d, want statement cost=7", out.Kind, out.Cost)
	}
}
