package jed

// Crash-recovery tests driven by the fault-injection seam (spec/design/storage.md §7). These verify
// the §4 commit atomicity at the actual commit points — mid-body, before the body sync, between the
// body and meta syncs, and a torn meta write — which the static torn_meta_slot*.jed goldens (a
// post-hoc byte corruption) cannot reach. The invariant under test: a crash anywhere in a commit
// leaves the file readable as a valid snapshot (the prior one, or — at the last barrier — the new
// one), never corrupt; and the free-list reconstruction (P6.2) stays correct after a recovery. This
// is per-core, not corpus (a crash mid-commit is not SQL-level deterministic, like P5.3 concurrency);
// the cross-core contract is the recovery outcome, asserted identically in Rust and TS
// (recovery.rs, crash_recovery.test.ts).

import (
	"path/filepath"
	"sort"
	"testing"
)

// armCommitFault arms a one-shot commit fault on db's backing pager (storage.md §7). Testing only.
func armCommitFault(t *testing.T, db *engine, f commitFault) {
	t.Helper()
	if err := db.paging.withPager(func(p *pager) error { p.armFault(f); return nil }); err != nil {
		t.Fatal(err)
	}
}

// seeded returns a fresh file-backed t(id i32 PRIMARY KEY) holding rows 1,2 (each INSERT autocommits
// durably) and the prior committed txid.
func seedTwoRows(t *testing.T, path string) (*engine, uint64) {
	t.Helper()
	db, err := create(path, DefaultDatabaseOptions())
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY)")
	mustExec(t, db, "INSERT INTO t VALUES (1)")
	mustExec(t, db, "INSERT INTO t VALUES (2)")
	return db, db.Txid()
}

// sortedIDs returns t's ids ascending (the B-tree scan is key-ordered, but sort to be order-robust).
func sortedIDs(t *testing.T, db *engine) []int64 {
	t.Helper()
	ids := selectIDs(t, db)
	sort.Slice(ids, func(i, j int) bool { return ids[i] < ids[j] })
	return ids
}

func equalIDs(a, b []int64) bool {
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

// insertWithFault arms f, then runs an autocommit INSERT (3) that drives persist into it — which must
// fail. Closes db (a clean close rolls back, no further writes) so the file is left in its crash state.
func insertWithFault(t *testing.T, db *engine, f commitFault) {
	t.Helper()
	armCommitFault(t, db, f)
	if _, err := execute(db, "INSERT INTO t VALUES (3)"); err == nil {
		t.Fatal("expected the injected commit crash to fail the INSERT")
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
}

func reopen(t *testing.T, path string) *engine {
	t.Helper()
	db, err := open(path)
	if err != nil {
		t.Fatal(err)
	}
	return db
}

// faultBodyWrite(1) — a clean crash on the first body-page write, before the body is even synced. The
// new commit's pages are partial/unreferenced and the prior meta is untouched, so the file reopens at
// the prior two-row snapshot.
func TestCrashMidBodyRecoversPrior(t *testing.T) {
	path := filepath.Join(t.TempDir(), "crash_mid_body.jed")
	db, prior := seedTwoRows(t, path)
	insertWithFault(t, db, commitFault{point: faultBodyWrite, n: 1, tearBytes: -1})

	db = reopen(t, path)
	defer db.Close()
	if db.Txid() != prior {
		t.Fatalf("should fall back to the prior snapshot, got txid %d (prior %d)", db.Txid(), prior)
	}
	if got := sortedIDs(t, db); !equalIDs(got, []int64{1, 2}) {
		t.Fatalf("prior snapshot should be intact, got %v", got)
	}
}

// faultBodyWrite(1) torn — a partial first body-page write. A dirty page is always a freshly allocated
// slot (copy-on-write never overwrites a page the prior meta references — P6.2 torn-safety), so the
// torn page is unreferenced and the prior snapshot reopens intact.
func TestTornBodyPageRecoversPrior(t *testing.T) {
	path := filepath.Join(t.TempDir(), "torn_body.jed")
	db, prior := seedTwoRows(t, path)
	insertWithFault(t, db, commitFault{point: faultBodyWrite, n: 1, tearBytes: 64})

	db = reopen(t, path)
	defer db.Close()
	if db.Txid() != prior {
		t.Fatalf("torn body page is never referenced, expected prior txid %d, got %d", prior, db.Txid())
	}
	if got := sortedIDs(t, db); !equalIDs(got, []int64{1, 2}) {
		t.Fatalf("prior snapshot should be intact, got %v", got)
	}
}

// faultSync(1) — the body-durability barrier fails. The body is written-through but unsynced and the
// meta is never written, so the prior meta still governs and the prior snapshot reopens.
func TestCrashBeforeBodySyncRecoversPrior(t *testing.T) {
	path := filepath.Join(t.TempDir(), "crash_body_sync.jed")
	db, prior := seedTwoRows(t, path)
	insertWithFault(t, db, commitFault{point: faultSync, n: 1, tearBytes: -1})

	db = reopen(t, path)
	defer db.Close()
	if db.Txid() != prior {
		t.Fatalf("expected prior txid %d, got %d", prior, db.Txid())
	}
	if got := sortedIDs(t, db); !equalIDs(got, []int64{1, 2}) {
		t.Fatalf("prior snapshot should be intact, got %v", got)
	}
}

// faultMetaWrite — the critical between-syncs window (§4): the body is fully written AND synced, then
// the publish (the meta-slot write) crashes. The new body pages are durable but unreferenced; the
// prior meta slot is untouched, so the file reopens at the prior snapshot.
func TestCrashBetweenSyncsRecoversPrior(t *testing.T) {
	path := filepath.Join(t.TempDir(), "crash_between_syncs.jed")
	db, prior := seedTwoRows(t, path)
	insertWithFault(t, db, commitFault{point: faultMetaWrite, tearBytes: -1})

	db = reopen(t, path)
	defer db.Close()
	if db.Txid() != prior {
		t.Fatalf("durable-but-unreferenced body should reopen at prior txid %d, got %d", prior, db.Txid())
	}
	if got := sortedIDs(t, db); !equalIDs(got, []int64{1, 2}) {
		t.Fatalf("prior snapshot should be intact, got %v", got)
	}
}

// faultMetaWrite torn — a partial meta-slot write corrupts its checksum. The loader rejects the torn
// slot (CRC mismatch) and falls back to the other, valid slot — the prior snapshot. This is the
// torn_meta_slot*.jed golden's property, now exercised at the actual publish point. Write only the
// first 20 bytes: the checksum at offset 32 keeps its old value while bytes [0,32) change → mismatch.
func TestTornMetaWriteFallsBackToPrior(t *testing.T) {
	path := filepath.Join(t.TempDir(), "torn_meta.jed")
	db, prior := seedTwoRows(t, path)
	insertWithFault(t, db, commitFault{point: faultMetaWrite, tearBytes: 20})

	db = reopen(t, path)
	defer db.Close()
	if db.Txid() != prior {
		t.Fatalf("torn meta slot should be rejected → fall back to prior txid %d, got %d", prior, db.Txid())
	}
	if got := sortedIDs(t, db); !equalIDs(got, []int64{1, 2}) {
		t.Fatalf("prior snapshot should be intact, got %v", got)
	}
}

// faultSync(2) — the meta is written, then its durability barrier fails. Atomicity holds either way: a
// real power loss could keep the meta (→ new) or lose it (→ prior); the seam writes through, so the
// reopen deterministically yields the new snapshot. Both are valid — assert a consistent, fully
// readable snapshot that is exactly one of the two (never a half-published state).
func TestCrashBeforeMetaSyncIsAtomic(t *testing.T) {
	path := filepath.Join(t.TempDir(), "crash_meta_sync.jed")
	db, prior := seedTwoRows(t, path)
	insertWithFault(t, db, commitFault{point: faultSync, n: 2, tearBytes: -1})

	db = reopen(t, path)
	defer db.Close()
	got := sortedIDs(t, db)
	if db.Txid() == prior {
		if !equalIDs(got, []int64{1, 2}) {
			t.Fatalf("prior snapshot (meta lost) should be {1,2}, got %v", got)
		}
	} else if db.Txid() == prior+1 {
		if !equalIDs(got, []int64{1, 2, 3}) {
			t.Fatalf("new snapshot (meta survived) should be {1,2,3}, got %v", got)
		}
	} else {
		t.Fatalf("txid should be prior %d or prior+1, got %d", prior, db.Txid())
	}
}

// After a crash-to-prior recovery the file is fully functional: the free-list reconstructs correctly
// on the reopen (P6.2), so subsequent commits reuse dead pages, persist durably, and round-trip — and
// the file does not corrupt across the crash → reopen → churn → reopen cycle.
func TestRecoveryThenFreeListReuseStaysConsistent(t *testing.T) {
	path := filepath.Join(t.TempDir(), "recovery_then_reuse.jed")
	db, prior := seedTwoRows(t, path)

	// Crash between the syncs → reopen at the prior two-row snapshot.
	insertWithFault(t, db, commitFault{point: faultMetaWrite, tearBytes: -1})
	db = reopen(t, path)
	if db.Txid() != prior {
		t.Fatalf("expected prior txid %d, got %d", prior, db.Txid())
	}
	if got := sortedIDs(t, db); !equalIDs(got, []int64{1, 2}) {
		t.Fatalf("expected {1,2} after recovery, got %v", got)
	}

	// Churn through several commits (frees pages a prior root abandoned, then reuses them).
	mustExec(t, db, "INSERT INTO t VALUES (3)")
	mustExec(t, db, "INSERT INTO t VALUES (4)")
	mustExec(t, db, "DELETE FROM t WHERE id = 1")
	mustExec(t, db, "INSERT INTO t VALUES (5)")
	pageCountAfter := db.PageCount()
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	db = reopen(t, path)
	if got := sortedIDs(t, db); !equalIDs(got, []int64{2, 3, 4, 5}) {
		t.Fatalf("post-recovery commits should be durable and correct, got %v", got)
	}

	// A second churn round reuses the reconstructed free-list rather than growing the file unbounded.
	mustExec(t, db, "DELETE FROM t WHERE id = 2")
	mustExec(t, db, "INSERT INTO t VALUES (6)")
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	db = reopen(t, path)
	defer db.Close()
	if got := sortedIDs(t, db); !equalIDs(got, []int64{3, 4, 5, 6}) {
		t.Fatalf("expected {3,4,5,6}, got %v", got)
	}
	if db.PageCount() > pageCountAfter+4 {
		t.Fatalf("free-list reuse should keep the file bounded after recovery (was %d, now %d)", pageCountAfter, db.PageCount())
	}
}
