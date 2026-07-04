package jed

// Phase B — session-local temp tables ride a per-domain MemoryBlockStore pager (spec/design/temp-tables.md
// §6, bplus-reshape.md), instead of a fully-resident decoded tree. Per-core tests for what the corpus
// cannot express: the internal page-based footprint / bound, the compact (packed) residency, and the
// zero-main-file-write invariant. The SQL-visible temp behavior (rows, errors, 54P03) is the corpus's job
// (ddl/temp_table.test, resource/temp_budget.test) — these assert the storage internals.

import (
	"fmt"
	"path/filepath"
	"strings"
	"testing"
)

// TestSessionLocalTempRunsThroughBlockStore proves a session-local temp table is demand-paged over its
// own in-RAM MemoryBlockStore: rows read back correctly (faulting demoted leaves through the temp pool),
// and heavy churn stays bounded (within-session compaction reclaims copy-on-write orphans — no leak).
func TestSessionLocalTempRunsThroughBlockStore(t *testing.T) {
	db := newInMemoryWithPageSize(256)
	sess := db.Session(SessionOptions{})
	sessExec(t, sess, "CREATE TEMP TABLE lt (id i32 PRIMARY KEY, pad text)")
	base := strings.Repeat("x", 40)
	for i := 1; i <= 60; i++ { // 60 rows at page 256 → a multi-level tree with demoted leaves
		sessExec(t, sess, fmt.Sprintf("INSERT INTO lt VALUES (%d, 'r%02d-%s')", i, i, base))
	}
	if sess.engine.tempStorage == nil {
		t.Fatal("session-local temp DDL should have created a temp storage domain")
	}
	// Reads fault demoted leaves back through the temp pool.
	got := queryRows(t, sess, "SELECT pad FROM lt WHERE id = 42")
	if len(got) != 1 || got[0][0].str() != "r42-"+base {
		t.Fatalf("faulted read wrong: %v", got)
	}
	if n := len(queryRows(t, sess, "SELECT id FROM lt")); n != 60 {
		t.Fatalf("want 60 rows, got %d", n)
	}
	// Churn one row 400×; the high-water plateaus (compaction), it does not grow ~linearly.
	pad := strings.Repeat("y", 40)
	for k := 0; k < 400; k++ {
		sessExec(t, sess, fmt.Sprintf("UPDATE lt SET pad = 'u%d-%s' WHERE id = 30", k, pad))
	}
	if pc := sess.engine.tempStorage.pageCount; pc > 200 {
		t.Fatalf("temp churn not bounded by compaction: pageCount=%d after 400 updates", pc)
	}
	got = queryRows(t, sess, "SELECT pad FROM lt WHERE id = 30")
	if len(got) != 1 || got[0][0].str() != fmt.Sprintf("u399-%s", pad) {
		t.Fatalf("post-churn read wrong: %v", got)
	}
	if n := len(queryRows(t, sess, "SELECT id FROM lt")); n != 60 {
		t.Fatalf("want 60 rows after churn, got %d", n)
	}
}

// TestSessionLocalTempPageBudgetBoundsMultiLeaf is the bug the page-based budget (Design decision 3)
// closes: once temp is paged, its leaves demote to OnDisk, so a record-byte walk sees only the one leaf a
// write touches and undercounts a multi-leaf temp table — the §13 bound would never fire. The page-based
// measure (committed pageCount × page_size) counts every allocated page, so a growing temp table hits
// 54P03 deterministically.
func TestSessionLocalTempPageBudgetBoundsMultiLeaf(t *testing.T) {
	db := newInMemoryWithPageSize(256)
	// ~20 pages of budget: a single leaf (≤ ~240 record bytes) is far under it, so a record-byte measure
	// would never abort; the page footprint crosses it as the tree grows past ~20 pages.
	budget := 20 * 256
	sess := db.Session(SessionOptions{TempBuffers: &budget})
	sessExec(t, sess, "CREATE TEMP TABLE lt (id i32 PRIMARY KEY, pad text)")
	pad := strings.Repeat("z", 40)
	aborted := false
	for i := 1; i <= 400 && !aborted; i++ {
		_, err := queryOutcome(sess, fmt.Sprintf("INSERT INTO lt VALUES (%d, 'r-%s')", i, pad), nil)
		if err != nil {
			if code := err.(*EngineError).Code(); code != "54P03" {
				t.Fatalf("insert %d: want 54P03, got %s (%v)", i, code, err)
			}
			aborted = true
		}
	}
	if !aborted {
		t.Fatal("a multi-leaf temp table past its page budget should abort 54P03; it never did (undercount bug)")
	}
}

// TestSessionLocalTempZeroFileWrites confirms the invariant that survives the flip: session-local temp
// writes touch only the temp MemoryBlockStore, never the main database file (temp-tables.md §2, D1). The
// file's committed version and page high-water are unchanged across a burst of temp DDL/DML.
func TestSessionLocalTempZeroFileWrites(t *testing.T) {
	path := filepath.Join(t.TempDir(), "ztemp.jed")
	db, err := create(path, databaseOptions{PageSize: 256})
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "CREATE TABLE p (id i32 PRIMARY KEY)")
	mustExec(t, db, "INSERT INTO p VALUES (1)")
	baseTxid, basePages := db.Txid(), db.PageCount()

	mustExec(t, db, "CREATE TEMP TABLE lt (id i32 PRIMARY KEY, pad text)")
	pad := strings.Repeat("q", 40)
	for i := 1; i <= 40; i++ {
		mustExec(t, db, fmt.Sprintf("INSERT INTO lt VALUES (%d, '%s')", i, pad))
	}
	for k := 0; k < 40; k++ {
		mustExec(t, db, fmt.Sprintf("UPDATE lt SET pad = 'u%d' WHERE id = 20", k))
	}
	if db.Txid() != baseTxid || db.PageCount() != basePages {
		t.Fatalf("session-local temp writes touched the file: txid %d->%d, pageCount %d->%d",
			baseTxid, db.Txid(), basePages, db.PageCount())
	}
	// The temp data is nonetheless present and correct (it lives in the temp store).
	if n := len(queryRows(t, db, "SELECT id FROM lt")); n != 40 {
		t.Fatalf("want 40 temp rows, got %d", n)
	}
	db.Close()
}
