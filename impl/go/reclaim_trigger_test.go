package jed

// The periodic reclamation trigger (format.md *Reclamation*): the walk runs once the pages off the
// free list pass compact_growth × the live count at the last reclamation, not once the high-water
// does. The high-water never shrinks, so measuring it made every commit after the first crossing walk
// the whole database again. These tests count walks — each one moves freeGenTxid to its commit's
// snapshot — over a steady single-row UPDATE churn: the file in-commit path and the in-memory
// post-commit path.

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

const triggerSetupSQL = "INSERT INTO t SELECT g, 'pad-pad-pad-pad-pad-pad-pad-pad' FROM generate_series(1, 1000) g"

// assertFewWalks checks that the churn walked a small fraction of its commits: one walk per commit
// is the high-water regression.
func assertFewWalks(t *testing.T, walks int) {
	t.Helper()
	if walks == 0 || walks > triggerRounds/10 {
		t.Fatalf("%d of %d steady commits walked the database (want between 1 and %d)", walks, triggerRounds, triggerRounds/10)
	}
}

func TestReclaimTriggerFileCommitsDoNotWalkEveryTime(t *testing.T) {
	t.Parallel()
	db, err := create(filepath.Join(t.TempDir(), "trigger.jed"), databaseOptions{PageSize: 256, noSync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, pad text)")
	mustExec(t, db, triggerSetupSQL)
	// Cross the threshold first, so the counted rounds are all past it.
	for k := 0; k < triggerRounds; k++ {
		mustExec(t, db, triggerChurnSQL(k))
	}
	if db.freeGenTxid == 0 {
		t.Fatal("the warm-up churn never reclaimed")
	}
	walks := 0
	for k := 0; k < triggerRounds; k++ {
		gen := db.freeGenTxid
		mustExec(t, db, triggerChurnSQL(triggerRounds+k))
		if db.freeGenTxid != gen {
			walks++
		}
	}
	assertFewWalks(t, walks)
	if hw, live := db.PageCount(), db.liveAtCompaction; hw > 3*live {
		t.Fatalf("high-water %d is unbounded against %d live pages", hw, live)
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
