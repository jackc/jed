package jed

// Within-session free-list compaction (Phase A of routing temp stores through a MemoryBlockStore —
// spec/design/temp-tables.md §6, spec/design/bplus-reshape.md). A reclaim domain
// (storage.reclaimWithinSession) rebuilds its free-list from the live reachable set at commit, so a
// never-reopened in-RAM store reuses its copy-on-write orphans instead of leaking a page per commit.
// These per-core tests cover what the corpus cannot: the internal high-water bound (~2× live) and the
// watermark gate (compaction defers while an older reader is pinned). The main domain leaves the flag
// off, so its reconstruct-on-open behavior (reclamation_test.go) is unchanged — asserted here too.

import (
	"fmt"
	"strings"
	"testing"
)

// churnInMemory builds a small multi-level tree in an in-memory database at page 256, then updates one
// row `rounds` times (each an autocommit copy-on-write commit that orphans its root→leaf path + the
// rewritten catalog). Returns the committed page high-water afterward. reclaim toggles within-session
// compaction on the (single) storage domain.
func churnInMemory(t *testing.T, reclaim bool, rounds int) (uint32, *Database) {
	t.Helper()
	db := newInMemoryWithPageSize(256)
	db.core.storage.reclaimWithinSession = reclaim
	sess := db.Session(SessionOptions{})
	sessExec(t, sess, "CREATE TABLE t (id i32 PRIMARY KEY, pad text)")
	base := strings.Repeat("x", 40)
	for i := 1; i <= 30; i++ {
		sessExec(t, sess, fmt.Sprintf("INSERT INTO t VALUES (%d, 'r%02d-%s')", i, i, base))
	}
	pad := strings.Repeat("y", 40)
	for k := 0; k < rounds; k++ {
		sessExec(t, sess, fmt.Sprintf("UPDATE t SET pad = 'a%d-%s' WHERE id = 15", k, pad))
	}
	return db.PageCount(), db
}

func TestWithinSessionCompactionBoundsInMemoryChurn(t *testing.T) {
	const rounds = 300

	// Control: reclaim OFF is the pre-Phase-A behavior — a never-reopened in-memory store leaks a page
	// per commit, so the high-water grows roughly linearly with the churn count.
	leaked, dbOff := churnInMemory(t, false, rounds)
	dbOff.Close()
	if leaked <= rounds {
		t.Fatalf("control (reclaim off) should leak ~1 page/commit; high-water only %d after %d rounds", leaked, rounds)
	}

	// Reclaim ON: the high-water plateaus at ~2× the live page count (a few dozen pages), independent of
	// the churn count — bounded well under the leaked control.
	bounded, dbOn := churnInMemory(t, true, rounds)
	defer dbOn.Close()
	if bounded > 128 {
		t.Fatalf("reclaim on should bound the high-water at ~2×live; got %d (leaked control was %d)", bounded, leaked)
	}
	if bounded*4 > leaked {
		t.Fatalf("reclaim on (%d) should be far below the leaked control (%d)", bounded, leaked)
	}

	// The churned value and every row survive the reuse (a reclaimed page was dead, never a live one).
	sess := dbOn.Session(SessionOptions{})
	want := fmt.Sprintf("a%d-%s", rounds-1, strings.Repeat("y", 40))
	got := queryRows(t, sess, "SELECT pad FROM t WHERE id = 15")
	if len(got) != 1 || got[0][0].str() != want {
		t.Fatalf("row 15 pad = %v, want %q", got, want)
	}
	if n := len(queryRows(t, sess, "SELECT id FROM t")); n != 30 {
		t.Fatalf("want 30 rows after churn, got %d", n)
	}
}

func TestCompactionDefersWhileOlderReaderPinned(t *testing.T) {
	db := newInMemoryWithPageSize(256)
	db.core.storage.reclaimWithinSession = true
	sess := db.Session(SessionOptions{})
	sessExec(t, sess, "CREATE TABLE t (id i32 PRIMARY KEY, pad text)")
	base := strings.Repeat("x", 40)
	for i := 1; i <= 30; i++ {
		sessExec(t, sess, fmt.Sprintf("INSERT INTO t VALUES (%d, 'r%02d-%s')", i, i, base))
	}
	pad := strings.Repeat("y", 40)

	// Pin an older version with an open read session: compaction must NOT free pages it may still
	// observe, so it defers and the high-water leaks while the reader is open.
	reader := db.ReadSession()
	for k := 0; k < 200; k++ {
		sessExec(t, sess, fmt.Sprintf("UPDATE t SET pad = 'p%d-%s' WHERE id = 15", k, pad))
	}
	withReaderOpen := db.PageCount()
	if withReaderOpen <= 200 {
		t.Fatalf("with an older reader pinned, compaction should defer and leak; high-water only %d", withReaderOpen)
	}

	// Close the reader (watermark advances to committed): a further churn now compacts, so the
	// high-water stops climbing — it grows by a handful of pages (the first post-close commit extends
	// before its own compaction reclaims), not by another ~200.
	reader.Close()
	for k := 200; k < 400; k++ {
		sessExec(t, sess, fmt.Sprintf("UPDATE t SET pad = 'q%d-%s' WHERE id = 15", k, pad))
	}
	afterReaderClosed := db.PageCount()
	if afterReaderClosed-withReaderOpen > 64 {
		t.Fatalf("after the reader closed, compaction should reuse pages, not keep growing: %d then %d (+%d)", withReaderOpen, afterReaderClosed, afterReaderClosed-withReaderOpen)
	}
	db.Close()
}
