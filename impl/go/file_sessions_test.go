package jed

// Slice 7c — file-backed sessions + the default-session bridge (spec/design/session.md §2.4/§10).
// These per-core tests cover what the corpus cannot express (host-API surface + concurrency +
// on-disk durability): that CreateDatabase/OpenDatabase return the shared core with a stateful
// default session whose autocommit writes persist durably and survive a reopen; that file-backed
// read sessions fault pages concurrently with a committing writer while staying snapshot-isolated;
// and that a read-only open rejects writes (25006). The logical transaction/visibility semantics
// stay in the shared concurrency corpus (suites/concurrency/).

import (
	"fmt"
	"path/filepath"
	"sync"
	"testing"
)

func countVia(t *testing.T, db *Database) int64 {
	t.Helper()
	out, err := db.Execute("SELECT count(*) FROM t", nil)
	if err != nil {
		t.Fatalf("count query: %v", err)
	}
	return out.Rows[0][0].Int
}

func TestFileBackedRoundtripAndReopen(t *testing.T) {
	path := filepath.Join(t.TempDir(), "file_sessions_roundtrip.jed")
	func() {
		db, err := CreateDatabase(CreateOptions{Path: path})
		if err != nil {
			t.Fatalf("create: %v", err)
		}
		defer db.Close()
		if db.Version() != 1 {
			t.Fatalf("fresh version = %d, want 1", db.Version())
		}
		if _, err := db.Execute("CREATE TABLE t (id i64 PRIMARY KEY)", nil); err != nil {
			t.Fatalf("create table: %v", err)
		}
		if db.Version() != 2 {
			t.Fatalf("after CREATE version = %d, want 2", db.Version())
		}
		for i := 1; i <= 5; i++ {
			if _, err := db.Execute(fmt.Sprintf("INSERT INTO t VALUES (%d)", i), nil); err != nil {
				t.Fatalf("insert %d: %v", i, err)
			}
		}
		if got := countVia(t, db); got != 5 {
			t.Fatalf("count = %d, want 5", got)
		}
	}()

	db, err := OpenDatabase(path)
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	defer db.Close()
	if got := countVia(t, db); got != 5 {
		t.Fatalf("reopened count = %d, want 5", got)
	}
	if db.Version() != 7 { // 1 (create) + 1 (CREATE TABLE) + 5 (inserts)
		t.Fatalf("reopened version = %d, want 7", db.Version())
	}
}

func TestFileBackedExplicitTransactionPersistsThenRollsBack(t *testing.T) {
	path := filepath.Join(t.TempDir(), "file_sessions_explicit_tx.jed")
	func() {
		db, err := CreateDatabase(CreateOptions{Path: path})
		if err != nil {
			t.Fatalf("create: %v", err)
		}
		defer db.Close()
		// Explicit transactions live on a Session (the persistent default-session bridge was removed
		// from Database): mint one over the file-backed core and drive BEGIN/COMMIT/ROLLBACK on it.
		s := db.Session(SessionOptions{})
		defer s.Close()
		if _, err := s.Execute("CREATE TABLE t (id i64 PRIMARY KEY)", nil); err != nil {
			t.Fatalf("create table: %v", err)
		}
		// A committed explicit block is durable.
		if err := s.Begin(true); err != nil {
			t.Fatalf("begin: %v", err)
		}
		if _, err := s.Execute("INSERT INTO t VALUES (1)", nil); err != nil {
			t.Fatalf("insert 1: %v", err)
		}
		if _, err := s.Execute("INSERT INTO t VALUES (2)", nil); err != nil {
			t.Fatalf("insert 2: %v", err)
		}
		if err := s.Commit(); err != nil {
			t.Fatalf("commit: %v", err)
		}
		if got := readCount(t, s); got != 2 {
			t.Fatalf("after commit count = %d, want 2", got)
		}
		// A rolled-back block leaves nothing.
		if err := s.Begin(true); err != nil {
			t.Fatalf("begin2: %v", err)
		}
		if _, err := s.Execute("INSERT INTO t VALUES (3)", nil); err != nil {
			t.Fatalf("insert 3: %v", err)
		}
		if err := s.Rollback(); err != nil {
			t.Fatalf("rollback: %v", err)
		}
		if got := readCount(t, s); got != 2 {
			t.Fatalf("after rollback count = %d, want 2", got)
		}
	}()

	db, err := OpenDatabase(path)
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	defer db.Close()
	if got := countVia(t, db); got != 2 {
		t.Fatalf("reopened count = %d, want 2", got)
	}
}

func TestFileBackedExecuteScriptIsAllOrNothing(t *testing.T) {
	path := filepath.Join(t.TempDir(), "file_sessions_script.jed")
	func() {
		db, err := CreateDatabase(CreateOptions{Path: path})
		if err != nil {
			t.Fatalf("create: %v", err)
		}
		defer db.Close()
		summary, err := db.ExecuteScript(
			"CREATE TABLE t (id i64 PRIMARY KEY); INSERT INTO t VALUES (1); INSERT INTO t VALUES (2);",
		)
		if err != nil {
			t.Fatalf("script: %v", err)
		}
		if summary.StatementsRun != 3 {
			t.Fatalf("statements_run = %d, want 3", summary.StatementsRun)
		}
		if got := countVia(t, db); got != 2 {
			t.Fatalf("count = %d, want 2", got)
		}
	}()

	db, err := OpenDatabase(path)
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	defer db.Close()
	if got := countVia(t, db); got != 2 {
		t.Fatalf("reopened count = %d, want 2", got)
	}
}

func TestFileBackedReadOnlyOpenRejectsWrites(t *testing.T) {
	path := filepath.Join(t.TempDir(), "file_sessions_read_only.jed")
	func() {
		db, err := CreateDatabase(CreateOptions{Path: path})
		if err != nil {
			t.Fatalf("create: %v", err)
		}
		defer db.Close()
		execDB(t, db, "CREATE TABLE t (id i64 PRIMARY KEY)")
		execDB(t, db, "INSERT INTO t VALUES (1)")
	}()

	db, err := OpenDatabaseWithOptions(path, OpenOptions{ReadOnly: true})
	if err != nil {
		t.Fatalf("open read-only: %v", err)
	}
	defer db.Close()
	if got := countVia(t, db); got != 1 {
		t.Fatalf("count = %d, want 1", got)
	}
	if _, err := db.Execute("INSERT INTO t VALUES (2)", nil); err == nil || errCodeOf(err) != "25006" {
		t.Fatalf("write on read-only handle: got %v, want 25006", err)
	}
	// A read/write session minted from a read-only core also rejects writes.
	w := db.WriteSession()
	defer w.Close()
	if _, err := w.Execute("INSERT INTO t VALUES (3)", nil); err == nil || errCodeOf(err) != "25006" {
		t.Fatalf("write via session on read-only core: got %v, want 25006", err)
	}
}

func TestFileBackedReadersRunConcurrentlyWithAWriter(t *testing.T) {
	// The deep 7c requirement: file-backed read sessions fault clean pages through the shared,
	// mutex-guarded buffer pool concurrently with a writer committing (and persisting dirty pages) on
	// another goroutine. Each reader pins a snapshot and must see an internally consistent count;
	// reclamation stays trivially watermark-safe (reconstruct-on-open free-list). Run under `go test
	// -race` (rake concurrency:race walks the threaded conformance; this exercises the file pager).
	path := filepath.Join(t.TempDir(), "file_sessions_concurrent.jed")
	func() {
		// Small pages so the table spans several leaves (real faults).
		db, err := CreateDatabase(CreateOptions{Path: path, PageSize: 256})
		if err != nil {
			t.Fatalf("create: %v", err)
		}
		defer db.Close()
		execDB(t, db, "CREATE TABLE t (id i64 PRIMARY KEY)")
		execDB(t, db, "INSERT INTO t VALUES (1)")
	}()

	db, err := OpenDatabaseWithOptions(path, OpenOptions{CacheBytes: 4 * 256}) // a handful of resident leaves
	if err != nil {
		t.Fatalf("open: %v", err)
	}
	defer db.Close()

	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		for i := 2; i <= 40; i++ {
			w := db.WriteSession()
			if _, err := w.Execute(fmt.Sprintf("INSERT INTO t VALUES (%d)", i), nil); err != nil {
				t.Errorf("writer insert %d: %v", i, err)
				w.Close()
				return
			}
			if err := w.Commit(); err != nil {
				t.Errorf("writer commit %d: %v", i, err)
			}
			w.Close()
		}
	}()

	for r := 0; r < 6; r++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for i := 0; i < 40; i++ {
				rd := db.ReadSession()
				first := readCount(t, rd)
				second := readCount(t, rd)
				rd.Close()
				if first != second {
					t.Errorf("a pinned snapshot changed mid-read: %d != %d", first, second)
					return
				}
				if first < 1 || first > 40 {
					t.Errorf("count %d out of range", first)
					return
				}
			}
		}()
	}
	wg.Wait()

	if db.Version() != 42 { // create v1 + CREATE TABLE v2 + seed insert v3 + 39 writer commits = v42
		t.Fatalf("version = %d, want 42", db.Version())
	}

	// Reopen from scratch: every committed row is durable on disk.
	db.Close()
	reopened, err := OpenDatabase(path)
	if err != nil {
		t.Fatalf("reopen: %v", err)
	}
	defer reopened.Close()
	if got := countVia(t, reopened); got != 40 {
		t.Fatalf("reopened count = %d, want 40", got)
	}
}

func execDB(t *testing.T, db *Database, sql string) {
	t.Helper()
	if _, err := db.Execute(sql, nil); err != nil {
		t.Fatalf("exec %q: %v", sql, err)
	}
}

// codeOf extracts the SQLSTATE from an engine error (readCount lives in shared_test.go).
func errCodeOf(err error) string {
	if ee, ok := err.(*EngineError); ok {
		return ee.Code()
	}
	return ""
}
