package jed

// Phase 7: the formal host API (spec/design/api.md) — open/create/commit/close a database file,
// Prepare/Execute/Query, the Rows cursor, and the structured-error surface. Files are written
// under t.TempDir(), never the repo tree.

import (
	"path/filepath"
	"slices"
	"testing"
)

func TestCreateCommitReopenRoundTrips(t *testing.T) {
	path := filepath.Join(t.TempDir(), "round_trip.jed")
	db, err := Create(path, DefaultDatabaseOptions())
	if err != nil {
		t.Fatal(err)
	}
	if db.Txid() != 1 { // the initial empty image is committed at create
		t.Fatalf("txid after create = %d want 1", db.Txid())
	}
	mustExec(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, v int32)")
	mustExec(t, db, "INSERT INTO t VALUES (1, 10), (2, 20)")
	if err := db.Commit(); err != nil {
		t.Fatal(err)
	}
	afterCommit := db.Txid()
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	db, err = Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if db.Txid() != afterCommit {
		t.Fatalf("reopened txid = %d want %d", db.Txid(), afterCommit)
	}
	rows := queryRows(t, db, "SELECT id, v FROM t")
	if len(rows) != 2 || rows[0][0].Int != 1 || rows[1][1].Int != 20 {
		t.Fatalf("got %v", rows)
	}
}

func TestOpenMissingFileIs58P01(t *testing.T) {
	path := filepath.Join(t.TempDir(), "nope.jed")
	if _, err := Open(path); err == nil {
		t.Fatal("expected error")
	} else if ee, ok := err.(*EngineError); !ok || ee.Code() != "58P01" {
		t.Fatalf("code = %v want 58P01", err)
	}
}

func TestCreateOverExistingFileIs58P02(t *testing.T) {
	path := filepath.Join(t.TempDir(), "here.jed")
	if _, err := Create(path, DefaultDatabaseOptions()); err != nil {
		t.Fatal(err)
	}
	if _, err := Create(path, DefaultDatabaseOptions()); err == nil {
		t.Fatal("expected error")
	} else if ee, ok := err.(*EngineError); !ok || ee.Code() != "58P02" {
		t.Fatalf("code = %v want 58P02", err)
	}
}

func TestCreateWithCustomPageSizeRoundTrips(t *testing.T) {
	path := filepath.Join(t.TempDir(), "page256.jed")
	db, err := Create(path, DatabaseOptions{PageSize: 256})
	if err != nil {
		t.Fatal(err)
	}
	if db.PageSize() != 256 {
		t.Fatalf("page size = %d want 256", db.PageSize())
	}
	db.Close()
	db, err = Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if db.PageSize() != 256 {
		t.Fatalf("reopened page size = %d want 256", db.PageSize())
	}
}

func TestAutocommitPersistsEachWriteAcrossClose(t *testing.T) {
	// jed autocommits (spec/design/transactions.md §4.1): a write is durable as soon as it
	// succeeds, so it survives a Close with no explicit Commit — the opposite of the original
	// "no autocommit" model this test used to assert.
	path := filepath.Join(t.TempDir(), "autocommit.jed")
	db, err := Create(path, DefaultDatabaseOptions())
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "CREATE TABLE t (id int32 PRIMARY KEY)")
	mustExec(t, db, "INSERT INTO t VALUES (1)") // autocommitted, no explicit commit
	db.Close()

	db, err = Open(path)
	if err != nil {
		t.Fatal(err)
	}
	rows := queryRows(t, db, "SELECT id FROM t")
	if len(rows) != 1 || rows[0][0].Int != 1 {
		t.Fatalf("autocommitted insert must persist, got %v", rows)
	}
}

func TestCommitAndRollbackAreNoopsUnderAutocommit(t *testing.T) {
	// With no explicit transaction open, both are lenient no-op successes (transactions.md §4.2).
	db := NewDatabase()
	mustExec(t, db, "CREATE TABLE t (id int32 PRIMARY KEY)")
	mustExec(t, db, "INSERT INTO t VALUES (1)")
	if err := db.Commit(); err != nil {
		t.Fatal(err)
	}
	if err := db.Rollback(); err != nil { // does NOT undo the autocommitted insert
		t.Fatal(err)
	}
	rows := queryRows(t, db, "SELECT id FROM t")
	if len(rows) != 1 || rows[0][0].Int != 1 {
		t.Fatalf("autocommitted row must remain, got %v", rows)
	}
}

func TestPrepareExecuteAndQueryWithParams(t *testing.T) {
	db := NewDatabase()
	mustExec(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, v int32)")
	insert, err := db.Prepare("INSERT INTO t VALUES ($1, $2)")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := insert.Execute([]Value{IntValue(1), IntValue(100)}); err != nil {
		t.Fatal(err)
	}
	if _, err := insert.Execute([]Value{IntValue(2), IntValue(200)}); err != nil {
		t.Fatal(err)
	}

	sel, err := db.Prepare("SELECT id, v FROM t WHERE v = $1")
	if err != nil {
		t.Fatal(err)
	}
	rows, err := sel.Query([]Value{IntValue(200)})
	if err != nil {
		t.Fatal(err)
	}
	if got := rows.ColumnNames(); len(got) != 2 || got[0] != "id" || got[1] != "v" {
		t.Fatalf("column names = %v", got)
	}
	var collected [][]Value
	for rows.Next() {
		collected = append(collected, rows.Row())
	}
	if len(collected) != 1 || collected[0][0].Int != 2 || collected[0][1].Int != 200 {
		t.Fatalf("got %v", collected)
	}
	if rows.Cost() < 0 {
		t.Fatalf("cost = %d", rows.Cost())
	}
}

func TestQueryOnNonQueryStatementErrors(t *testing.T) {
	db := NewDatabase()
	if _, err := db.QuerySQL("CREATE TABLE t (id int32 PRIMARY KEY)", nil); err == nil {
		t.Fatal("expected error")
	}
}

func TestErrorsSurfaceWithSQLState(t *testing.T) {
	db := NewDatabase()
	if _, err := db.Prepare("SELCT 1"); err == nil {
		t.Fatal("expected error")
	} else if ee, ok := err.(*EngineError); !ok || ee.Code() != "42601" {
		t.Fatalf("code = %v want 42601", err)
	}
}

func TestCommitOnInMemoryIsNoopSuccess(t *testing.T) {
	db := NewDatabase()
	mustExec(t, db, "CREATE TABLE t (id int32 PRIMARY KEY)")
	if err := db.Commit(); err != nil { // no path -> no-op, not an error
		t.Fatal(err)
	}
	if db.Txid() != 0 || db.Path() != "" {
		t.Fatalf("txid=%d path=%q want 0 and empty", db.Txid(), db.Path())
	}
}

func mustExec(t *testing.T, db *Database, sql string) {
	t.Helper()
	if _, err := Execute(db, sql); err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
}

func TestTableNamesListsTablesSortedExcludingIndexes(t *testing.T) {
	// The catalog-read surface (api.md §6): canonical names, sorted ascending by
	// lowercased name; secondary indexes are relations but not tables.
	db := NewDatabase()
	if got := db.TableNames(); len(got) != 0 {
		t.Fatalf("empty catalog: got %v", got)
	}
	mustCreate(t, db, "CREATE TABLE Zed (id int32 PRIMARY KEY, v int32)")
	mustCreate(t, db, "CREATE TABLE apple (id int32 PRIMARY KEY)")
	mustCreate(t, db, "CREATE INDEX zed_v_idx ON Zed (v)")
	// Sorted by LOWERCASED name (apple < zed), returning the canonical spelling (`Zed`).
	want := []string{"apple", "Zed"}
	if got := db.TableNames(); !slices.Equal(got, want) {
		t.Fatalf("TableNames() = %v, want %v", got, want)
	}
	// The visible snapshot includes an open transaction's working set.
	mustCreate(t, db, "BEGIN")
	mustCreate(t, db, "CREATE TABLE mid (id int32 PRIMARY KEY)")
	if got := db.TableNames(); !slices.Equal(got, []string{"apple", "mid", "Zed"}) {
		t.Fatalf("in-tx TableNames() = %v", got)
	}
	mustCreate(t, db, "ROLLBACK")
	if got := db.TableNames(); !slices.Equal(got, want) {
		t.Fatalf("post-rollback TableNames() = %v, want %v", got, want)
	}
}
