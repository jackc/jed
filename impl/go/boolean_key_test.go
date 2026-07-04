package jed

// Boolean as a key (spec/design/types.md §9, encoding.md §2.9) — boolean is the second
// non-integer key type after uuid. Its bool-byte key (0x00 false < 0x01 true) drives a
// boolean PRIMARY KEY, a boolean member of a composite key, and a secondary index on a
// boolean column. The byte-exact stored key is pinned cross-core by bool_pk_table.jed
// (fileformat_golden_test.go); these are the behavioral checks. Mirrors
// impl/rust/tests/boolean_key.rs.

import (
	"slices"
	"testing"
)

func boolKeyErrCode(t *testing.T, db dbHandle, sql string) string {
	t.Helper()
	_, err := queryOutcome(db, sql, nil)
	if err == nil {
		t.Fatalf("expected an error from %q", sql)
	}
	return err.(*EngineError).Code()
}

// A boolean PRIMARY KEY is accepted (the gate lifted) and CRUD works.
func TestBooleanPrimaryKeyCRUD(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE t (k boolean PRIMARY KEY, v i32)",
		"INSERT INTO t VALUES (FALSE, 10), (TRUE, 20)",
	)

	// Point lookup on the boolean PK resolves to the right row.
	out, err := queryOutcome(db, "SELECT v FROM t WHERE k = TRUE", nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Rows) != 1 || out.Rows[0][0].Int != 20 {
		t.Fatalf("k = TRUE got %v, want [20]", out.Rows)
	}
	out, err = queryOutcome(db, "SELECT v FROM t WHERE k = FALSE", nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(out.Rows) != 1 || out.Rows[0][0].Int != 10 {
		t.Fatalf("k = FALSE got %v, want [10]", out.Rows)
	}

	// A full scan iterates in key (byte) order: false (0x00) before true (0x01).
	rows := db.RowsInKeyOrder("t")
	if len(rows) != 2 || rows[0][0].boolVal() != false || rows[1][0].boolVal() != true {
		t.Fatalf("key order got %v, want [false, true]", rows)
	}
}

// A boolean member of a COMPOSITE primary key concatenates with the other component.
func TestBooleanCompositePrimaryKey(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE t (a i32, b boolean, v i32, PRIMARY KEY (a, b))",
		"INSERT INTO t VALUES (1, TRUE, 10), (1, FALSE, 20), (2, FALSE, 30)",
	)
	// (1,FALSE) and (1,TRUE) are distinct keys; the same (a,b) again conflicts.
	if code := boolKeyErrCode(t, db, "INSERT INTO t VALUES (1, TRUE, 99)"); code != "23505" {
		t.Fatalf("duplicate composite tuple: got %s, want 23505", code)
	}
	// Key order: a ascending, then b false<true within an a-group.
	rows := db.RowsInKeyOrder("t")
	type ab struct {
		a int64
		b bool
	}
	want := []ab{{1, false}, {1, true}, {2, false}}
	if len(rows) != len(want) {
		t.Fatalf("got %d rows, want %d", len(rows), len(want))
	}
	for i, r := range rows {
		if r[0].Int != want[i].a || r[1].boolVal() != want[i].b {
			t.Fatalf("row %d = (%d,%v), want (%d,%v)", i, r[0].Int, r[1].boolVal(), want[i].a, want[i].b)
		}
	}
}

// A secondary index on a (nullable) boolean column is accepted and serves equality.
func TestBooleanSecondaryIndex(t *testing.T) {
	db := dbWith(
		t,
		"CREATE TABLE t (id i32 PRIMARY KEY, flag boolean)",
		"INSERT INTO t VALUES (1, TRUE), (2, FALSE), (3, NULL), (4, TRUE)",
		"CREATE INDEX i ON t (flag)",
	)
	out, err := queryOutcome(db, "SELECT id FROM t WHERE flag = TRUE", nil)
	if err != nil {
		t.Fatal(err)
	}
	ids := make([]int64, 0, len(out.Rows))
	for _, r := range out.Rows {
		ids = append(ids, r[0].Int)
	}
	slices.Sort(ids)
	if !slices.Equal(ids, []int64{1, 4}) {
		t.Fatalf("flag = TRUE ids got %v, want [1 4]", ids)
	}
}
