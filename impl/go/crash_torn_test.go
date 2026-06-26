package jed

// Torn-write commit-atomicity sweep (.scratch/testing-ideas.md §1 item 3). Durability is core
// (CLAUDE.md §9) but adversarially under-tested: crash_recovery_test.go arms the pager fault seam at
// a few hand-picked commit points with single-row commits. This file generalizes that to a
// SYSTEMATIC sweep over a FAT, multi-page commit, driven by the recording BlockStore decorator
// (storefault_test.go): one real commit is recorded as its exact write/sync op log, then a power loss
// is replayed at EVERY write/sync boundary — clean prefix, torn boundary page, and (in the fuzz
// target) arbitrary in-flight reordering.
//
// The invariant — the crisp one the single-writer + root-swap model affords: a crash anywhere in a
// commit leaves a file that opens as EXACTLY the pre-commit OR the post-commit snapshot, never a torn
// middle, and never fails to open (losing the database is itself a durability bug). The recovery must
// also never panic or loop. This is the Go carve-out's intrinsic oracle (testing-ideas.md §3): no
// differential needed.

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// recordFatCommit builds a K-row prior snapshot, then drives ONE fat INSERT of M more rows through a
// recording store, returning the pre-commit image, the recorded op log, and the row id sets of the
// pre- and post-commit snapshots. Page size 256 with a padded text column forces a multi-level tree,
// so the single commit dirties many body pages (many writeBlock calls before the body sync) — the
// breadth the sweep needs. The fat INSERT is one autocommit statement ⇒ one persist ⇒ one op log.
func recordFatCommit(tb testing.TB) (prior []byte, ops []storeOp, priorIDs, postIDs []int64) {
	tb.Helper()
	const (
		pageSize = 256
		k        = 8                                      // prior rows: 1..k
		m        = 40                                     // committed rows: k+1..k+m
		pad      = "padpadpadpadpadpadpadpadpadpadpadpad" // ~36 chars → ~3 rows/leaf at ps 256
	)
	path := filepath.Join(tb.TempDir(), "torn_seed.jed")
	db, err := Create(path, DatabaseOptions{PageSize: pageSize})
	if err != nil {
		tb.Fatal(err)
	}
	exec := func(sql string) {
		if _, err := Execute(db, sql); err != nil {
			tb.Fatalf("%q: %v", sql, err)
		}
	}
	exec("CREATE TABLE t (id i32 PRIMARY KEY, pad text)")
	for i := 1; i <= k; i++ {
		exec(fmt.Sprintf("INSERT INTO t VALUES (%d, '%s')", i, pad))
	}
	if err := db.Close(); err != nil {
		tb.Fatal(err)
	}
	priorBytes, err := os.ReadFile(path)
	if err != nil {
		tb.Fatal(err)
	}

	// Re-open the prior image through a recording store and run the fat commit, capturing its ops.
	base := newSliceStore(priorBytes)
	rec := &recordingStore{base: base}
	p, err := pagerFromStore(rec)
	if err != nil {
		tb.Fatal(err)
	}
	rdb, err := LoadDatabasePaged(p, cacheLeaves(DefaultCacheBytes, p.pageSize))
	if err != nil {
		tb.Fatal(err)
	}
	// LoadDatabasePaged alone leaves db.path empty; the real Open path sets it, and commitTx gates the
	// txid increment + meta-slot alternation on db.path != "" (executor.go). Set it so this commit
	// behaves EXACTLY like a reopen-then-write: txid advances and the new meta lands in the OTHER slot,
	// preserving the prior snapshot — the property under test. (Writes still go through the recording
	// store, not this path; path is only the file-backed/in-memory discriminator + the spill dir.)
	rdb.path = path
	var b strings.Builder
	b.WriteString("INSERT INTO t VALUES ")
	for i := 1; i <= m; i++ {
		if i > 1 {
			b.WriteString(", ")
		}
		fmt.Fprintf(&b, "(%d, '%s')", k+i, pad)
	}
	if _, err := Execute(rdb, b.String()); err != nil {
		tb.Fatalf("fat commit: %v", err)
	}
	postBytes := append([]byte(nil), base.buf...)
	if err := rdb.Close(); err != nil {
		tb.Fatal(err)
	}

	priorIDs, err = guardedScanIDs(priorBytes)
	if err != nil {
		tb.Fatalf("prior image must scan cleanly: %v", err)
	}
	postIDs, err = guardedScanIDs(postBytes)
	if err != nil {
		tb.Fatalf("post image must scan cleanly: %v", err)
	}
	if len(priorIDs) != k || len(postIDs) != k+m {
		tb.Fatalf("seed sanity: prior=%d (want %d), post=%d (want %d)", len(priorIDs), k, len(postIDs), k+m)
	}
	return priorBytes, rec.ops, priorIDs, postIDs
}

// assertRecovers opens the reconstructed crash image and asserts the durability invariant: it opens
// cleanly (a crash must not lose the database) AND yields exactly the prior or the post snapshot.
func assertRecovers(tb testing.TB, image []byte, priorIDs, postIDs []int64, what string) {
	tb.Helper()
	ids, err := guardedScanIDs(image)
	if err != nil {
		// A torn/partial commit must still leave a readable snapshot — the prior one at worst. Any
		// error here (including a panic or hang surfaced as a non-EngineError) is a durability failure.
		tb.Fatalf("%s: recovered image failed to open/scan: %v", what, err)
	}
	if equalIDs(ids, priorIDs) || equalIDs(ids, postIDs) {
		return
	}
	tb.Fatalf("%s: recovered a TORN snapshot (neither pre- nor post-commit): %d rows %v", what, len(ids), ids)
}

// TestTornWriteCommitSweepIsAtomic replays a power loss at every write/sync boundary of a fat commit:
// each clean prefix, plus the boundary page torn to a few byte lengths. Every reconstruction must
// recover the prior or the post snapshot — the §9 commit-atomicity guarantee, exercised across a
// whole multi-page commit rather than at a single hand-picked point.
func TestTornWriteCommitSweepIsAtomic(t *testing.T) {
	prior, ops, priorIDs, postIDs := recordFatCommit(t)
	if len(ops) < 4 {
		t.Fatalf("expected a fat op log (many body writes + 2 syncs + meta), got %d ops", len(ops))
	}

	// Clean prefixes: a crash that durably landed exactly ops[0:cut] (cut = 0..len).
	for cut := 0; cut <= len(ops); cut++ {
		img := applyCrash(prior, ops, cut, -1, 0)
		assertRecovers(t, img, priorIDs, postIDs, fmt.Sprintf("clean prefix cut=%d/%d", cut, len(ops)))
	}

	// Torn boundary page: ops[0:i] fully landed, then ops[i] (a write) torn to a partial page. Covers
	// a torn body page (→ prior, the page is unreferenced) and a torn meta page (→ prior, the slot's
	// CRC rejects it and the loader falls back to the other slot).
	for i, op := range ops {
		if op.kind != opWrite {
			continue
		}
		for _, tear := range []int{1, len(op.data) / 2, len(op.data) - 1} {
			if tear <= 0 || tear >= len(op.data) {
				continue
			}
			img := applyCrash(prior, ops, i, tear, 0)
			assertRecovers(t, img, priorIDs, postIDs, fmt.Sprintf("torn write op=%d tear=%d/%d", i, tear, len(op.data)))
		}
	}
}

// TestTornWriteInFlightSubsetsAreAtomic checks the harder real-device case: between the two syncs no
// barrier orders the body writes, so a crash may land an ARBITRARY subset of them (not just a
// prefix). Every subset, with the meta page never yet published, must still recover the prior
// snapshot — confirming no body page is ever wrongly referenced by the prior root (the P6.2
// copy-on-write-to-free-pages torn-safety property).
func TestTornWriteInFlightSubsetsAreAtomic(t *testing.T) {
	prior, ops, priorIDs, postIDs := recordFatCommit(t)

	// Cut just before the body sync: every op so far is an un-barriered body write.
	bodySync := -1
	for i, op := range ops {
		if op.kind == opSync {
			bodySync = i
			break
		}
	}
	if bodySync < 2 {
		t.Fatalf("expected several body writes before the first sync, got bodySync=%d", bodySync)
	}
	cut := bodySync // ops[0:cut] are all body writes, none barriered (lastSync = -1)
	if cut > 10 {
		cut = 10 // keep the subset enumeration (2^cut) bounded
	}
	for mask := uint64(0); mask < (uint64(1) << uint(cut)); mask++ {
		img := applyCrash(prior, ops, cut, -1, mask)
		// No meta written ⇒ the prior root still governs; every body subset must recover the prior snapshot.
		ids, err := guardedScanIDs(img)
		if err != nil {
			t.Fatalf("in-flight body subset mask=%b failed to open: %v", mask, err)
		}
		if !equalIDs(ids, priorIDs) {
			t.Fatalf("in-flight body subset mask=%b recovered %v, want prior %v", mask, ids, priorIDs)
		}
	}
	_ = postIDs
}

// FuzzCommitCrash is the explorer (testing-ideas.md §3: Go explores, the intrinsic oracle judges):
// the fuzz input picks a crash boundary, a tear length, and an in-flight drop mask; every
// reconstruction must recover the prior or post snapshot. Its f.Add seeds run inside `go test`
// (so the path is covered in `rake ci`); `-fuzz` runs it as a campaign (rake fuzz:crash).
func FuzzCommitCrash(f *testing.F) {
	prior, ops, priorIDs, postIDs := recordFatCommit(f)
	f.Add(0, 0, uint64(0))
	f.Add(len(ops), 0, uint64(0))
	f.Add(len(ops)/2, 7, uint64(0b1011))
	f.Add(len(ops)-1, 0, uint64(0xFFFF))
	f.Fuzz(func(t *testing.T, cut, tear int, mask uint64) {
		if cut < 0 {
			cut = -cut
		}
		cut %= len(ops) + 1
		if tear < 0 {
			tear = -1 // a clean (untorn) boundary
		}
		img := applyCrash(prior, ops, cut, tear, mask)
		assertRecovers(t, img, priorIDs, postIDs, fmt.Sprintf("fuzz cut=%d tear=%d mask=%x", cut, tear, mask))
	})
}
