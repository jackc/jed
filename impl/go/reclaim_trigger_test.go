package jed

// The reclamation triggers. A file commit lists the pages it orphans in its own free list
// (incremental-reclaim.md), so steady churn never walks the database and the file stays at its live
// pages plus what the last commit wrote. The in-memory post-commit path keeps the periodic rebuild
// (format.md *Reclamation*): the walk runs once the pages off the free list pass compact_growth × the
// live count at the last reclamation, not once the high-water does — the high-water never shrinks, so
// measuring it made every commit after the first crossing walk the whole database again. These tests
// run a steady single-row UPDATE churn on each path.

import (
	"fmt"
	"path/filepath"
	"testing"
)

const triggerRounds = 200

// triggerChurnSQL is the single-row UPDATE of round k.
func triggerChurnSQL(k int) string {
	return fmt.Sprintf("UPDATE t SET pad = 'r%d' WHERE id = 500", k)
}

const triggerSetupSQL = "INSERT INTO t SELECT g, 'pad-pad-pad-pad-pad-pad-pad-pad' || g::text FROM generate_series(1, 1000) g"

// assertFewWalks checks that the churn walked a small fraction of its commits: one walk per commit
// is the high-water regression.
func assertFewWalks(t *testing.T, walks int) {
	t.Helper()
	if walks == 0 || walks > triggerRounds/10 {
		t.Fatalf("%d of %d steady commits walked the database (want between 1 and %d)", walks, triggerRounds, triggerRounds/10)
	}
}

// Not parallel: reclaimWalks is process-wide, and top-level parallel tests start only after the
// sequential ones finish.
func TestReclaimTriggerFileCommitsListTheirOrphans(t *testing.T) {
	if !incrOn() {
		t.Skip("a Go-only prototype that disables incremental reclamation is on")
	}
	db, err := create(filepath.Join(t.TempDir(), "trigger.jed"), databaseOptions{PageSize: 256, noSync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, pad text)")
	mustExec(t, db, triggerSetupSQL)
	walks := reclaimWalks.Load()
	for k := 0; k < 2*triggerRounds; k++ {
		mustExec(t, db, triggerChurnSQL(k))
		// Every page below the high-water is meta, live, listed free, or a free-list chain or
		// manifest page the commit itself wrote.
		chain := 0
		if db.freeChain != nil {
			chain = len(db.freeChain.refs)
		}
		first := db.paging.cap().firstBody()
		if unlisted := int(db.pageCount) - int(first) - int(db.live.live) - len(db.freePages) - chain; unlisted < 0 || unlisted > 2 {
			t.Fatalf("round %d: %d pages, %d live, %d free, %d chain: %d dead pages unlisted", k, db.pageCount, db.live.live, len(db.freePages), chain, unlisted)
		}
	}
	if n := reclaimWalks.Load() - walks; n != 0 {
		t.Fatalf("%d of %d steady commits walked the database (want 0)", n, 2*triggerRounds)
	}
}

func TestReclaimTriggerInMemoryCommitsDoNotWalkEveryTime(t *testing.T) {
	t.Parallel()
	db := newInMemoryWithPageSize(256, NewEnvironment())
	defer db.Close()
	st := db.core.storage
	st.reclaimWithinSession = true
	sess := db.Session(SessionOptions{})
	sessExec(t, sess, "CREATE TABLE t (id i32 PRIMARY KEY, pad text)")
	sessExec(t, sess, triggerSetupSQL)
	for k := 0; k < triggerRounds; k++ {
		sessExec(t, sess, triggerChurnSQL(k))
	}
	if st.freeGenTxid == 0 {
		t.Fatal("the warm-up churn never reclaimed")
	}
	walks := 0
	for k := 0; k < triggerRounds; k++ {
		gen := st.freeGenTxid
		sessExec(t, sess, triggerChurnSQL(triggerRounds+k))
		if st.freeGenTxid != gen {
			walks++
		}
	}
	assertFewWalks(t, walks)
}
