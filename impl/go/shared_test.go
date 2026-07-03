package jed

// Phase 5 (P5.3b): the goroutine-safe shared handle — concurrent readers + a single writer,
// lock-free reads, and the oldest-live-version watermark (spec/design/transactions.md §8/§10). The
// SQL transaction semantics are pinned by the shared conformance corpus (suites/transactions/);
// these per-core tests cover what the corpus cannot express — concurrency: that a reader pins a
// consistent snapshot, runs in parallel with a writer without blocking, and that the watermark
// tracks live readers (the Phase-6 reclamation gate). Run under `go test -race`.

import (
	"fmt"
	"sync"
	"testing"
)

// readCount runs SELECT count(*) FROM t against a read handle and returns the count.
func readCount(t *testing.T, r *Session) int64 {
	t.Helper()
	rows, err := r.Query("SELECT count(*) FROM t", nil)
	if err != nil {
		t.Fatalf("count query: %v", err)
	}
	if !rows.Next() {
		t.Fatalf("count query returned no row")
	}
	v := rows.Row()[0]
	if v.Kind != ValInt {
		t.Fatalf("expected an int count, got kind %v", v.Kind)
	}
	return v.Int
}

// seeded builds a shared db with table t holding the given ids, committed via a write handle.
func seeded(t *testing.T, ids ...int64) *Database {
	t.Helper()
	db := memDB()
	w := db.WriteSession()
	if _, err := w.Execute("CREATE TABLE t (id bigint PRIMARY KEY)", nil); err != nil {
		t.Fatalf("create: %v", err)
	}
	for _, id := range ids {
		if _, err := w.Execute(fmt.Sprintf("INSERT INTO t VALUES (%d)", id), nil); err != nil {
			t.Fatalf("insert %d: %v", id, err)
		}
	}
	if err := w.Commit(); err != nil {
		t.Fatalf("commit: %v", err)
	}
	return db
}

func TestSharedWriteThenReadSeesCommittedRows(t *testing.T) {
	db := seeded(t, 1, 2, 3)
	if db.Version() != 1 {
		t.Fatalf("version = %d, want 1", db.Version())
	}
	r := db.ReadSession()
	defer r.Close()
	if got := readCount(t, r); got != 3 {
		t.Fatalf("count = %d, want 3", got)
	}
}

func TestSharedReadHandleRejectsWrites(t *testing.T) {
	db := seeded(t, 1)
	r := db.ReadSession()
	defer r.Close()
	_, err := r.Execute("INSERT INTO t VALUES (2)", nil)
	if err == nil {
		t.Fatal("expected a write through a read handle to fail")
	}
	if ee, ok := err.(*EngineError); !ok || ee.Code() != "25006" {
		t.Fatalf("error = %v, want code 25006", err)
	}
	if got := readCount(t, r); got != 1 { // still usable, still the pinned snapshot
		t.Fatalf("count after rejected write = %d, want 1", got)
	}
}

func TestSharedReaderDoesNotBlockOnOpenWriter(t *testing.T) {
	// A reader running while a writer holds an open, uncommitted transaction must not block and
	// must see the pre-commit (committed) state — the core "readers parallel with a writer" claim.
	db := seeded(t, 1)
	w := db.WriteSession()
	if _, err := w.Execute("INSERT INTO t VALUES (2)", nil); err != nil { // staged, not committed
		t.Fatalf("staged insert: %v", err)
	}
	r := db.ReadSession() // does NOT block on the open writer
	defer r.Close()
	if got := readCount(t, r); got != 1 { // sees only the committed row
		t.Fatalf("count during open writer = %d, want 1", got)
	}
	if err := w.Commit(); err != nil {
		t.Fatalf("commit: %v", err)
	}
	if got := readCount(t, r); got != 1 { // the already-pinned reader is unaffected by the commit
		t.Fatalf("count after commit (pinned) = %d, want 1", got)
	}
	r2 := db.ReadSession()
	defer r2.Close()
	if got := readCount(t, r2); got != 2 { // a fresh reader sees the new row
		t.Fatalf("fresh reader count = %d, want 2", got)
	}
}

func TestSharedPinnedReaderIsolatedFromConcurrentWriter(t *testing.T) {
	db := seeded(t, 1)
	pinned := db.ReadSession() // pins version 1 (one row)
	defer pinned.Close()

	done := make(chan struct{})
	go func() {
		defer close(done)
		w := db.WriteSession()
		if _, err := w.Execute("INSERT INTO t VALUES (2)", nil); err != nil {
			t.Errorf("writer insert: %v", err)
			return
		}
		if err := w.Commit(); err != nil {
			t.Errorf("writer commit: %v", err)
		}
	}()
	<-done

	if got := readCount(t, pinned); got != 1 { // snapshot isolation: pinned reader unchanged
		t.Fatalf("pinned reader count = %d, want 1", got)
	}
	if db.Version() != 2 { // the writer's commit advanced the published version
		t.Fatalf("version = %d, want 2", db.Version())
	}
	fresh := db.ReadSession()
	defer fresh.Close()
	if got := readCount(t, fresh); got != 2 { // a fresh reader sees both rows
		t.Fatalf("fresh reader count = %d, want 2", got)
	}
}

func TestSharedManyReadersParallelWithWriter(t *testing.T) {
	// Fan out reader goroutines while a writer goroutine commits repeatedly. Each reader pins a
	// consistent snapshot (a count that never changes mid-read) and never blocks. Run under -race;
	// the assertion is that every reader observes an internally-consistent snapshot.
	db := seeded(t, 1)

	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		for i := int64(2); i <= 20; i++ {
			w := db.WriteSession()
			if _, err := w.Execute(fmt.Sprintf("INSERT INTO t VALUES (%d)", i), nil); err != nil {
				t.Errorf("writer insert %d: %v", i, err)
				return
			}
			if err := w.Commit(); err != nil {
				t.Errorf("writer commit %d: %v", i, err)
				return
			}
		}
	}()

	for g := 0; g < 8; g++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for n := 0; n < 50; n++ {
				r := db.ReadSession()
				first := readCount(t, r)
				second := readCount(t, r) // same pinned snapshot ⇒ identical
				if first != second {
					t.Errorf("a pinned snapshot changed mid-read: %d != %d", first, second)
				}
				if first < 1 || first > 20 {
					t.Errorf("count %d out of expected range", first)
				}
				r.Close()
			}
		}()
	}

	wg.Wait()
	// The seed committed once (version 1); the writer committed 19 more times (ids 2..=20).
	if db.Version() != 20 {
		t.Fatalf("version = %d, want 20", db.Version())
	}
	r := db.ReadSession()
	defer r.Close()
	if got := readCount(t, r); got != 20 {
		t.Fatalf("final count = %d, want 20", got)
	}
}

func TestSharedOldestLiveTxidTracksPinnedReaders(t *testing.T) {
	db := seeded(t, 1) // version 1
	if db.Version() != 1 {
		t.Fatalf("version = %d, want 1", db.Version())
	}
	if db.OldestLiveTxid() != 1 { // no readers ⇒ the committed version
		t.Fatalf("oldest (no readers) = %d, want 1", db.OldestLiveTxid())
	}

	r1 := db.ReadSession() // pins version 1
	if db.OldestLiveTxid() != 1 {
		t.Fatalf("oldest (r1 pinned v1) = %d, want 1", db.OldestLiveTxid())
	}

	w := db.WriteSession()
	if _, err := w.Execute("INSERT INTO t VALUES (2)", nil); err != nil {
		t.Fatalf("insert: %v", err)
	}
	if err := w.Commit(); err != nil { // version 2
		t.Fatalf("commit: %v", err)
	}
	if db.Version() != 2 {
		t.Fatalf("version = %d, want 2", db.Version())
	}
	if db.OldestLiveTxid() != 1 { // r1 still pins v1 ⇒ watermark held at 1
		t.Fatalf("oldest (r1 still pinned) = %d, want 1", db.OldestLiveTxid())
	}

	r2 := db.ReadSession() // pins version 2
	if db.OldestLiveTxid() != 1 {
		t.Fatalf("oldest (r1+r2) = %d, want 1", db.OldestLiveTxid())
	}

	r1.Close()
	if db.OldestLiveTxid() != 2 { // r1 gone ⇒ watermark advances to r2's version
		t.Fatalf("oldest (r1 closed) = %d, want 2", db.OldestLiveTxid())
	}

	r2.Close()
	if db.OldestLiveTxid() != 2 { // no readers ⇒ the committed version
		t.Fatalf("oldest (none) = %d, want 2", db.OldestLiveTxid())
	}
}

func TestSharedRolledBackWriterPublishesNothing(t *testing.T) {
	db := seeded(t, 1)
	w := db.WriteSession()
	if _, err := w.Execute("INSERT INTO t VALUES (2)", nil); err != nil {
		t.Fatalf("insert: %v", err)
	}
	if err := w.Rollback(); err != nil {
		t.Fatalf("rollback: %v", err)
	}
	r := db.ReadSession()
	defer r.Close()
	if got := readCount(t, r); got != 1 { // the rolled-back insert never became visible
		t.Fatalf("count after rollback = %d, want 1", got)
	}
	if db.Version() != 1 { // version unchanged by a rollback
		t.Fatalf("version after rollback = %d, want 1", db.Version())
	}

	// A second writer can proceed after the first rolled back (the gate was released).
	w2 := db.WriteSession()
	if _, err := w2.Execute("INSERT INTO t VALUES (3)", nil); err != nil {
		t.Fatalf("second insert: %v", err)
	}
	if err := w2.Commit(); err != nil {
		t.Fatalf("second commit: %v", err)
	}
	r2 := db.ReadSession()
	defer r2.Close()
	if got := readCount(t, r2); got != 2 {
		t.Fatalf("count after second commit = %d, want 2", got)
	}
}
