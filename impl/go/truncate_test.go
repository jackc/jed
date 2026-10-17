package jed

// In-place trailing-free truncation (spec/fileformat/format.md *Trailing truncation*, storage.md §6).
// A file commit whose top pages are all on the free list lowers the page high-water and, after its
// barrier, shortens the file to one growth step past it. Page counts, file lengths, and the reader
// watermark are host/internal state the corpus cannot see (CLAUDE.md §10). Mirrors
// impl/rust/tests/truncate.rs and impl/ts/tests/truncate.test.ts.

import (
	"fmt"
	"os"
	"path/filepath"
	"testing"
)

// churn runs n single-row updates of t, each its own durable commit.
func churn(t *testing.T, db *Database, n int, tag string) {
	t.Helper()
	s := db.Session(SessionOptions{})
	defer s.Close()
	for k := 0; k < n; k++ {
		sql := fmt.Sprintf("UPDATE t SET pad = '%s%d' || repeat('y', 200), n = n + 7 WHERE id = %d", tag, k, 1+k%100)
		if _, err := queryOutcome(s, sql, nil); err != nil {
			t.Fatalf("%q: %v", sql, err)
		}
	}
}

// fileLength is the database file's length in bytes.
func fileLength(t *testing.T, path string) int64 {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	return info.Size()
}

func TestTrailingTruncationShrinksAFileAfterADelete(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "truncate.jed")
	db := createCompactFile(t, path)
	growAndShrink(t, db)
	peak := db.PageCount()
	peakLength := fileLength(t, path)
	churn(t, db, 600, "a")
	pc := db.PageCount()
	if pc*3 > peak {
		t.Fatalf("trailing truncation should return the dead top of the file: %d pages at peak, %d after churn", peak, pc)
	}
	limit := int64(pc+growthStep(pc, compactPage)) * compactPage
	if length := fileLength(t, path); length > limit || length >= peakLength {
		t.Fatalf("file is %d bytes after truncation to %d pages (limit %d, peak %d)", length, pc, limit, peakLength)
	}
	st := db.core.storage
	st.mu.Lock()
	for _, p := range st.freePages {
		if p >= st.pageCount {
			t.Errorf("free page %d at or above the high-water %d", p, st.pageCount)
		}
	}
	st.mu.Unlock()
	rows, cost := rowsAndCost(t, db, compactProbe)
	count := countRows(t, db, "t")
	db.Close()

	// The truncated file reopens to the same high-water, rows, and costs.
	re, err := OpenDatabase(path)
	if err != nil {
		t.Fatalf("reopen: %v", err)
	}
	defer re.Close()
	if re.PageCount() != pc {
		t.Fatalf("reopened page count %d, want %d", re.PageCount(), pc)
	}
	rows2, cost2 := rowsAndCost(t, re, compactProbe)
	if fmt.Sprint(rows2) != fmt.Sprint(rows) || cost2 != cost || countRows(t, re, "t") != count {
		t.Fatalf("rows/cost changed across reopen: %v/%d vs %v/%d", rows2, cost2, rows, cost)
	}
}

func TestTrailingTruncationWaitsForAnOlderReader(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "truncate-pinned.jed")
	db := createCompactFile(t, path)
	defer db.Close()
	growAndShrink(t, db)
	// A reader pinned at the post-delete version keeps every page of that version, so the free list
	// may not be reused and nothing is truncated while it is open: the high-water never goes down.
	reader := db.ReadSession()
	high := db.PageCount()
	s := db.Session(SessionOptions{})
	for k := 0; k < 400; k++ {
		if _, err := queryOutcome(s, fmt.Sprintf("UPDATE t SET n = %d WHERE id = %d", k, 1+k%100), nil); err != nil {
			t.Fatal(err)
		}
		if pc := db.PageCount(); pc < high {
			t.Fatalf("commit %d lowered the high-water from %d to %d under a pinned reader", k, high, pc)
		} else {
			high = pc
		}
	}
	reader.Close()
	s.Close()
	churn(t, db, 600, "b")
	if db.PageCount() >= high {
		t.Fatalf("after the reader closed, truncation should resume: %d pages, %d before", db.PageCount(), high)
	}
	if got := countRows(t, db, "t"); got != 100 {
		t.Fatalf("count %d", got)
	}
}

// pendingCount is the main file's pending-set size: non-zero while a relaxed batch awaits its flush. It
// first waits for a background cap flush, which the next commit would wait for too.
func pendingCount(db *Database) int {
	st := db.core.storage
	st.mu.Lock()
	defer st.mu.Unlock()
	_ = st.awaitFlush()
	n := 0
	_ = st.paging.withPager(func(p *pager) error { n = len(p.pending); return nil })
	return n
}

// Under synchronous = off only a commit that begins with nothing pending lowers the high-water, and
// the file is shortened only at the barrier that makes the lowering generation durable — the batch's
// cap flush or close — never while the batch is pending (validated-cow.md §4). A table dropped at the
// top of the file leaves a long free run there once a flush lets reclamation list its pages; until
// then the relaxed churn grows the file past it.
func TestTrailingTruncationWaitsForTheFlushUnderSynchronousOff(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "truncate-relaxed.jed")
	db := createCompactFile(t, path)
	s := db.Session(SessionOptions{})
	for _, sql := range []string{
		"CREATE TABLE t (id i64 PRIMARY KEY, pad text, n i64)",
		"INSERT INTO t SELECT g, repeat('x', 300), g % 7 FROM generate_series(1, 100) g",
		"CREATE TABLE big (id i64 PRIMARY KEY, pad text)",
		"INSERT INTO big SELECT g, repeat('z', 300) FROM generate_series(1, 20000) g",
	} {
		if _, err := queryOutcome(s, sql, nil); err != nil {
			t.Fatalf("%q: %v", sql, err)
		}
	}
	s.Close()
	db.Close()
	db, err := OpenDatabaseWithOptions(path, OpenOptions{Synchronous: SynchronousOff})
	if err != nil {
		t.Fatal(err)
	}
	peak := db.PageCount()
	s = db.Session(SessionOptions{})
	if _, err := queryOutcome(s, "DROP TABLE big", nil); err != nil {
		t.Fatal(err)
	}
	deferred := false
	for k := 0; k < 1500; k++ {
		before, high, pendingBefore := fileLength(t, path), db.PageCount(), pendingCount(db)
		sql := fmt.Sprintf("UPDATE t SET pad = 'r%d' || repeat('y', 200), n = n + 7 WHERE id = %d", k, 1+k%100)
		if _, err := queryOutcome(s, sql, nil); err != nil {
			t.Fatalf("%q: %v", sql, err)
		}
		pc, length, pending := db.PageCount(), fileLength(t, path), pendingCount(db)
		peak = max(peak, pc)
		if pendingBefore > 0 && pc < high {
			t.Fatalf("commit %d lowered the high-water from %d to %d inside a pending batch", k, high, pc)
		}
		if pending > 0 && length < before {
			t.Fatalf("commit %d shortened the file from %d to %d bytes while its batch is pending", k, before, length)
		}
		if pending > 0 && pc < high && length > int64(pc+growthStep(pc, compactPage))*compactPage {
			deferred = true // lowered now, shortened at the batch's flush
		}
	}
	s.Close()
	pc := db.PageCount()
	if pc*3 > peak {
		t.Fatalf("trailing truncation should return the dropped table's pages: %d pages at the peak, %d after churn", peak, pc)
	}
	if !deferred {
		t.Fatal("no relaxed commit lowered the high-water with its shrink left to the flush")
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	if limit := int64(pc+growthStep(pc, compactPage)) * compactPage; fileLength(t, path) > limit {
		t.Fatalf("close flushed the batch but left the file %d bytes (at most %d)", fileLength(t, path), limit)
	}
	re, err := OpenDatabase(path)
	if err != nil {
		t.Fatalf("reopen: %v", err)
	}
	defer re.Close()
	if re.PageCount() != pc || countRows(t, re, "t") != 100 {
		t.Fatalf("reopened page count %d (want %d), %d rows", re.PageCount(), pc, countRows(t, re, "t"))
	}
}
