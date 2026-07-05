package jed

// fsync=off (api.md §2.1) is a DEV/TESTING durability knob: a commit writes the identical bytes in the
// same order but skips the fdatasync barrier. It must be byte/result-NEUTRAL — a database built with it
// holds the exact same on-disk image and reads back identically; only the flush-to-platter is skipped
// (so the data survives a process crash but not an OS crash). The conformance disk harness runs with it
// to cut the fsync-per-commit cost. The corpus cannot express fsync timing or file-byte identity, so
// this is a per-core unit test (CLAUDE.md §10). Mirrors impl/rust/tests/nosync.rs and
// impl/ts/tests/nosync.test.ts.

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"
	"testing"
)

// buildSampleDB creates a file database at path with the given noSync setting, runs a fixed
// deterministic workload (DDL + inserts + an update + a delete, autocommitted across many commits),
// and closes it. Deterministic (no clock/entropy), so two runs differing only in noSync must produce
// byte-identical files.
func buildSampleDB(t *testing.T, path string, noSync bool) {
	t.Helper()
	db, err := create(path, databaseOptions{PageSize: DefaultPageSize, noSync: noSync})
	if err != nil {
		t.Fatal(err)
	}
	mustExec(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32, s text)")
	for i := 1; i <= 50; i++ {
		mustExec(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, %d, 'row-%d')", i, i*10, i))
	}
	mustExec(t, db, "UPDATE t SET v = v + 1 WHERE id % 2 = 0")
	mustExec(t, db, "DELETE FROM t WHERE id > 40")
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
}

// TestSkipFsyncRoundTrips builds a database with fsync=off, then reopens it in the same process. The OS
// page cache holds the un-synced writes, so the committed state is fully readable — fsync=off forfeits
// durability only across an OS crash, not a clean close + reopen.
func TestSkipFsyncRoundTrips(t *testing.T) {
	path := filepath.Join(t.TempDir(), "nosync.jed")
	buildSampleDB(t, path, true) // fsync=off
	db, err := openWithOptions(path, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	rows := queryRows(t, db, "SELECT id, v FROM t ORDER BY id")
	if len(rows) != 40 { // 50 inserted, ids 41..50 deleted
		t.Fatalf("got %d rows, want 40", len(rows))
	}
	// id=2 is even, so v = 20 + 1 = 21 after the UPDATE; id=1 is odd, v = 10.
	if rows[0][0].Int != 1 || rows[0][1].Int != 10 {
		t.Fatalf("row id=1: got (%d, %d), want (1, 10)", rows[0][0].Int, rows[0][1].Int)
	}
	if rows[1][0].Int != 2 || rows[1][1].Int != 21 {
		t.Fatalf("row id=2: got (%d, %d), want (2, 21)", rows[1][0].Int, rows[1][1].Int)
	}
}

// TestSkipFsyncByteIdentical is the load-bearing guarantee: fsync=off changes only *when* bytes are
// flushed, never *which* bytes. The same deterministic workload built with fsync on and off must yield
// byte-identical files (so no golden churn, no format bump, cross-core byte-identity preserved).
func TestSkipFsyncByteIdentical(t *testing.T) {
	dir := t.TempDir()
	on := filepath.Join(dir, "on.jed")
	off := filepath.Join(dir, "off.jed")
	buildSampleDB(t, on, false) // fsync on (the default)
	buildSampleDB(t, off, true) // fsync off
	a, err := os.ReadFile(on)
	if err != nil {
		t.Fatal(err)
	}
	b, err := os.ReadFile(off)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(a, b) {
		t.Fatalf("fsync=off changed the on-disk image: fsync-on=%d bytes, fsync-off=%d bytes", len(a), len(b))
	}
}
