package jed

// Host-attached in-memory databases — the Database.Attach/Detach host API (spec/design/attached-
// databases.md §4/§6, Slice 1b). These are the behaviors the shared corpus CANNOT express (it is
// single-handle SQL-in/rows-out and cannot call db.Attach — CLAUDE.md §10): the attach/detach
// lifecycle, the read-only write-rejection (25006), detach-in-use (55006), reserved/duplicate names
// (42710), unknown detach (42704), and the file-source deferral (0A000). The SQL routing itself lives
// in the corpus (suites/attach/in_memory.test). Mirrors impl/rust/tests/attach.rs and
// impl/ts/test/attach.test.ts.

import "testing"

// attachExec runs a statement on a session and fails the test on error.
func attachExec(t *testing.T, s *Session, sql string) {
	t.Helper()
	if _, err := s.Execute(sql, nil); err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
}

// attachErrCode runs a statement expected to fail and returns its SQLSTATE.
func attachErrCode(t *testing.T, s *Session, sql string) string {
	t.Helper()
	_, err := s.Execute(sql, nil)
	if err == nil {
		t.Fatalf("%q: expected an error, got nil", sql)
	}
	return err.(*EngineError).Code()
}

// TestAttachLifecycle drives the whole single-handle arc: attach an in-memory database, create +
// populate a table in it by qualifier, read it back, then detach it (making it unreachable again).
func TestAttachLifecycle(t *testing.T) {
	db := memDB()
	if err := db.Attach("mydb", AttachMemory(), false); err != nil {
		t.Fatalf("attach: %v", err)
	}
	s := db.Session(SessionOptions{})
	attachExec(t, s, "CREATE TABLE mydb.t (id i32 PRIMARY KEY, v i32)")
	attachExec(t, s, "INSERT INTO mydb.t VALUES (1, 10), (2, 20)")

	rows, err := s.Query("SELECT v FROM mydb.t ORDER BY id", nil)
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
	r2, err := s2.Query("SELECT v FROM mydb.t WHERE id = 1", nil)
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

// TestAttachReadOnlyRejectsWrites — a read-only attachment rejects every write (DML + DDL) with 25006
// before any I/O (attached-databases.md §4), while a bare/main write is unaffected.
func TestAttachReadOnlyRejectsWrites(t *testing.T) {
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

// TestAttachFileSourceDeferred — a FILE-backed attach source is Slice 2; Database.Attach with a file
// source returns 0A000 now, so the host-API signature never changes when file attach lands. This is
// also the one-durable-writer guard's inert form in 1b (no writable file attachment can exist yet).
func TestAttachFileSourceDeferred(t *testing.T) {
	db := memDB()
	if err := db.Attach("f", AttachFile("/tmp/whatever.jed"), false); err == nil {
		t.Fatal("attach file source: want 0A000, got nil")
	} else if code := err.(*EngineError).Code(); code != "0A000" {
		t.Fatalf("attach file source: want 0A000, got %s", code)
	}
}

// TestAttachCaseInsensitiveQualifier — an attachment is reached case-insensitively by its qualifier
// (unquoted identifiers fold to lower case), matching how main/temp resolve.
func TestAttachCaseInsensitiveQualifier(t *testing.T) {
	db := memDB()
	if err := db.Attach("Reports", AttachMemory(), false); err != nil {
		t.Fatalf("attach: %v", err)
	}
	s := db.Session(SessionOptions{})
	attachExec(t, s, "CREATE TABLE reports.sales (id i32 PRIMARY KEY)")
	attachExec(t, s, "INSERT INTO REPORTS.sales VALUES (1)")
	if _, err := s.Query("SELECT id FROM Reports.sales", nil); err != nil {
		t.Fatalf("case-insensitive qualifier: %v", err)
	}
}
