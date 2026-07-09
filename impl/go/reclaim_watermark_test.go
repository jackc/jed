package jed

// Deterministic regression test for the reader-liveness watermark gating within-session free-list reuse
// (transactions.md §8). This is the concurrency case the corpus cannot express (CLAUDE.md §10): a
// file-backed reader that pins the committed snapshot in the persist→publish window (the "fallback
// reader") must never observe rows from a later commit, even though continuous within-session reclamation
// is recycling pages. It reproduced a snapshot-isolation violation before the free-list generation gate +
// atomic pin registration landed (a pinned reader saw newer rows because its pages were reclaimed and
// overwritten). Uses the afterPersistHook seam to make the race deterministic.

import (
	"fmt"
	"path/filepath"
	"sync/atomic"
	"testing"
)

func countOf(db *Database, rd *Session) (int64, error) {
	rows, err := rd.queryValues("SELECT count(*) FROM t", nil)
	if err != nil {
		return 0, err
	}
	defer rows.Close()
	if !rows.Next() {
		return 0, fmt.Errorf("no row (rows.Err=%v)", rows.Err())
	}
	return rows.Row()[0].Int, nil
}

func TestFallbackReaderSnapshotIsolationUnderReclamation(t *testing.T) {
	path := filepath.Join(t.TempDir(), "fallback.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, PageSize: 256, SkipFsync: true})
	if err != nil {
		t.Fatalf("create: %v", err)
	}
	defer db.Close()
	execDB(t, db, "CREATE TABLE t (id i64 PRIMARY KEY)")
	// Seed a multi-leaf tree so within-session compaction runs and a free-list accumulates.
	for i := 1; i <= 120; i++ {
		execDB(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d)", i))
	}

	var hookFired atomic.Bool
	readerReady := make(chan struct{})
	readerPinned := make(chan struct{})
	var rd *Session
	var pinnedCount int64
	var pinErr error

	// The hook fires in the persist→publish window. It acts ONLY on a commit that compacted — i.e. one
	// that advanced freeGenTxid past the still-published version, so a page reachable at the published
	// (fallback) version has just entered the reusable free-list. That is exactly the version the reader
	// below pins, and the version a subsequent reuse-commit would overwrite on the buggy path.
	afterPersistHook = func() {
		if hookFired.Load() {
			return
		}
		published := db.core.roots.Load().committed.txid
		if db.core.storage.freeGenTxid <= published {
			return // this commit did not compact — nothing fresh in the reusable free-list yet
		}
		hookFired.Store(true)
		close(readerReady)
		<-readerPinned // block the publish until the reader has pinned the prior (fallback) version
	}
	defer func() { afterPersistHook = nil }()

	go func() {
		<-readerReady
		rd = db.ReadSession() // pins the PRIOR committed version (this commit is not yet published)
		pinnedCount, pinErr = countOf(db, rd)
		close(readerPinned)
	}()

	// Drive commits until one compacts and fires the hook (the writer blocks in the hook while the reader
	// pins the fallback version, then the commit publishes).
	for i := 121; !hookFired.Load() && i <= 4000; i++ {
		execDB(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d)", i))
	}
	<-readerPinned
	afterPersistHook = nil
	if pinErr != nil {
		t.Fatalf("reader pin/read: %v", pinErr)
	}
	if !hookFired.Load() {
		t.Fatalf("no compacting commit occurred — test did not exercise the reuse path")
	}

	// The reader is now pinned at the fallback version. Hammer reuse-commits: on the buggy path these
	// recycle a page the reader still references and overwrite it; the gate must defer that reuse while
	// the pin is held.
	for i := 4001; i <= 4200; i++ {
		execDB(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d)", i))
	}

	got, err := countOf(db, rd)
	rd.Close()
	if err != nil {
		t.Fatalf("fallback reader re-read: %v", err)
	}
	if got != pinnedCount {
		t.Fatalf("SNAPSHOT ISOLATION VIOLATED: fallback reader pinned count=%d but now sees %d (its pages were reclaimed and overwritten)", pinnedCount, got)
	}
}
