package jed

// Host compaction — Database.Compact (spec/design/api.md §2.6). Compaction is a host-API act the SQL
// corpus cannot reach, so its contract lives here (CLAUDE.md §10): the rewrite returns dead pages and
// equals the from-scratch image of the committed snapshot at the next version, rows/costs/prepared
// statements survive it, a reopen sees it, and every precondition fails 42704/25006/55006 without
// writing. Cross-process presence (another process holding the file) is covered by the shared process
// corpus (spec/conformance/process/compact.process.toml). Mirrors impl/rust/tests/compact.rs and
// impl/ts/tests/compact.test.ts.

import (
	"bytes"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"testing"
)

const compactPage = 4096

// compactProbe reads through the secondary index, so its cost covers index and table pages.
const compactProbe = "SELECT id, length(pad), n FROM t WHERE n = 3 ORDER BY id"

// createCompactFile creates a file database at path with the test page size and fsync off.
func createCompactFile(t *testing.T, path string) *Database {
	t.Helper()
	db, err := CreateDatabase(CreateOptions{Path: path, PageSize: compactPage, SkipFsync: true})
	if err != nil {
		t.Fatalf("create %s: %v", path, err)
	}
	return db
}

// growAndShrink grows table t to 2000 wide rows, then deletes all but 100: most pages end up dead.
func growAndShrink(t *testing.T, db *Database) {
	t.Helper()
	s := db.Session(SessionOptions{})
	defer s.Close()
	for _, sql := range []string{
		"CREATE TABLE t (id i64 PRIMARY KEY, pad text, n i64)",
		"CREATE INDEX t_n ON t (n)",
		"INSERT INTO t SELECT g, repeat('x', 300), g % 7 FROM generate_series(1, 2000) g",
		"DELETE FROM t WHERE id > 100",
	} {
		if _, err := queryOutcome(s, sql, nil); err != nil {
			t.Fatalf("%q: %v", sql, err)
		}
	}
}

// rowsAndCost returns the rows and the cost of sql on a fresh session.
func rowsAndCost(t *testing.T, db *Database, sql string) ([][]Value, int64) {
	t.Helper()
	s := db.Session(SessionOptions{})
	defer s.Close()
	out, err := queryOutcome(s, sql, nil)
	if err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
	return out.Rows, out.Cost
}

// compactCode returns the SQLSTATE of an error expected from Compact.
func compactCode(t *testing.T, err error) string {
	t.Helper()
	if err == nil {
		t.Fatalf("expected an error, got nil")
	}
	return err.(*EngineError).Code()
}

// countRows is SELECT count(*) FROM table, as an int.
func countRows(t *testing.T, db *Database, table string) int64 {
	t.Helper()
	rows, _ := rowsAndCost(t, db, "SELECT count(*) FROM "+table)
	return rows[0][0].Int
}

func TestFileCompactionReturnsDeadPagesAndKeepsRowsAndCosts(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "compact-file.jed")
	db := createCompactFile(t, path)
	growAndShrink(t, db)
	beforePages := db.PageCount()
	beforeVersion := db.Version()
	rows, cost := rowsAndCost(t, db, compactProbe)
	count := countRows(t, db, "t")

	if err := db.Compact("main"); err != nil {
		t.Fatalf("compact: %v", err)
	}

	afterPages := db.PageCount()
	if afterPages*4 >= beforePages {
		t.Fatalf("compaction returns dead pages: %d -> %d", beforePages, afterPages)
	}
	if db.Version() != beforeVersion+1 {
		t.Fatalf("version = %d, want %d", db.Version(), beforeVersion+1)
	}
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if info.Size() != int64(afterPages)*compactPage {
		t.Fatalf("file is %d bytes, want exactly its image %d (no preallocation slack)", info.Size(), int64(afterPages)*compactPage)
	}
	if _, err := os.Stat(path + ".jedtmp"); !os.IsNotExist(err) {
		t.Fatalf("temp file left behind: %v", err)
	}
	gotRows, gotCost := rowsAndCost(t, db, compactProbe)
	if !reflect.DeepEqual(gotRows, rows) || gotCost != cost {
		t.Fatalf("rows and cost changed: cost %d -> %d", cost, gotCost)
	}
	if got := countRows(t, db, "t"); got != count {
		t.Fatalf("count = %d, want %d", got, count)
	}

	// The compacted file is the from-scratch image of the committed snapshot at the new version.
	image, err := db.ToImage(compactPage, db.Version())
	if err != nil {
		t.Fatal(err)
	}
	file, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(file, image) {
		t.Fatalf("compacted file differs from ToImage at version %d", db.Version())
	}

	// Later commits build on it, and a reopen sees both.
	if _, err := queryOutcome(db, "INSERT INTO t VALUES (5000, 'tail', 3)", nil); err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	db, err = OpenDatabase(path)
	if err != nil {
		t.Fatalf("reopen: %v", err)
	}
	defer db.Close()
	reopened, _ := rowsAndCost(t, db, compactProbe)
	if len(reopened) != len(rows)+1 || !reflect.DeepEqual(reopened[:len(rows)], rows) {
		t.Fatalf("reopened rows = %v, want %v plus the tail row", reopened, rows)
	}
}

func TestCompactionOfACompactFileIsAFixedPoint(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "compact-fixed-point.jed")
	db := createCompactFile(t, path)
	defer db.Close()
	growAndShrink(t, db)
	if err := db.Compact("main"); err != nil {
		t.Fatal(err)
	}
	first, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if err := db.Compact("main"); err != nil {
		t.Fatal(err)
	}
	second, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if len(first) != len(second) {
		t.Fatalf("length changed: %d -> %d", len(first), len(second))
	}
	// Only the version (and so the meta checksums) moved.
	if !bytes.Equal(first[compactPage*2:], second[compactPage*2:]) {
		t.Fatal("body pages changed on a second compaction")
	}
}

func TestInMemoryCompactionLowersStorageBytes(t *testing.T) {
	t.Parallel()
	db, err := CreateDatabase(CreateOptions{PageSize: compactPage})
	if err != nil {
		t.Fatal(err)
	}
	growAndShrink(t, db)
	before, err := db.StorageBytes("main")
	if err != nil {
		t.Fatal(err)
	}
	rows, cost := rowsAndCost(t, db, compactProbe)
	version := db.Version()
	if err := db.Compact("MAIN"); err != nil {
		t.Fatal(err)
	}
	after, err := db.StorageBytes("main")
	if err != nil {
		t.Fatal(err)
	}
	if after*4 >= before {
		t.Fatalf("storage bytes %d -> %d, want a >4x shrink", before, after)
	}
	if db.Version() != version+1 {
		t.Fatalf("version = %d, want %d", db.Version(), version+1)
	}
	gotRows, gotCost := rowsAndCost(t, db, compactProbe)
	if !reflect.DeepEqual(gotRows, rows) || gotCost != cost {
		t.Fatalf("rows and cost changed: cost %d -> %d", cost, gotCost)
	}
	if _, err := queryOutcome(db, "INSERT INTO t VALUES (5000, 'tail', 3)", nil); err != nil {
		t.Fatal(err)
	}
	if got := countRows(t, db, "t"); got != 101 {
		t.Fatalf("count = %d, want 101", got)
	}
}

func TestPreparedStatementsSurviveCompaction(t *testing.T) {
	t.Parallel()
	db := memDB()
	growAndShrink(t, db)
	s := db.Session(SessionOptions{})
	defer s.Close()
	stmt, err := s.Prepare("SELECT count(*) FROM t WHERE n = $1")
	if err != nil {
		t.Fatal(err)
	}
	before, err := prepOutcome(s, stmt, []Value{IntValue(3)})
	if err != nil {
		t.Fatal(err)
	}
	if err := db.Compact("main"); err != nil {
		t.Fatal(err)
	}
	after, err := prepOutcome(s, stmt, []Value{IntValue(3)})
	if err != nil {
		t.Fatalf("prepared statement after compaction: %v", err)
	}
	if !reflect.DeepEqual(after.Rows, before.Rows) {
		t.Fatalf("rows = %v, want %v", after.Rows, before.Rows)
	}
}

func TestCompactionPreconditionsFailWithoutWriting(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "compact-preconditions.jed")
	db := createCompactFile(t, path)
	growAndShrink(t, db)
	pages := db.PageCount()

	if code := compactCode(t, db.Compact("nope")); code != "42704" {
		t.Fatalf("unknown name: %s, want 42704", code)
	}
	if code := compactCode(t, db.Compact("temp")); code != "42704" {
		t.Fatalf("temp: %s, want 42704", code)
	}

	// A pinned reader blocks it, and so does an open cursor.
	reader := db.ReadSession()
	if code := compactCode(t, db.Compact("main")); code != "55006" {
		t.Fatalf("pinned reader: %s, want 55006", code)
	}
	reader.Close()
	s := db.Session(SessionOptions{})
	rows, err := s.queryValues("SELECT id FROM t", nil)
	if err != nil {
		t.Fatal(err)
	}
	if !rows.Next() {
		t.Fatal("expected a row")
	}
	if code := compactCode(t, db.Compact("main")); code != "55006" {
		t.Fatalf("open cursor: %s, want 55006", code)
	}
	rows.Close()

	// An open write transaction blocks it; compaction never waits for the writer gate.
	writer := db.Session(SessionOptions{})
	if err := writer.Begin(true); err != nil {
		t.Fatal(err)
	}
	if code := compactCode(t, db.Compact("main")); code != "55006" {
		t.Fatalf("open write transaction: %s, want 55006", code)
	}
	if err := writer.Rollback(); err != nil {
		t.Fatal(err)
	}
	s.Close()
	writer.Close()

	if db.PageCount() != pages {
		t.Fatalf("a refused compaction wrote: page count %d -> %d", pages, db.PageCount())
	}
	if err := db.Compact("main"); err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}

	ro, err := OpenDatabaseWithOptions(path, OpenOptions{ReadOnly: true})
	if err != nil {
		t.Fatal(err)
	}
	defer ro.Close()
	if code := compactCode(t, ro.Compact("main")); code != "25006" {
		t.Fatalf("read-only handle: %s, want 25006", code)
	}
}

func TestAttachmentsCompactIndependently(t *testing.T) {
	t.Parallel()
	work := filepath.Join(t.TempDir(), "compact-attachment.jed")
	{
		db := createCompactFile(t, work)
		growAndShrink(t, db)
		if err := db.Close(); err != nil {
			t.Fatal(err)
		}
	}
	db := memDB()
	defer db.Close()
	if err := db.Attach("work", AttachFile(work), false); err != nil {
		t.Fatal(err)
	}
	if err := db.Attach("scratch", AttachMemory(), false); err != nil {
		t.Fatal(err)
	}
	mainVersion := db.Version()
	info, err := os.Stat(work)
	if err != nil {
		t.Fatal(err)
	}
	before := info.Size()
	if err := db.Compact("Work"); err != nil {
		t.Fatalf("compact work: %v", err)
	}
	if err := db.Compact("scratch"); err != nil {
		t.Fatalf("compact scratch: %v", err)
	}
	if info, err = os.Stat(work); err != nil {
		t.Fatal(err)
	}
	if info.Size()*4 >= before {
		t.Fatalf("attachment file %d -> %d bytes, want a >4x shrink", before, info.Size())
	}
	if db.Version() != mainVersion {
		t.Fatalf("main version moved: %d -> %d", mainVersion, db.Version())
	}
	if got := countRows(t, db, "work.t"); got != 100 {
		t.Fatalf("work.t count = %d, want 100", got)
	}
	if _, err := queryOutcome(db, "INSERT INTO work.t VALUES (5000, 'tail', 3)", nil); err != nil {
		t.Fatal(err)
	}
	if err := db.Detach("work"); err != nil {
		t.Fatal(err)
	}
	if err := db.Attach("frozen", AttachFile(work), true); err != nil {
		t.Fatal(err)
	}
	if code := compactCode(t, db.Compact("frozen")); code != "25006" {
		t.Fatalf("read-only attachment: %s, want 25006", code)
	}
	if got := countRows(t, db, "frozen.t"); got != 101 {
		t.Fatalf("frozen.t count = %d, want 101", got)
	}
}

func TestAStaleTempFileIsIgnoredAndReplaced(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "compact-stale-temp.jed")
	db := createCompactFile(t, path)
	growAndShrink(t, db)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	// A crash mid-compaction leaves a partial temp file; open never reads it.
	if err := os.WriteFile(path+".jedtmp", []byte("partial image"), 0o644); err != nil {
		t.Fatal(err)
	}
	db, err := OpenDatabase(path)
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	if got := countRows(t, db, "t"); got != 100 {
		t.Fatalf("count = %d, want 100", got)
	}
	if err := db.Compact("main"); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(path + ".jedtmp"); !os.IsNotExist(err) {
		t.Fatalf("temp file left behind: %v", err)
	}
}

func TestCompactionKeepsSymlinksAndPermissionBits(t *testing.T) {
	t.Parallel()
	if runtime.GOOS == "windows" {
		t.Skip("symlinks and unix permission bits")
	}
	dir := t.TempDir()
	path := filepath.Join(dir, "compact-target.jed")
	link := filepath.Join(dir, "compact-link.jed")
	db := createCompactFile(t, path)
	growAndShrink(t, db)
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(path, 0o640); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(path, link); err != nil {
		t.Fatal(err)
	}

	db, err := OpenDatabase(link)
	if err != nil {
		t.Fatal(err)
	}
	if err := db.Compact("main"); err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	linfo, err := os.Lstat(link)
	if err != nil {
		t.Fatal(err)
	}
	if linfo.Mode()&os.ModeSymlink == 0 {
		t.Fatal("the symlink was replaced by a regular file")
	}
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	if info.Mode().Perm() != 0o640 {
		t.Fatalf("permission bits = %o, want 640", info.Mode().Perm())
	}
	db, err = OpenDatabase(link)
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	if got := countRows(t, db, "t"); got != 100 {
		t.Fatalf("count = %d, want 100", got)
	}
}
