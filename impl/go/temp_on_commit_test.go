package jed

// CREATE TEMP TABLE … ON COMMIT (spec/design/temp-tables.md §5.1). The SQL-visible behavior — what a
// commit empties or drops, 42P16, the grammar — is the corpus's job (ddl/temp_on_commit.test). These
// cover what the corpus cannot express: the host transaction and prepared-statement surfaces, the
// zero-main-file-write invariant, and the storage footprint under max_temp_storage_bytes.

import (
	"context"
	"fmt"
	"path/filepath"
	"strings"
	"testing"
)

// TestTempOnCommitHostTransaction runs the actions through the host API rather than SQL COMMIT: a
// Session.Update closure commits through the same path, and a prepared SELECT over an ON COMMIT DROP
// table re-resolves after the commit drops it (temp relations are never plan-cached) — 42P01.
func TestTempOnCommitHostTransaction(t *testing.T) {
	t.Parallel()
	sess := memDB().Session(SessionOptions{})
	defer sess.Close()
	sessExec(t, sess, "CREATE TEMP TABLE d (id i32 PRIMARY KEY) ON COMMIT DELETE ROWS")
	err := sess.Update(func(tx *Transaction) error {
		if _, err := tx.Exec(context.Background(), "INSERT INTO d VALUES (1), (2)"); err != nil {
			return err
		}
		rows := queryRows(t, sess, "SELECT id FROM d")
		if len(rows) != 2 {
			t.Fatalf("inside the transaction want 2 rows, got %d", len(rows))
		}
		_, err := tx.Exec(context.Background(), "CREATE TEMP TABLE x (a i32) ON COMMIT DROP")
		return err
	})
	if err != nil {
		t.Fatal(err)
	}
	if n := len(queryRows(t, sess, "SELECT id FROM d")); n != 0 {
		t.Fatalf("the commit should have emptied d, got %d rows", n)
	}
	if _, err := queryOutcome(sess, "SELECT a FROM x", nil); err == nil || err.(*EngineError).Code() != "42P01" {
		t.Fatalf("the commit should have dropped x, got %v", err)
	}

	sessExec(t, sess, "BEGIN")
	sessExec(t, sess, "CREATE TEMP TABLE y (a i32) ON COMMIT DROP")
	sessExec(t, sess, "INSERT INTO y VALUES (7)")
	sel, err := sess.Prepare("SELECT a FROM y")
	if err != nil {
		t.Fatal(err)
	}
	if out, err := prepOutcome(sess, sel, nil); err != nil || len(out.Rows) != 1 {
		t.Fatalf("prepared SELECT inside the block: rows %v, err %v", out.Rows, err)
	}
	sessExec(t, sess, "COMMIT")
	if _, err := prepOutcome(sess, sel, nil); err == nil || err.(*EngineError).Code() != "42P01" {
		t.Fatalf("prepared SELECT after the drop: want 42P01, got %v", err)
	}
}

// TestTempOnCommitZeroFileWrites: the commit-time truncate and drop touch only the temp domain, so a
// transaction whose only effects are temp writes and their ON COMMIT actions leaves the main file's
// version and page high-water unchanged (temp-tables.md §2 D1, transactions.md §2.1).
func TestTempOnCommitZeroFileWrites(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "oncommit.jed")
	db, err := create(path, databaseOptions{PageSize: 256, noSync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	mustExec(t, db, "CREATE TABLE p (id i32 PRIMARY KEY)")
	baseTxid, basePages := db.Txid(), db.PageCount()

	mustExec(t, db, "CREATE TEMP TABLE d (id i32 PRIMARY KEY, pad text) ON COMMIT DELETE ROWS")
	for i := 1; i <= 20; i++ {
		mustExec(t, db, fmt.Sprintf("INSERT INTO d VALUES (%d, '%s')", i, strings.Repeat("q", 40)))
	}
	mustExec(t, db, "CREATE TEMP TABLE x (a serial PRIMARY KEY) ON COMMIT DROP")
	mustExec(t, db, "BEGIN")
	mustExec(t, db, "CREATE TEMP TABLE y (a i32) ON COMMIT DROP")
	mustExec(t, db, "INSERT INTO y VALUES (1)")
	mustExec(t, db, "INSERT INTO d VALUES (1, 'x')")
	mustExec(t, db, "COMMIT")
	if db.Txid() != baseTxid || db.PageCount() != basePages {
		t.Fatalf("ON COMMIT actions touched the file: txid %d->%d, pageCount %d->%d",
			baseTxid, db.Txid(), basePages, db.PageCount())
	}
	if n := len(queryRows(t, db, "SELECT id FROM d")); n != 0 {
		t.Fatalf("want an empty d, got %d rows", n)
	}
}

// TestTempOnCommitDeleteRowsStaysUnderBudget: rows written to an ON COMMIT DELETE ROWS table count
// against max_temp_storage_bytes only until their commit empties the table; the freed pages are
// reclaimed by within-session compaction, so many autocommit inserts never reach the ceiling that the
// same inserts into an ordinary temp table cross (TestSessionLocalTempPageBudgetBoundsMultiLeaf).
func TestTempOnCommitDeleteRowsStaysUnderBudget(t *testing.T) {
	t.Parallel()
	db := newInMemoryWithPageSize(256)
	budget := 20 * 256
	sess := db.Session(SessionOptions{MaxTempStorageBytes: &budget})
	defer sess.Close()
	sessExec(t, sess, "CREATE TEMP TABLE d (id i32 PRIMARY KEY, pad text) ON COMMIT DELETE ROWS")
	pad := strings.Repeat("z", 40)
	for i := 1; i <= 400; i++ {
		sessExec(t, sess, fmt.Sprintf("INSERT INTO d VALUES (%d, 'r-%s')", i, pad))
	}
	if n := len(queryRows(t, sess, "SELECT id FROM d")); n != 0 {
		t.Fatalf("want an empty d, got %d rows", n)
	}
}
