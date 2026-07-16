package jed

// Host-attached databases — the Database.Attach/Detach host API (spec/design/attached-databases.md
// §4/§6, Slices 1b + 2). These are the behaviors the shared corpus CANNOT express (it is single-handle
// SQL-in/rows-out and cannot call db.Attach — CLAUDE.md §10): the attach/detach lifecycle, the read-only
// write-rejection (25006), detach-in-use (55006), reserved/duplicate names (42710), unknown detach
// (42704), and — for FILE attachments (Slice 2) — cross-file read/join, read-write durability across a
// standalone reopen, the one-durable-writer rule (0A000), page-size independence, and missing-file
// (58P01). The in-memory SQL routing lives in the corpus (suites/attach/in_memory.test); file durability
// / reopen is inherently a per-core host test (out of corpus reach). Mirrors impl/rust/tests/attach.rs
// and impl/ts/tests/attach.test.ts.

import (
	"path/filepath"
	"strconv"
	"testing"
)

// attachExec runs a statement on a session and fails the test on error.
func attachExec(t *testing.T, s *Session, sql string) {
	t.Helper()
	if _, err := queryOutcome(s, sql, nil); err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
}

// attachErrCode runs a statement expected to fail and returns its SQLSTATE.
func attachErrCode(t *testing.T, s *Session, sql string) string {
	t.Helper()
	_, err := queryOutcome(s, sql, nil)
	if err == nil {
		t.Fatalf("%q: expected an error, got nil", sql)
	}
	return err.(*EngineError).Code()
}

// TestAttachLifecycle drives the whole single-handle arc: attach an in-memory database, create +
// populate a table in it by qualifier, read it back, then detach it (making it unreachable again).
func TestAttachLifecycle(t *testing.T) {
	t.Parallel()
	db := memDB()
	if err := db.Attach("mydb", AttachMemory(), false); err != nil {
		t.Fatalf("attach: %v", err)
	}
	s := db.Session(SessionOptions{})
	attachExec(t, s, "CREATE TABLE mydb.t (id i32 PRIMARY KEY, v i32)")
	attachExec(t, s, "INSERT INTO mydb.t VALUES (1, 10), (2, 20)")

	rows, err := s.queryValues("SELECT v FROM mydb.t ORDER BY id", nil)
	if err != nil {
		t.Fatalf("select: %v", err)
	}
	var got []int64
	for rows.Next() {
		got = append(got, rows.Row()[0].Int)
	}
	rows.Close() // release the streaming cursor's reader pin (a live cursor would block the detach below)
	if len(got) != 2 || got[0] != 10 || got[1] != 20 {
		t.Fatalf("rows = %v, want [10 20]", got)
	}

	// The committed attachment change is visible to a freshly-minted session over the same handle
	// (the roots.attached publish, §5) — proves the commit published a new attached root. Drain the
	// cursor fully so it releases its reader pin (an undrained streaming cursor would hold the roots
	// and make the detach below 55006).
	s2 := db.Session(SessionOptions{})
	r2, err := s2.queryValues("SELECT v FROM mydb.t WHERE id = 1", nil)
	if err != nil {
		t.Fatalf("second session cannot see attachment: %v", err)
	}
	n := 0
	for r2.Next() {
		n++
	}
	r2.Close()
	if n != 1 {
		t.Fatalf("second session rows = %d, want 1", n)
	}

	if err := db.Detach("mydb"); err != nil {
		t.Fatalf("detach: %v", err)
	}
	// After detach the qualifier is unknown again (42P01).
	if code := attachErrCode(t, db.Session(SessionOptions{}), "SELECT v FROM mydb.t"); code != "42P01" {
		t.Fatalf("post-detach select: want 42P01, got %s", code)
	}
}

// TestAttachmentWorkingRootsAndCursorsDoNotAlias pins main and attached working roots through
// write-transaction cursors, churns both rightmost leaves, and proves neither old root nor either
// database aliases the other's mutation state. Rollback restores both committed roots together.
func TestAttachmentWorkingRootsAndCursorsDoNotAlias(t *testing.T) {
	t.Parallel()
	db := memDB()
	if err := db.Attach("work", AttachMemory(), false); err != nil {
		t.Fatal(err)
	}
	s := db.Session(SessionOptions{})
	defer s.Close()
	attachExec(t, s, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
	attachExec(t, s, "CREATE TABLE work.t (id i32 PRIMARY KEY, v i32)")
	attachExec(t, s, "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)")
	attachExec(t, s, "INSERT INTO work.t VALUES (1, 100), (2, 200), (3, 300)")
	if err := s.Begin(true); err != nil {
		t.Fatal(err)
	}
	mainPin, err := s.queryValues("SELECT id, v FROM t ORDER BY id", nil)
	if err != nil {
		t.Fatal(err)
	}
	workPin, err := s.queryValues("SELECT id, v FROM work.t ORDER BY id", nil)
	if err != nil {
		t.Fatal(err)
	}
	if !mainPin.Next() || mainPin.Row()[0].Int != 1 || mainPin.Row()[1].Int != 10 {
		t.Fatalf("main first row = %v", mainPin.Row())
	}
	if !workPin.Next() || workPin.Row()[0].Int != 1 || workPin.Row()[1].Int != 100 {
		t.Fatalf("work first row = %v", workPin.Row())
	}
	for id := int64(4); id <= 40; id++ {
		attachExec(t, s, "INSERT INTO t VALUES ("+strconv.FormatInt(id, 10)+", "+strconv.FormatInt(id*10, 10)+")")
		attachExec(t, s, "INSERT INTO work.t VALUES ("+strconv.FormatInt(id, 10)+", "+strconv.FormatInt(id*100, 10)+")")
	}
	for _, sql := range []string{"SELECT count(*) FROM t", "SELECT count(*) FROM work.t"} {
		out, err := queryOutcome(s, sql, nil)
		if err != nil || len(out.Rows) != 1 || out.Rows[0][0].Int != 40 {
			t.Fatalf("%s = %v, err=%v", sql, out.Rows, err)
		}
	}
	var mainRest, workRest []int64
	for mainPin.Next() {
		mainRest = append(mainRest, mainPin.Row()[1].Int)
	}
	for workPin.Next() {
		workRest = append(workRest, workPin.Row()[1].Int)
	}
	if len(mainRest) != 2 || mainRest[0] != 20 || mainRest[1] != 30 {
		t.Fatalf("main pinned rest = %v", mainRest)
	}
	if len(workRest) != 2 || workRest[0] != 200 || workRest[1] != 300 {
		t.Fatalf("work pinned rest = %v", workRest)
	}
	_ = mainPin.Close()
	_ = workPin.Close()
	if err := s.Rollback(); err != nil {
		t.Fatal(err)
	}
	fresh := db.Session(SessionOptions{})
	defer fresh.Close()
	for _, sql := range []string{"SELECT count(*) FROM t", "SELECT count(*) FROM work.t"} {
		out, err := queryOutcome(fresh, sql, nil)
		if err != nil || len(out.Rows) != 1 || out.Rows[0][0].Int != 3 {
			t.Fatalf("after rollback %s = %v, err=%v", sql, out.Rows, err)
		}
	}
}

// TestAttachReadOnlyRejectsWrites — a read-only attachment rejects every write (DML + DDL) with 25006
// before any I/O (attached-databases.md §4), while a bare/main write is unaffected.
func TestAttachReadOnlyRejectsWrites(t *testing.T) {
	t.Parallel()
	db := memDB()
	if err := db.Attach("ro", AttachMemory(), true); err != nil {
		t.Fatalf("attach read-only: %v", err)
	}
	s := db.Session(SessionOptions{})
	for _, sql := range []string{
		"CREATE TABLE ro.t (id i32 PRIMARY KEY)",
		"CREATE INDEX ix ON ro.t (id)",
		"INSERT INTO ro.t VALUES (1)",
		"UPDATE ro.t SET id = 2",
		"DELETE FROM ro.t",
	} {
		if code := attachErrCode(t, s, sql); code != "25006" {
			t.Fatalf("%q: want 25006, got %s", sql, code)
		}
	}
	// A write to main is unaffected by a read-only attachment elsewhere.
	attachExec(t, s, "CREATE TABLE keep (id i32 PRIMARY KEY)")
}

// TestDetachInUseIs55006 — detaching while a live reader session pins the committed roots is 55006
// (object_in_use); once the reader closes, the detach succeeds (attached-databases.md §4/§5, the
// reader-liveness watermark — a reader pins the whole roots, so it pins every attachment).
func TestDetachInUseIs55006(t *testing.T) {
	t.Parallel()
	db := memDB()
	if err := db.Attach("mydb", AttachMemory(), false); err != nil {
		t.Fatalf("attach: %v", err)
	}
	reader := db.ReadSession() // pins the committed roots in the live registry
	if err := db.Detach("mydb"); err == nil {
		t.Fatal("detach while a reader is live: want 55006, got nil")
	} else if code := err.(*EngineError).Code(); code != "55006" {
		t.Fatalf("detach in use: want 55006, got %s", code)
	}
	reader.Close() // drains the pin
	if err := db.Detach("mydb"); err != nil {
		t.Fatalf("detach after reader closed: %v", err)
	}
}

// TestAttachNameErrors — a reserved name (main/temp) or an already-attached name is 42710; detaching
// an unknown / reserved database is 42704.
func TestAttachNameErrors(t *testing.T) {
	t.Parallel()
	db := memDB()
	if err := db.Attach("mydb", AttachMemory(), false); err != nil {
		t.Fatalf("attach: %v", err)
	}
	for _, name := range []string{"main", "temp", "MAIN", "Temp", "mydb", "MyDB"} {
		if err := db.Attach(name, AttachMemory(), false); err == nil {
			t.Fatalf("attach %q: want 42710, got nil", name)
		} else if code := err.(*EngineError).Code(); code != "42710" {
			t.Fatalf("attach %q: want 42710, got %s", name, code)
		}
	}
	for _, name := range []string{"nope", "main", "temp"} {
		if err := db.Detach(name); err == nil {
			t.Fatalf("detach %q: want 42704, got nil", name)
		} else if code := err.(*EngineError).Code(); code != "42704" {
			t.Fatalf("detach %q: want 42704, got %s", name, code)
		}
	}
}

// makeFileDB creates a fresh single-file database at dir/name with the given page size (0 → default),
// runs each statement autocommitting it durably, closes it, and returns the path — the reusable
// fixture for the file-attach tests below (a self-describing jed file another handle can attach).
func makeFileDB(t *testing.T, dir, name string, pageSize uint32, stmts ...string) string {
	t.Helper()
	path := filepath.Join(dir, name)
	db, err := CreateDatabase(CreateOptions{Path: path, PageSize: pageSize, SkipFsync: true})
	if err != nil {
		t.Fatalf("create %s: %v", name, err)
	}
	s := db.Session(SessionOptions{})
	for _, sql := range stmts {
		attachExec(t, s, sql)
	}
	s.Close()
	if err := db.Close(); err != nil {
		t.Fatalf("close %s: %v", name, err)
	}
	return path
}

// TestAttachFileReadOnlyCrossRead — attach an existing file database read-only, join a local table
// against it, and confirm every write to it is 25006 (the natural reference-database mode,
// attached-databases.md §4, Slice 2). Reads fault the attached file's pages through its own pager.
func TestAttachFileReadOnlyCrossRead(t *testing.T) {
	t.Parallel()
	ref := makeFileDB(t, t.TempDir(), "ref.jed", 0,
		"CREATE TABLE city (id i32 PRIMARY KEY, name text)",
		"INSERT INTO city VALUES (1, 'Ada'), (2, 'Bos')")

	db := memDB()
	if err := db.Attach("ref", AttachFile(ref), true); err != nil {
		t.Fatalf("attach file read-only: %v", err)
	}
	defer db.Detach("ref")
	s := db.Session(SessionOptions{})
	defer s.Close()
	attachExec(t, s, "CREATE TABLE visit (city_id i32 PRIMARY KEY, n i32)")
	attachExec(t, s, "INSERT INTO visit VALUES (1, 7), (2, 9)")

	// A cross-FILE join: local `visit` against the read-only attached file's `city`.
	rows, err := s.queryValues(
		"SELECT c.name, v.n FROM visit v JOIN ref.city c ON c.id = v.city_id ORDER BY c.id", nil,
	)
	if err != nil {
		t.Fatalf("cross-file join: %v", err)
	}
	var got []string
	for rows.Next() {
		got = append(got, rows.Row()[0].str())
	}
	rows.Close()
	if len(got) != 2 || got[0] != "Ada" || got[1] != "Bos" {
		t.Fatalf("join rows = %v, want [Ada Bos]", got)
	}

	// Every write to the read-only attachment is 25006, before any I/O.
	for _, sql := range []string{
		"CREATE TABLE ref.t (id i32 PRIMARY KEY)",
		"INSERT INTO ref.city VALUES (3, 'Cai')",
		"UPDATE ref.city SET name = 'x'",
		"DELETE FROM ref.city",
	} {
		if code := attachErrCode(t, s, sql); code != "25006" {
			t.Fatalf("%q: want 25006, got %s", sql, code)
		}
	}
}

// TestAttachFileReadWritePersistsAcrossReopen — attach a file read-write, create+populate a table in
// it by qualifier, detach (flushing the OS handle), then open that file STANDALONE and confirm the
// writes are durable (attached-databases.md §5 — a file attachment commits durably through its own
// pager + alternating meta slot + fsync).
func TestAttachFileReadWritePersistsAcrossReopen(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	work := makeFileDB(t, dir, "work.jed", 0) // an empty writable file to attach

	db := memDB()
	if err := db.Attach("work", AttachFile(work), false); err != nil {
		t.Fatalf("attach file read-write: %v", err)
	}
	s := db.Session(SessionOptions{})
	attachExec(t, s, "CREATE TABLE work.acct (id i32 PRIMARY KEY, bal i32)")
	attachExec(t, s, "INSERT INTO work.acct VALUES (1, 100), (2, 200)")
	attachExec(t, s, "CREATE INDEX acct_bal ON work.acct (bal)")
	s.Close()
	if err := db.Detach("work"); err != nil {
		t.Fatalf("detach: %v", err)
	}
	db.Close()

	// Reopen the attached file on its own — the rows must be there (durable + self-describing).
	reopened, err := OpenDatabaseWithOptions(work, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatalf("reopen attached file standalone: %v", err)
	}
	defer reopened.Close()
	rows := queryRows(t, reopened, "SELECT id, bal FROM acct ORDER BY id")
	if len(rows) != 2 || rows[0][0].Int != 1 || rows[0][1].Int != 100 || rows[1][1].Int != 200 {
		t.Fatalf("reopened rows = %v, want [[1 100] [2 200]]", rows)
	}
	// The index persisted too (introspection — the catalog carries it).
	if tbl, ok := reopened.Table("acct"); !ok || len(tbl.Indexes) != 1 || tbl.Indexes[0].Name != "acct_bal" {
		t.Fatalf("reopened index missing: %+v", tbl)
	}
}

// TestAttachFileOneDurableWriter — a transaction may write at most one FILE-backed database (§5). With
// a FILE main and a read-write FILE attachment, a block that writes BOTH is 0A000 at COMMIT and commits
// nothing; writing either one alone succeeds. In-memory attachments never count against the slot.
func TestAttachFileOneDurableWriter(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	mainPath := makeFileDB(t, dir, "main.jed", 0, "CREATE TABLE m (id i32 PRIMARY KEY)")
	extra := makeFileDB(t, dir, "extra.jed", 0, "CREATE TABLE e (id i32 PRIMARY KEY)")

	db, err := OpenDatabaseWithOptions(mainPath, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatalf("open file main: %v", err)
	}
	defer db.Close()
	if err := db.Attach("extra", AttachFile(extra), false); err != nil {
		t.Fatalf("attach extra file: %v", err)
	}
	defer db.Detach("extra")

	s := db.Session(SessionOptions{})
	defer s.Close()
	if err := s.Begin(true); err != nil {
		t.Fatalf("begin: %v", err)
	}
	attachExec(t, s, "INSERT INTO m VALUES (1)")       // main (file) dirtied
	attachExec(t, s, "INSERT INTO extra.e VALUES (1)") // a SECOND durable (file) database dirtied
	if err := s.Commit(); err == nil {
		t.Fatal("commit writing two durable databases: want 0A000, got nil")
	} else if code := err.(*EngineError).Code(); code != "0A000" {
		t.Fatalf("two durable writers: want 0A000, got %s", code)
	}
	// Nothing was committed — both files are still empty of the attempted rows.
	if rows := queryRows(t, s, "SELECT count(*) FROM m"); rows[0][0].Int != 0 {
		t.Fatalf("main m should be empty after the rejected commit, got %v", rows)
	}
	if rows := queryRows(t, s, "SELECT count(*) FROM extra.e"); rows[0][0].Int != 0 {
		t.Fatalf("extra e should be empty after the rejected commit, got %v", rows)
	}

	// Writing each durable database ALONE (its own autocommit statement) is fine.
	attachExec(t, s, "INSERT INTO m VALUES (2)")
	attachExec(t, s, "INSERT INTO extra.e VALUES (2)")
	if rows := queryRows(t, s, "SELECT id FROM m"); len(rows) != 1 || rows[0][0].Int != 2 {
		t.Fatalf("main m = %v, want [[2]]", rows)
	}
	if rows := queryRows(t, s, "SELECT id FROM extra.e"); len(rows) != 1 || rows[0][0].Int != 2 {
		t.Fatalf("extra e = %v, want [[2]]", rows)
	}
}

// TestAttachFileWithMemoryMainMultiWrite — the slot counts only FILE databases: an IN-MEMORY main plus
// a read-write FILE attachment is ONE durable writer, so a block writing both commits cleanly (§5).
func TestAttachFileWithMemoryMainMultiWrite(t *testing.T) {
	t.Parallel()
	work := makeFileDB(t, t.TempDir(), "work.jed", 0, "CREATE TABLE w (id i32 PRIMARY KEY)")
	db := memDB() // in-memory main — not durable
	if err := db.Attach("work", AttachFile(work), false); err != nil {
		t.Fatalf("attach: %v", err)
	}
	defer func() { db.Detach("work"); db.Close() }()
	s := db.Session(SessionOptions{})
	defer s.Close()
	attachExec(t, s, "CREATE TABLE local (id i32 PRIMARY KEY)")
	if err := s.Begin(true); err != nil {
		t.Fatalf("begin: %v", err)
	}
	attachExec(t, s, "INSERT INTO local VALUES (1)")  // in-memory main (free)
	attachExec(t, s, "INSERT INTO work.w VALUES (1)") // the one durable writer
	if err := s.Commit(); err != nil {
		t.Fatalf("memory-main + one file attachment should commit: %v", err)
	}
	if rows := queryRows(t, s, "SELECT id FROM work.w"); len(rows) != 1 || rows[0][0].Int != 1 {
		t.Fatalf("work.w = %v, want [[1]]", rows)
	}
}

// TestAttachFilePageSizeIndependent — an attached file keeps its OWN page space (§2): attaching a file
// created at a non-default page size and writing into it serializes at THAT page size, verified by a
// standalone reopen. Guards the CREATE TABLE / CREATE INDEX page-size routing (attachPageSize).
func TestAttachFilePageSizeIndependent(t *testing.T) {
	t.Parallel()
	dir := t.TempDir()
	small := makeFileDB(t, dir, "small.jed", 256) // a 256-byte-page file, unlike the 4096 default main
	db := memDB()
	if err := db.Attach("small", AttachFile(small), false); err != nil {
		t.Fatalf("attach: %v", err)
	}
	s := db.Session(SessionOptions{})
	attachExec(t, s, "CREATE TABLE small.grid (id i32 PRIMARY KEY, v i32)")
	// Enough rows to force at least one leaf split at the small page size (its own page space).
	for i := 1; i <= 40; i++ {
		attachExec(t, s, "INSERT INTO small.grid VALUES ("+strconv.Itoa(i)+", "+strconv.Itoa(i*i)+")")
	}
	s.Close()
	if err := db.Detach("small"); err != nil {
		t.Fatalf("detach: %v", err)
	}
	db.Close()

	reopened, err := OpenDatabaseWithOptions(small, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatalf("reopen small-page file: %v", err)
	}
	defer reopened.Close()
	if reopened.PageSize() != 256 {
		t.Fatalf("reopened page size = %d, want 256", reopened.PageSize())
	}
	rows := queryRows(t, reopened, "SELECT count(*), sum(v) FROM grid")
	if rows[0][0].Int != 40 || rows[0][1].Int != 22140 { // sum of i*i for i in 1..40
		t.Fatalf("reopened grid aggregate = %v, want [[40 22140]]", rows)
	}
}

// TestAttachFileReattach — detaching a file releases its OS handle, so the same file can be attached
// again (a leaked descriptor would not prevent this, but a re-attach also proves the registry cleared).
func TestAttachFileReattach(t *testing.T) {
	t.Parallel()
	ref := makeFileDB(t, t.TempDir(), "ref.jed", 0,
		"CREATE TABLE t (id i32 PRIMARY KEY)", "INSERT INTO t VALUES (1)")
	db := memDB()
	defer db.Close()
	for i := 0; i < 3; i++ {
		if err := db.Attach("ref", AttachFile(ref), true); err != nil {
			t.Fatalf("attach #%d: %v", i, err)
		}
		s := db.Session(SessionOptions{})
		rows := queryRows(t, s, "SELECT id FROM ref.t")
		s.Close()
		if len(rows) != 1 || rows[0][0].Int != 1 {
			t.Fatalf("attach #%d rows = %v, want [[1]]", i, rows)
		}
		if err := db.Detach("ref"); err != nil {
			t.Fatalf("detach #%d: %v", i, err)
		}
	}
}

// TestAttachFileMissingIs58P01 — attaching a nonexistent file surfaces the same host/file code as
// opening main (attached-databases.md §11 / hosts.md §4).
func TestAttachFileMissingIs58P01(t *testing.T) {
	t.Parallel()
	db := memDB()
	defer db.Close()
	path := filepath.Join(t.TempDir(), "nope.jed")
	if err := db.Attach("x", AttachFile(path), true); err == nil {
		t.Fatal("attach missing file: want an error, got nil")
	} else if code := err.(*EngineError).Code(); code != "58P01" {
		t.Fatalf("attach missing file: want 58P01, got %s", code)
	}
	// The failed attach left no registry entry — the name is free.
	if err := db.Attach("x", AttachMemory(), false); err != nil {
		t.Fatalf("re-attach after failed file attach: %v", err)
	}
}

// TestAttachCaseInsensitiveQualifier — an attachment is reached case-insensitively by its qualifier
// (unquoted identifiers fold to lower case), matching how main/temp resolve.
func TestAttachCaseInsensitiveQualifier(t *testing.T) {
	t.Parallel()
	db := memDB()
	if err := db.Attach("Reports", AttachMemory(), false); err != nil {
		t.Fatalf("attach: %v", err)
	}
	s := db.Session(SessionOptions{})
	attachExec(t, s, "CREATE TABLE reports.sales (id i32 PRIMARY KEY)")
	attachExec(t, s, "INSERT INTO REPORTS.sales VALUES (1)")
	if _, err := s.queryValues("SELECT id FROM Reports.sales", nil); err != nil {
		t.Fatalf("case-insensitive qualifier: %v", err)
	}
}
