package jed

// Host-API surface of the committed-storage limit (spec/design/memory.md §8, Q4a): the create and
// attach options, the runtime setter and the StorageBytes gauge, the 0A000/42704 rejections, the
// multi-root precheck, and the reader watermark's hold on forced compaction. Trip points themselves
// are pinned in spec/conformance/suites/resource/storage_memory.test.

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"testing"
)

const storagePage int64 = 8192

func storageBatch(lo, n int) string {
	return fmt.Sprintf("INSERT INTO t SELECT g, repeat('x', 1000) FROM generate_series(%d, %d) g", lo, lo+n-1)
}

// storageExec runs sql on s, failing the test on error.
func storageExec(t *testing.T, s *Session, sql string) {
	t.Helper()
	if _, err := s.Exec(context.Background(), sql); err != nil {
		t.Fatalf("exec %q: %v", sql, err)
	}
}

// storageExecCode runs sql on s and returns the error's SQLSTATE ("" on success).
func storageExecCode(s *Session, sql string) string {
	_, err := s.Exec(context.Background(), sql)
	return errCodeOf(err)
}

func mustStorageBytes(t *testing.T, db *Database, name string) int64 {
	t.Helper()
	n, err := db.StorageBytes(name)
	if err != nil {
		t.Fatalf("StorageBytes(%q): %v", name, err)
	}
	return n
}

func TestStorageLimitOptionsAndGauge(t *testing.T) {
	db := memDB()
	fresh := mustStorageBytes(t, db, "main")
	if fresh <= 0 || fresh%storagePage != 0 {
		t.Fatalf("a fresh image holds its meta and catalog pages: %d", fresh)
	}
	s := db.Session(SessionOptions{})
	storageExec(t, s, "CREATE TABLE t (id i32 PRIMARY KEY, v text)")
	// Unlimited by default: a large insert commits.
	storageExec(t, s, storageBatch(0, 200))
	full := mustStorageBytes(t, db, "MAIN")
	if full <= fresh {
		t.Fatalf("storage %d did not grow past %d", full, fresh)
	}
	// Lowering the limit below the current size blocks growth, not the database.
	if err := db.SetMaxStorageBytes("main", fresh); err != nil {
		t.Fatal(err)
	}
	if code := storageExecCode(s, storageBatch(200, 200)); code != "54P06" {
		t.Fatalf("growth past the limit: want 54P06, got %q", code)
	}
	if got := mustStorageBytes(t, db, "main"); got != full {
		t.Fatalf("a rejected commit writes nothing: storage %d, want %d", got, full)
	}
	if err := db.SetMaxStorageBytes("main", -1); err != nil {
		t.Fatal(err)
	}
	storageExec(t, s, storageBatch(200, 200))

	// The create option limits from the first commit.
	db, err := CreateDatabase(CreateOptions{MaxStorageBytes: 8 * storagePage})
	if err != nil {
		t.Fatal(err)
	}
	s = db.Session(SessionOptions{})
	storageExec(t, s, "CREATE TABLE t (id i32 PRIMARY KEY, v text)")
	if code := storageExecCode(s, storageBatch(0, 200)); code != "54P06" {
		t.Fatalf("create-option limit: want 54P06, got %q", code)
	}

	// An in-memory attachment's limit, set on attach or later.
	if err := db.Attach("aux", AttachMemory().WithMaxStorageBytes(storagePage), false); err != nil {
		t.Fatal(err)
	}
	if code := storageExecCode(s, "CREATE TABLE aux.a (id i32 PRIMARY KEY, v text)"); code != "54P06" {
		t.Fatalf("attach-option limit: want 54P06, got %q", code)
	}
	if err := db.SetMaxStorageBytes("aux", 0); err != nil {
		t.Fatal(err)
	}
	storageExec(t, s, "CREATE TABLE aux.a (id i32 PRIMARY KEY, v text)")
	if got := mustStorageBytes(t, db, "aux"); got <= storagePage {
		t.Fatalf("aux storage %d, want > %d", got, storagePage)
	}

	// Unknown databases and the session-local temp domain are not attachments.
	if _, err := db.StorageBytes("nope"); errCodeOf(err) != "42704" {
		t.Fatalf("unknown database: want 42704, got %v", err)
	}
	if err := db.SetMaxStorageBytes("temp", storagePage); errCodeOf(err) != "42704" {
		t.Fatalf("temp: want 42704, got %v", err)
	}
}

func TestStorageLimitRejectsFileBackings(t *testing.T) {
	path := filepath.Join(t.TempDir(), "storage.jed")
	_, err := CreateDatabase(CreateOptions{Path: path, MaxStorageBytes: storagePage})
	if errCodeOf(err) != "0A000" {
		t.Fatalf("file create with a limit: want 0A000, got %v", err)
	}
	if _, statErr := os.Stat(path); !os.IsNotExist(statErr) {
		t.Fatalf("a rejected create makes no file: %v", statErr)
	}

	db, err := CreateDatabase(CreateOptions{Path: path, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	if err := db.SetMaxStorageBytes("main", storagePage); errCodeOf(err) != "0A000" {
		t.Fatalf("file setter: want 0A000, got %v", err)
	}
	if err := db.SetMaxStorageBytes("main", 0); err != nil {
		t.Fatal(err)
	}
	if got := mustStorageBytes(t, db, "main"); got <= 0 {
		t.Fatalf("file storage %d, want > 0", got)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	host := memDB()
	if err := host.Attach("f", AttachFile(path).WithMaxStorageBytes(storagePage), false); errCodeOf(err) != "0A000" {
		t.Fatalf("file attach with a limit: want 0A000, got %v", err)
	}
}

func TestStorageLimitMultiRootRejectionPacksNoAttachment(t *testing.T) {
	db := memDB()
	if err := db.Attach("aux", AttachMemory(), false); err != nil {
		t.Fatal(err)
	}
	s := db.Session(SessionOptions{})
	storageExec(t, s, "CREATE TABLE t (id i32 PRIMARY KEY, v text)")
	storageExec(t, s, "CREATE TABLE aux.a (id i32 PRIMARY KEY, v text)")
	main := mustStorageBytes(t, db, "main")
	aux := mustStorageBytes(t, db, "aux")
	if err := db.SetMaxStorageBytes("main", main); err != nil {
		t.Fatal(err)
	}
	storageExec(t, s, "BEGIN")
	storageExec(t, s, "INSERT INTO aux.a SELECT g, repeat('y', 1000) FROM generate_series(1, 50) g")
	storageExec(t, s, storageBatch(0, 50))
	if code := storageExecCode(s, "COMMIT"); code != "54P06" {
		t.Fatalf("multi-root commit: want 54P06, got %q", code)
	}
	// Main is rejected after the attachment's precheck, before any domain packs a page.
	if got := mustStorageBytes(t, db, "aux"); got != aux {
		t.Fatalf("aux storage %d, want %d (unchanged)", got, aux)
	}
	if got := mustStorageBytes(t, db, "main"); got != main {
		t.Fatalf("main storage %d, want %d (unchanged)", got, main)
	}
	var n int64
	if err := s.QueryRow(context.Background(), "SELECT count(*) FROM aux.a").Scan(&n); err != nil {
		t.Fatal(err)
	}
	if n != 0 {
		t.Fatalf("aux.a holds %d rows, want 0", n)
	}
}

func TestStorageLimitPinnedReaderBlocksForcedCompaction(t *testing.T) {
	db := memDB()
	w := db.Session(SessionOptions{})
	storageExec(t, w, "CREATE TABLE t (id i32 PRIMARY KEY, v text)")
	storageExec(t, w, storageBatch(0, 100))
	if err := db.SetMaxStorageBytes("main", mustStorageBytes(t, db, "main")); err != nil {
		t.Fatal(err)
	}
	// A reader pinned at the current version keeps every page a compaction would keep: the delete
	// (admitted over the limit) orphans pages, and the next commit's forced compaction is allowed only
	// once no reader pins a version older than the committed one.
	r := db.ReadSession()
	storageExec(t, w, "DELETE FROM t WHERE id >= 50")
	if code := storageExecCode(w, storageBatch(50, 50)); code != "54P06" {
		t.Fatalf("pinned reader: want 54P06, got %q", code)
	}
	r.Close()
	storageExec(t, w, storageBatch(50, 50))
}
