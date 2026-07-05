package jed

// Phase 7: the formal host API (spec/design/api.md) — open/create/commit/close a database file,
// Prepare/queryValues/Exec, the Rows cursor, and the structured-error surface. Files are written
// under t.TempDir(), never the repo tree. Everything runs through the public Database/Session
// surface; the low-level engine is internal.

import (
	"bytes"
	"errors"
	"os"
	"path/filepath"
	"slices"
	"testing"
)

func mustExec(t *testing.T, db dbHandle, sql string) {
	t.Helper()
	if _, err := queryOutcome(db, sql, nil); err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
}

func TestCreateCommitReopenRoundTrips(t *testing.T) {
	path := filepath.Join(t.TempDir(), "round_trip.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	if db.Txid() != 1 { // the initial empty image is committed at create
		t.Fatalf("txid after create = %d want 1", db.Txid())
	}
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
	mustExec(t, db, "INSERT INTO t VALUES (1, 10), (2, 20)") // autocommitted durably
	afterCommit := db.Txid()
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	db, err = OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
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
	if _, err := OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true}); err == nil {
		t.Fatal("expected error")
	} else if ee, ok := err.(*EngineError); !ok || ee.Code() != "58P01" {
		t.Fatalf("code = %v want 58P01", err)
	}
}

func TestCreateOverExistingFileIs58P02(t *testing.T) {
	path := filepath.Join(t.TempDir(), "here.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := CreateDatabase(CreateOptions{Path: path, SkipFsync: true}); err == nil {
		t.Fatal("expected error")
	} else if ee, ok := err.(*EngineError); !ok || ee.Code() != "58P02" {
		t.Fatalf("code = %v want 58P02", err)
	}
}

func TestCreateWithCustomPageSizeRoundTrips(t *testing.T) {
	path := filepath.Join(t.TempDir(), "page256.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, PageSize: 256, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	if db.PageSize() != 256 {
		t.Fatalf("page size = %d want 256", db.PageSize())
	}
	db.Close()
	db, err = OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	if db.PageSize() != 256 {
		t.Fatalf("reopened page size = %d want 256", db.PageSize())
	}
}

func TestAutocommitPersistsEachWriteAcrossClose(t *testing.T) {
	// jed autocommits (spec/design/transactions.md §4.1): a write is durable as soon as it
	// succeeds, so it survives a Close with no explicit Commit — the opposite of the original
	// "no autocommit" model this test used to assert.
	path := filepath.Join(t.TempDir(), "autocommit.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	mustExec(t, db, "INSERT INTO t VALUES (1)") // autocommitted, no explicit commit
	db.Close()

	db, err = OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	rows := queryRows(t, db, "SELECT id FROM t")
	if len(rows) != 1 || rows[0][0].Int != 1 {
		t.Fatalf("autocommitted insert must persist, got %v", rows)
	}
}

func TestCommitAndRollbackAreNoopsUnderAutocommit(t *testing.T) {
	// With no explicit transaction open, both are lenient no-op successes (transactions.md §4.2).
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
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
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
	insert, err := db.Prepare("INSERT INTO t VALUES ($1, $2)")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := prepOutcome(insert, []Value{IntValue(1), IntValue(100)}); err != nil {
		t.Fatal(err)
	}
	if _, err := prepOutcome(insert, []Value{IntValue(2), IntValue(200)}); err != nil {
		t.Fatal(err)
	}

	sel, err := db.Prepare("SELECT id, v FROM t WHERE v = $1")
	if err != nil {
		t.Fatal(err)
	}
	rows, err := sel.queryValues([]Value{IntValue(200)})
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

// TestQueryOnNonQueryStatementIsTotal locks in the total exec/query seam (spec/design/api.md §11):
// Query on a statement that produces no rows is VALID — it returns a Rows with no columns carrying the
// command tag, and (the effect-then-error bug this fixed) the statement's effect actually lands. A
// write run through Query used to commit and THEN return 42601; now it just succeeds.
func TestQueryOnNonQueryStatementIsTotal(t *testing.T) {
	db := memDB().Session(SessionOptions{})

	// DDL through Query: no columns, no rows, no error, and the table is really created.
	rows, err := db.queryValues("CREATE TABLE t (id i32 PRIMARY KEY, v i32)", nil)
	if err != nil {
		t.Fatalf("Query(CREATE TABLE) errored: %v", err)
	}
	if got := rows.ColumnNames(); len(got) != 0 {
		t.Fatalf("statement Rows has columns %v, want none", got)
	}
	if rows.Next() {
		t.Fatal("statement Rows produced a row")
	}
	if err := rows.Err(); err != nil {
		t.Fatalf("drain error: %v", err)
	}
	_ = rows.Close()

	// A write through Query commits AND carries the affected count on the Rows (no 42601).
	wr, err := db.queryValues("INSERT INTO t VALUES (1, 100), (2, 200)", nil)
	if err != nil {
		t.Fatalf("Query(INSERT) errored: %v", err)
	}
	for wr.Next() {
		t.Fatal("INSERT via Query produced a row")
	}
	if n, ok := wr.RowsAffected(); !ok || n != 2 {
		t.Fatalf("RowsAffected = (%d,%v), want (2,true)", n, ok)
	}
	_ = wr.Close()

	// The insert really landed (proves the write committed, not merely reported).
	sel, err := db.queryValues("SELECT v FROM t ORDER BY id", nil)
	if err != nil {
		t.Fatalf("Query(SELECT) errored: %v", err)
	}
	var got [][]Value
	for sel.Next() {
		got = append(got, append([]Value(nil), sel.Row()...))
	}
	if err := sel.Err(); err != nil {
		t.Fatalf("SELECT drain error: %v", err)
	}
	_ = sel.Close()
	if len(got) != 2 || got[0][0].Int != 100 || got[1][0].Int != 200 {
		t.Fatalf("rows after INSERT via Query = %v", got)
	}
}

func TestErrorsSurfaceWithSQLState(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	if _, err := db.Prepare("SELCT 1"); err == nil {
		t.Fatal("expected error")
	} else if ee, ok := err.(*EngineError); !ok || ee.Code() != "42601" {
		t.Fatalf("code = %v want 42601", err)
	}
}

func TestCommitOnInMemoryIsNoopSuccess(t *testing.T) {
	db := memDB().Session(SessionOptions{})
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	before := db.Txid()
	if err := db.Commit(); err != nil { // no open block -> no-op success, not an error
		t.Fatal(err)
	}
	if db.Txid() != before || db.Path() != "" {
		t.Fatalf("txid=%d (want %d) path=%q want empty", db.Txid(), before, db.Path())
	}
}

func TestTableNamesListsTablesSortedExcludingIndexes(t *testing.T) {
	// The catalog-read surface (api.md §6): canonical names, sorted ascending by
	// lowercased name; secondary indexes are relations but not tables. Uses a single Session so the
	// open BEGIN block is visible across calls (the bare Database conveniences mint a fresh session
	// per call and would not share the block).
	db := memDB().Session(SessionOptions{})
	if got := db.TableNames(); len(got) != 0 {
		t.Fatalf("empty catalog: got %v", got)
	}
	mustCreate(t, db, "CREATE TABLE Zed (id i32 PRIMARY KEY, v i32)")
	mustCreate(t, db, "CREATE TABLE apple (id i32 PRIMARY KEY)")
	mustCreate(t, db, "CREATE INDEX zed_v_idx ON Zed (v)")
	// Sorted by LOWERCASED name (apple < zed), returning the canonical spelling (`Zed`).
	want := []string{"apple", "Zed"}
	if got := db.TableNames(); !slices.Equal(got, want) {
		t.Fatalf("TableNames() = %v, want %v", got, want)
	}
	// The visible snapshot includes an open transaction's working set.
	mustCreate(t, db, "BEGIN")
	mustCreate(t, db, "CREATE TABLE mid (id i32 PRIMARY KEY)")
	if got := db.TableNames(); !slices.Equal(got, []string{"apple", "mid", "Zed"}) {
		t.Fatalf("in-tx TableNames() = %v", got)
	}
	mustCreate(t, db, "ROLLBACK")
	if got := db.TableNames(); !slices.Equal(got, want) {
		t.Fatalf("post-rollback TableNames() = %v, want %v", got, want)
	}
}

func TestRowsAffectedReportsDMLCounts(t *testing.T) {
	// The affected-row count (api.md §4): INSERT/UPDATE/DELETE without RETURNING report
	// how many rows they touched (PostgreSQL's command-tag count); a DML statement that
	// matched nothing reports (0, true); DDL and transaction control report (0, false);
	// DML with RETURNING is a query outcome (its row count is the result's length).
	db := memDB().Session(SessionOptions{})
	affected := func(sql string) (int64, bool) {
		t.Helper()
		out, err := queryOutcome(db, sql, nil)
		if err != nil {
			t.Fatalf("%q: %v", sql, err)
		}
		if out.Kind != outcomeStatement {
			t.Fatalf("%q: expected a statement outcome", sql)
		}
		return out.RowsAffected, out.HasRowsAffected
	}

	if n, ok := affected("CREATE TABLE t (id i32 PRIMARY KEY, v i32)"); ok || n != 0 {
		t.Fatalf("DDL: got (%d, %v) want (0, false)", n, ok)
	}
	if n, ok := affected("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)"); !ok || n != 3 {
		t.Fatalf("INSERT: got (%d, %v) want (3, true)", n, ok)
	}
	if n, ok := affected("UPDATE t SET v = v + 1 WHERE id <= 2"); !ok || n != 2 {
		t.Fatalf("UPDATE: got (%d, %v) want (2, true)", n, ok)
	}
	if n, ok := affected("DELETE FROM t WHERE id = 3"); !ok || n != 1 {
		t.Fatalf("DELETE: got (%d, %v) want (1, true)", n, ok)
	}
	if n, ok := affected("DELETE FROM t WHERE id = 99"); !ok || n != 0 {
		t.Fatalf("zero-match DELETE: got (%d, %v) want (0, true)", n, ok)
	}
	if n, ok := affected("BEGIN"); ok || n != 0 {
		t.Fatalf("BEGIN: got (%d, %v) want (0, false)", n, ok)
	}
	if n, ok := affected("COMMIT"); ok || n != 0 {
		t.Fatalf("COMMIT: got (%d, %v) want (0, false)", n, ok)
	}

	// INSERT ... SELECT counts the inserted rows; DML with RETURNING is a Query.
	mustExec(t, db, "CREATE TABLE dst (id i32 PRIMARY KEY)")
	if n, ok := affected("INSERT INTO dst SELECT id FROM t"); !ok || n != 2 {
		t.Fatalf("INSERT ... SELECT: got (%d, %v) want (2, true)", n, ok)
	}
	out, err := queryOutcome(db, "DELETE FROM dst RETURNING id", nil)
	if err != nil {
		t.Fatal(err)
	}
	if out.Kind != outcomeQuery || len(out.Rows) != 2 {
		t.Fatalf("RETURNING must yield a query outcome with 2 rows, got kind=%v rows=%d", out.Kind, len(out.Rows))
	}
}

func TestOpenReadOnlyBlocksWritesAndNeverTouchesTheFile(t *testing.T) {
	// Read-only open (api.md §2.1): the handle behaves like PostgreSQL hot standby — every
	// transaction defaults to READ ONLY, an explicit READ WRITE request and any write are
	// 25006, and the file bytes are never touched.
	path := filepath.Join(t.TempDir(), "readonly.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	mustExec(t, db, "INSERT INTO t VALUES (1)")
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	before, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}

	rodb, err := OpenDatabaseWithOptions(path, OpenOptions{ReadOnly: true, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	if !rodb.ReadOnly() {
		t.Fatal("handle must report read-only")
	}
	// A single stateful session so an open block spans calls (a read-only block, then poisoning).
	s := rodb.Session(SessionOptions{})
	wantCode := func(sql, code string) {
		t.Helper()
		_, err := queryOutcome(s, sql, nil)
		var ee *EngineError
		if !errors.As(err, &ee) || ee.Code() != code {
			t.Fatalf("%q: got %v, want %s", sql, err, code)
		}
	}

	// Reads work — bare and inside an explicit block (plain BEGIN defaults to READ ONLY here).
	out, err := queryOutcome(s, "SELECT id FROM t", nil)
	if err != nil || len(out.Rows) != 1 {
		t.Fatalf("read on a read-only handle: %v", err)
	}
	mustExec(t, s, "BEGIN")
	mustExec(t, s, "SELECT id FROM t")
	mustExec(t, s, "COMMIT")

	// Autocommit writes are 25006 (the implicit transaction is read-only)...
	wantCode("INSERT INTO t VALUES (2)", "25006")
	// ...as are writes inside a block (which then poisons, like any in-block error)...
	mustExec(t, s, "BEGIN")
	wantCode("DELETE FROM t", "25006")
	wantCode("SELECT id FROM t", "25P02")
	mustExec(t, s, "ROLLBACK")
	// ...and an explicit READ WRITE request, via SQL or the host API.
	wantCode("BEGIN READ WRITE", "25006")
	var ee *EngineError
	if err := s.Begin(true); !errors.As(err, &ee) || ee.Code() != "25006" {
		t.Fatalf("Begin(true) on a read-only handle: %v", err)
	}
	if err := rodb.View(func(tx *Transaction) error { _, err := tx.queryValues("SELECT id FROM t", nil); return err }); err != nil {
		t.Fatalf("View on a read-only handle: %v", err)
	}
	err = rodb.Update(func(tx *Transaction) error { _, err := queryOutcome(tx, "DELETE FROM t", nil); return err })
	if !errors.As(err, &ee) || ee.Code() != "25006" {
		t.Fatalf("Update on a read-only handle: %v", err)
	}
	s.Close()
	if err := rodb.Close(); err != nil {
		t.Fatal(err)
	}

	// The file is byte-identical after the whole read-only session.
	after, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(before, after) {
		t.Fatal("a read-only session must not change the file")
	}

	// A normal reopen is writable again.
	wdb, err := OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer wdb.Close()
	if wdb.ReadOnly() {
		t.Fatal("a normal open must not be read-only")
	}
	mustExec(t, wdb, "INSERT INTO t VALUES (2)")
}
