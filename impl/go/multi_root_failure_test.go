package jed

// A multi-root commit that fails after an in-memory attachment was staged (attached-databases.md §5)
// must leave that attachment's storage matching its still-published root. Before staging was split
// from adoption, the attachment adopted its page accounting and compacted relative to the UNPUBLISHED
// root, so its free list held pages the published root still referenced; the next commit overwrote
// them under every reader of the old root (silent corruption or XX001). Fault injection and storage
// internals are out of the corpus's reach (CLAUDE.md §10). Mirrors the Rust
// shared::multi_root_failure_tests and the TS multi_root_failure test.

import (
	"fmt"
	"path/filepath"
	"reflect"
	"testing"
)

const multiRootRows = 200

// attachmentRows reads every (id, v) of a.t in key order through a fresh session.
func attachmentRows(t *testing.T, db *Database) [][2]int64 {
	t.Helper()
	rows := queryRows(t, db.Session(SessionOptions{}), "SELECT id, v FROM a.t ORDER BY id")
	out := make([][2]int64, len(rows))
	for i, row := range rows {
		out[i] = [2]int64{row[0].Int, row[1].Int}
	}
	return out
}

func expectedRows(v func(id int64) int64) [][2]int64 {
	out := make([][2]int64, multiRootRows)
	for i := range out {
		id := int64(i + 1)
		out[i] = [2]int64{id, v(id)}
	}
	return out
}

func assertAttachmentRows(t *testing.T, db *Database, want [][2]int64) {
	t.Helper()
	if got := attachmentRows(t, db); !reflect.DeepEqual(got, want) {
		t.Fatalf("attachment rows diverged from the published root:\n got %v\nwant %v", got, want)
	}
}

// seedAttachment seeds a.t with multiRootRows rows (v = 0) in one commit — a multi-leaf tree at page
// size 256.
func seedAttachment(t *testing.T, s *Session) {
	t.Helper()
	mustExec(t, s, "CREATE TABLE a.t (id i64 PRIMARY KEY, v i64)")
	mustExec(t, s, fmt.Sprintf("INSERT INTO a.t SELECT g, 0 FROM generate_series(1, %d) AS g", multiRootRows))
}

// dirtyAttachment rewrites every leaf of a.t and doubles it, so the failing commit's high-water passes
// twice the live count and its post-commit compaction is due.
func dirtyAttachment(t *testing.T, s *Session) {
	t.Helper()
	mustExec(t, s, "UPDATE a.t SET v = 1")
	mustExec(t, s, fmt.Sprintf("INSERT INTO a.t SELECT g, 1 FROM generate_series(%d, %d) AS g", multiRootRows+1, 3*multiRootRows))
}

func armStorageFault(t *testing.T, st *storage, f commitFault) {
	t.Helper()
	if err := st.paging.withPager(func(p *pager) error { p.armFault(f); return nil }); err != nil {
		t.Fatal(err)
	}
}

func wantCode(t *testing.T, err error, code string) {
	t.Helper()
	var ee *EngineError
	if err == nil || !asEngineError(err, &ee) || ee.Code() != code {
		t.Fatalf("want %s, got %v", code, err)
	}
}

func TestFailedMainPersistLeavesInMemoryAttachmentIntact(t *testing.T) {
	t.Parallel()
	path := filepath.Join(t.TempDir(), "multi_root_failure.jed")
	db, err := CreateDatabase(CreateOptions{Path: path, PageSize: 256, SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	if err := db.Attach("a", AttachMemory(), false); err != nil {
		t.Fatal(err)
	}
	s := db.Session(SessionOptions{})
	mustExec(t, s, "CREATE TABLE t (id i64 PRIMARY KEY)")
	seedAttachment(t, s)

	// Main's durable commit fails at its barrier after the attachment was dirtied in the same tx.
	if err := s.Begin(true); err != nil {
		t.Fatal(err)
	}
	dirtyAttachment(t, s)
	mustExec(t, s, "INSERT INTO t VALUES (1)")
	armStorageFault(t, db.core.storage, commitFault{point: faultSync, n: 1})
	wantCode(t, s.Commit(), "58030")

	// Nothing published: a fresh session sees the attachment's prior root.
	assertAttachmentRows(t, db, expectedRows(func(int64) int64 { return 0 }))
	// Main now refuses writes until reopened, but each attempt still stages attachment pages before
	// main's persist fails. They must never land on pages the published root uses.
	for k := 0; k < 4; k++ {
		_, err := queryOutcome(s, fmt.Sprintf("UPDATE a.t SET v = %d WHERE id %% 7 = %d", 10+k, k), nil)
		wantCode(t, err, "58030")
		assertAttachmentRows(t, db, expectedRows(func(int64) int64 { return 0 }))
	}
}

func TestFailedLaterAttachmentLeavesEarlierAttachmentIntact(t *testing.T) {
	t.Parallel()
	db, err := CreateDatabase(CreateOptions{PageSize: 256})
	if err != nil {
		t.Fatal(err)
	}
	for _, name := range []string{"a", "b"} {
		if err := db.Attach(name, AttachMemory(), false); err != nil {
			t.Fatal(err)
		}
	}
	s := db.Session(SessionOptions{})
	mustExec(t, s, "CREATE TABLE t (id i64 PRIMARY KEY)")
	mustExec(t, s, "CREATE TABLE b.t (id i64 PRIMARY KEY)")
	seedAttachment(t, s)

	// a stages before b (in-memory attachments commit in name order); b's pack fails.
	if err := s.Begin(true); err != nil {
		t.Fatal(err)
	}
	dirtyAttachment(t, s)
	mustExec(t, s, "INSERT INTO b.t VALUES (1)")
	mustExec(t, s, "INSERT INTO t VALUES (1)")
	armStorageFault(t, db.core.attachment("b").storage, commitFault{point: faultBodyWrite, n: 1, tearBytes: -1})
	wantCode(t, s.Commit(), "58030")
	assertAttachmentRows(t, db, expectedRows(func(int64) int64 { return 0 }))

	// Main and a stay writable. Each later commit rewrites one leaf path and reuses a's free list; the
	// leaves it does not touch are still the published root's and must survive.
	for k := 0; k < 6; k++ {
		mustExec(t, s, fmt.Sprintf("UPDATE a.t SET v = 7 WHERE id = %d", 1+37*k))
		mustExec(t, s, fmt.Sprintf("INSERT INTO t VALUES (%d)", k+2))
	}
	assertAttachmentRows(t, db, expectedRows(func(id int64) int64 {
		if (id-1)%37 == 0 && id <= 1+37*5 {
			return 7
		}
		return 0
	}))
	if n := queryRows(t, db.Session(SessionOptions{}), "SELECT count(*) FROM t")[0][0].Int; n != 6 {
		t.Fatalf("main rows = %d want 6", n)
	}
}
