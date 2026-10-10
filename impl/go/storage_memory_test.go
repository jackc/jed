package jed

// Host-API surface of the committed-storage limit (spec/design/memory.md §8): the create, open, and
// attach options, the runtime setter and the StorageBytes gauge, the 42704 rejections, the multi-root
// precheck, the reader watermark's hold on forced compaction (in-memory, Q4a), and the file form's
// live-page measure (§8.7). Trip points themselves are pinned in
// spec/conformance/suites/resource/storage_memory.test and storage_file.test.

import (
	"context"
	"fmt"
	"path/filepath"
	"strings"
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

// storageFileDB creates a fresh file-backed database with the live-page self-check on (memory.md §8.7):
// every commit in these tests also recounts its live pages by reachability and panics on a mismatch.
func storageFileDB(t *testing.T, maxStorageBytes int64) (*Database, string) {
	t.Helper()
	SetVerifyLivePages(true)
	t.Cleanup(func() { SetVerifyLivePages(false) })
	path := filepath.Join(t.TempDir(), "storage.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, SkipFsync: true, MaxStorageBytes: maxStorageBytes})
	if err != nil {
		t.Fatal(err)
	}
	return db, path
}

func TestFileStorageLimitMeasuresLivePages(t *testing.T) {
	db, path := storageFileDB(t, 0)
	// A fresh file holds one live page: its catalog.
	if got := mustStorageBytes(t, db, "main"); got != storagePage {
		t.Fatalf("fresh file storage %d, want %d", got, storagePage)
	}
	s := db.Session(SessionOptions{})
	storageExec(t, s, "CREATE TABLE t (id i32 PRIMARY KEY, v text)")
	storageExec(t, s, storageBatch(0, 200))
	full := mustStorageBytes(t, db, "main")
	if full <= storagePage || full%storagePage != 0 {
		t.Fatalf("full storage %d", full)
	}
	// A delete lowers the measure at once; the file keeps its high-water and free pages.
	highWater := db.PageCount()
	storageExec(t, s, "DELETE FROM t WHERE id >= 100")
	half := mustStorageBytes(t, db, "main")
	if half >= full {
		t.Fatalf("storage after delete %d, want < %d", half, full)
	}
	if db.PageCount() < highWater {
		t.Fatalf("page count %d fell below the high-water %d", db.PageCount(), highWater)
	}

	// The setter limits the file from the next commit; a rejected commit writes nothing.
	if err := db.SetMaxStorageBytes("main", half); err != nil {
		t.Fatal(err)
	}
	if code := storageExecCode(s, storageBatch(100, 100)); code != "54P06" {
		t.Fatalf("growth past the limit: want 54P06, got %q", code)
	}
	if got := mustStorageBytes(t, db, "main"); got != half {
		t.Fatalf("a rejected commit writes nothing: storage %d, want %d", got, half)
	}
	// A commit that does not grow the live count is admitted at the limit.
	storageExec(t, s, "UPDATE t SET v = 'short' WHERE id < 10")
	var n int64
	if err := s.QueryRow(context.Background(), "SELECT count(*) FROM t").Scan(&n); err != nil {
		t.Fatal(err)
	}
	if n != 100 {
		t.Fatalf("t holds %d rows, want 100", n)
	}
	measured := mustStorageBytes(t, db, "main")
	s.Close()
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	// The count is persisted: a reopen reports it without walking the file, and the open option sets
	// the limit.
	db, err := OpenDatabaseWithOptions(path, OpenOptions{SkipFsync: true, MaxStorageBytes: measured})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	if got := mustStorageBytes(t, db, "main"); got != measured {
		t.Fatalf("reopened storage %d, want %d", got, measured)
	}
	s = db.Session(SessionOptions{})
	defer s.Close()
	if code := storageExecCode(s, storageBatch(100, 100)); code != "54P06" {
		t.Fatalf("open-option limit: want 54P06, got %q", code)
	}
	if err := db.SetMaxStorageBytes("main", 0); err != nil {
		t.Fatal(err)
	}
	storageExec(t, s, storageBatch(100, 100))
}

func TestFileStorageLimitCountsEveryLiveStructure(t *testing.T) {
	// Indexes (B-tree, GIN, GiST), overflow chains, a drop, and a host compaction, each checked by the
	// reachability recount.
	db, _ := storageFileDB(t, 0)
	defer db.Close()
	s := db.Session(SessionOptions{})
	defer s.Close()
	storageExec(t, s, "CREATE TABLE t (id i32 PRIMARY KEY, k i32, v text, a i32[], r i32range)")
	storageExec(t, s, "CREATE INDEX t_k ON t (k)")
	storageExec(t, s, "CREATE INDEX t_a ON t USING gin (a)")
	storageExec(t, s, "CREATE INDEX t_r ON t USING gist (r)")
	// An incompressible value larger than a record spills into its own overflow chain per row.
	x := uint32(0x4A454442)
	filler := make([]byte, 5000)
	for i := range filler {
		x ^= x << 13
		x ^= x >> 17
		x ^= x << 5
		filler[i] = 'A' + byte(x%26)
	}
	if _, err := s.Exec(context.Background(), "INSERT INTO t VALUES (0, 0, $1, '{0,1}', '[0,5)')", string(filler)); err != nil {
		t.Fatal(err)
	}
	rows := make([]string, 0, 300)
	for g := 1; g <= 300; g++ {
		rows = append(rows, fmt.Sprintf("(%d, '{%d,%d}', '[%d,%d)')", g, g, g+1, g, g+5))
	}
	storageExec(t, s, "INSERT INTO t (id, a, r) VALUES "+strings.Join(rows, ", "))
	storageExec(t, s, "UPDATE t SET k = id % 7, v = (SELECT v FROM t WHERE id = 0)")
	full := mustStorageBytes(t, db, "main")
	storageExec(t, s, "UPDATE t SET v = left(v, 10) WHERE id % 3 = 0")
	storageExec(t, s, "DELETE FROM t WHERE id > 150")
	if got := mustStorageBytes(t, db, "main"); got >= full {
		t.Fatalf("storage after shrinking %d, want < %d", got, full)
	}
	storageExec(t, s, "CREATE TABLE u (id i32 PRIMARY KEY, v text)")
	storageExec(t, s, "INSERT INTO u SELECT id, v FROM t")
	storageExec(t, s, "DROP TABLE t")
	before := mustStorageBytes(t, db, "main")
	if err := db.Compact("main"); err != nil {
		t.Fatal(err)
	}
	// Compaction renumbers pages but keeps every live one.
	if got := mustStorageBytes(t, db, "main"); got != before {
		t.Fatalf("storage after compaction %d, want %d", got, before)
	}
	storageExec(t, s, "DROP TABLE u")
	if got := mustStorageBytes(t, db, "main"); got != storagePage {
		t.Fatalf("storage after dropping everything %d, want %d", got, storagePage)
	}
}

func TestFileAttachmentStorageLimit(t *testing.T) {
	file, path := storageFileDB(t, 0)
	if _, err := file.Exec(context.Background(), "CREATE TABLE a (id i32 PRIMARY KEY, v text)"); err != nil {
		t.Fatal(err)
	}
	if err := file.Close(); err != nil {
		t.Fatal(err)
	}
	host := memDB()
	defer host.Close()
	if err := host.Attach("f", AttachFile(path).WithMaxStorageBytes(4*storagePage), false); err != nil {
		t.Fatal(err)
	}
	s := host.Session(SessionOptions{})
	storageExec(t, s, "CREATE TABLE t (id i32 PRIMARY KEY, v text)")
	main := mustStorageBytes(t, host, "main")
	aux := mustStorageBytes(t, host, "f")
	// A multi-root commit rejected in the file attachment publishes neither database.
	storageExec(t, s, "BEGIN")
	storageExec(t, s, storageBatch(0, 10))
	storageExec(t, s, "INSERT INTO f.a SELECT g, repeat('y', 1000) FROM generate_series(1, 100) g")
	if code := storageExecCode(s, "COMMIT"); code != "54P06" {
		t.Fatalf("multi-root commit: want 54P06, got %q", code)
	}
	if got := mustStorageBytes(t, host, "main"); got != main {
		t.Fatalf("main storage %d, want %d (unchanged)", got, main)
	}
	if got := mustStorageBytes(t, host, "f"); got != aux {
		t.Fatalf("f storage %d, want %d (unchanged)", got, aux)
	}
	var nt, na int64
	if err := s.QueryRow(context.Background(), "SELECT (SELECT count(*) FROM t), (SELECT count(*) FROM f.a)").Scan(&nt, &na); err != nil {
		t.Fatal(err)
	}
	if nt != 0 || na != 0 {
		t.Fatalf("rows after the rejected commit: t %d, f.a %d, want 0 and 0", nt, na)
	}
	// A small write fits.
	storageExec(t, s, "INSERT INTO f.a VALUES (1, 'z')")
	if got := mustStorageBytes(t, host, "f"); got > 4*storagePage {
		t.Fatalf("f storage %d, want <= %d", got, 4*storagePage)
	}
	s.Close()
	if err := host.Detach("f"); err != nil {
		t.Fatal(err)
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
