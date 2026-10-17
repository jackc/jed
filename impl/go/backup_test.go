package jed

// Backup (spec/design/page-service.md §9; spec/pageservice/README.md §6.6) — a host act the SQL corpus
// cannot reach, so its byte-level contract lives here (CLAUDE.md §10): the compacted form is exactly
// the file compaction leaves, the page-identical form copies every reachable page at its own index
// and zeroes the rest, and the preconditions fail without writing. Backups through a page client of
// another core's owner are the process corpus's (spec/conformance/process/backup.process.toml).
// Mirrors impl/rust/tests/backup.rs and impl/ts/tests/backup.test.ts.

import (
	"bytes"
	"encoding/binary"
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

// backupFixture builds a file database with dead pages, a persisted free list, overflow chains, a
// secondary index, and a GiST index, so a page-identical backup must walk every kind of page.
func backupFixture(t *testing.T, path string) *Database {
	t.Helper()
	db := createCompactFile(t, path)
	growAndShrink(t, db)
	s := db.Session(SessionOptions{})
	defer s.Close()
	for _, sql := range []string{
		"CREATE TABLE big (id i64 PRIMARY KEY, body text)",
		"INSERT INTO big SELECT g, (SELECT string_agg(md5((g * 1000 + k)::text), '') FROM generate_series(1, 300) k) FROM generate_series(1, 20) g",
		"CREATE TABLE spans (id i64 PRIMARY KEY, r i64range)",
		"INSERT INTO spans SELECT g, i64range(g, g + 10) FROM generate_series(1, 300) g",
		"CREATE INDEX spans_r ON spans USING gist (r)",
		"UPDATE t SET n = n + 1 WHERE id % 2 = 0",
		"DELETE FROM big WHERE id > 10",
		"UPDATE t SET n = n + 1 WHERE id = 1",
	} {
		if _, err := queryOutcome(s, sql, nil); err != nil {
			t.Fatalf("%q: %v", sql, err)
		}
	}
	return db
}

func backupCode(t *testing.T, err error) string {
	t.Helper()
	if err == nil {
		t.Fatalf("expected an error, got nil")
	}
	return err.(*EngineError).Code()
}

func readFile(t *testing.T, path string) []byte {
	t.Helper()
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return b
}

// committedImage is the database file up to the committed meta's page count (the file may carry
// preallocation past it).
func committedImage(t *testing.T, path string) ([]byte, meta) {
	t.Helper()
	image := readFile(t, path)
	ps := int(binary.BigEndian.Uint32(image[8:12]))
	var best meta
	for slot := 0; slot < 2; slot++ {
		if m, ok := parseMeta(image[slot*ps : (slot+1)*ps]); ok && m.txid > best.txid {
			best = m
		}
	}
	return image[:int(best.pageCount)*ps], best
}

func TestCompactedBackupIsTheCompactedFile(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "db.jed")
	db := backupFixture(t, path)
	defer db.Close()
	dest := filepath.Join(dir, "backup.jed")
	if err := BackupForTesting(db, dest, false); err != nil {
		t.Fatal(err)
	}
	backup := readFile(t, dest)
	if err := db.Compact("main"); err != nil {
		t.Fatal(err)
	}
	compacted, _ := committedImage(t, path)
	if !bytes.Equal(backup, compacted) {
		t.Fatalf("the compacted backup (%d bytes) differs from the compacted file (%d bytes)", len(backup), len(compacted))
	}
	if _, err := os.Stat(dest + ".jedtmp"); !os.IsNotExist(err) {
		t.Fatalf("the backup's temp file was left behind")
	}
}

func TestPageIdenticalBackupCopiesReachablePagesAtTheirIndices(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "db.jed")
	db := backupFixture(t, path)
	defer db.Close()
	dest := filepath.Join(dir, "backup.jed")
	if err := BackupForTesting(db, dest, true); err != nil {
		t.Fatal(err)
	}
	source, mt := committedImage(t, path)
	if mt.freeListHead == 0 {
		t.Fatalf("the fixture must persist a free list")
	}
	backup := readFile(t, dest)
	if len(backup) != len(source) {
		t.Fatalf("backup is %d bytes, the snapshot spans %d", len(backup), len(source))
	}
	pages, err := db.core.backupPages(db.core.storage.paging, mt)
	if err != nil {
		t.Fatal(err)
	}
	reached := map[uint32]bool{}
	for _, p := range pages {
		reached[p] = true
	}
	zero := make([]byte, compactPage)
	copied, zeroed := 0, 0
	for p := uint32(2); p < mt.pageCount; p++ {
		got := backup[int(p)*compactPage : int(p+1)*compactPage]
		if reached[p] {
			copied++
			if !bytes.Equal(got, source[int(p)*compactPage:int(p+1)*compactPage]) {
				t.Fatalf("page %d differs from the source", p)
			}
		} else {
			zeroed++
			if !bytes.Equal(got, zero) {
				t.Fatalf("unreachable page %d is not zero", p)
			}
		}
	}
	if zeroed == 0 {
		t.Fatalf("the fixture must leave dead pages")
	}
	bootstrap := metaPage(compactPage, mt.txid, mt.rootPage, mt.pageCount, mt.freeListHead, mt.livePages)
	if !bytes.Equal(backup[:compactPage], bootstrap) || !bytes.Equal(backup[compactPage:2*compactPage], bootstrap) {
		t.Fatalf("both meta slots must carry the bootstrap meta")
	}

	// The copy opens as the same database at the same version, with the same rows and costs.
	copyDB, err := OpenDatabaseWithOptions(dest, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer copyDB.Close()
	if copyDB.Txid() != mt.txid {
		t.Fatalf("the copy is at txid %d, the snapshot at %d", copyDB.Txid(), mt.txid)
	}
	for _, sql := range []string{compactProbe, "SELECT count(*), sum(length(body)) FROM big", "SELECT count(*) FROM spans WHERE r && i64range(100, 105)"} {
		wantRows, wantCost := rowsAndCost(t, db, sql)
		gotRows, gotCost := rowsAndCost(t, copyDB, sql)
		if !reflect.DeepEqual(gotRows, wantRows) || gotCost != wantCost {
			t.Fatalf("%q: copy %v (cost %d), source %v (cost %d)", sql, gotRows, gotCost, wantRows, wantCost)
		}
	}
	// It keeps the source's free list and stays writable.
	if _, err := copyDB.Exec(t.Context(), "INSERT INTO t VALUES (9001, 'tail', 1)"); err != nil {
		t.Fatal(err)
	}
}

func TestBackupDoesNotWaitForTheWriterOrReaders(t *testing.T) {
	dir := t.TempDir()
	db := backupFixture(t, filepath.Join(dir, "db.jed"))
	defer db.Close()
	writer := db.Session(SessionOptions{})
	defer writer.Close()
	if err := writer.Begin(true); err != nil {
		t.Fatal(err)
	}
	if _, err := writer.Exec(t.Context(), "INSERT INTO t VALUES (5000, 'pending', 1)"); err != nil {
		t.Fatal(err)
	}
	reader := db.ReadSession()
	defer reader.Close()
	if _, err := queryOutcome(reader, "SELECT count(*) FROM t", nil); err != nil {
		t.Fatal(err)
	}
	for i, pageIdentical := range []bool{true, false} {
		dest := filepath.Join(dir, "backup"+string(rune('a'+i))+".jed")
		if err := BackupForTesting(db, dest, pageIdentical); err != nil {
			t.Fatalf("backup (page-identical %v) under an open writer: %v", pageIdentical, err)
		}
		copyDB, err := OpenDatabaseWithOptions(dest, OpenOptions{SkipFsync: true})
		if err != nil {
			t.Fatal(err)
		}
		if n := countRows(t, copyDB, "t"); n != 100 {
			t.Fatalf("the backup holds %d rows, want the committed 100 (not the pending insert)", n)
		}
		copyDB.Close()
	}
	if err := writer.Commit(); err != nil {
		t.Fatal(err)
	}
}

func TestBackupPreconditions(t *testing.T) {
	dir := t.TempDir()
	db := backupFixture(t, filepath.Join(dir, "db.jed"))
	defer db.Close()
	dest := filepath.Join(dir, "taken.jed")
	if err := os.WriteFile(dest, []byte("keep"), 0o644); err != nil {
		t.Fatal(err)
	}
	for _, pageIdentical := range []bool{true, false} {
		if code := backupCode(t, BackupForTesting(db, dest, pageIdentical)); code != "58P02" {
			t.Fatalf("an existing destination: %s, want 58P02", code)
		}
	}
	if got := readFile(t, dest); string(got) != "keep" {
		t.Fatalf("a refused backup overwrote its destination")
	}

	mem, _ := CreateDatabase(CreateOptions{PageSize: compactPage})
	defer mem.Close()
	if _, err := mem.Exec(t.Context(), "CREATE TABLE m (id i64 PRIMARY KEY, v text)"); err != nil {
		t.Fatal(err)
	}
	if _, err := mem.Exec(t.Context(), "INSERT INTO m SELECT g, g::text FROM generate_series(1, 50) g"); err != nil {
		t.Fatal(err)
	}
	if code := backupCode(t, BackupForTesting(mem, filepath.Join(dir, "mem.jed"), true)); code != "0A000" {
		t.Fatalf("a page-identical backup of an in-memory database: %s, want 0A000", code)
	}
	// The compacted form copies an in-memory database to a file (SQLite's VACUUM INTO from :memory:).
	memDest := filepath.Join(dir, "mem-compacted.jed")
	if err := BackupForTesting(mem, memDest, false); err != nil {
		t.Fatal(err)
	}
	copyDB, err := OpenDatabaseWithOptions(memDest, OpenOptions{SkipFsync: true})
	if err != nil {
		t.Fatal(err)
	}
	defer copyDB.Close()
	if n := countRows(t, copyDB, "m"); n != 50 {
		t.Fatalf("the in-memory backup holds %d rows, want 50", n)
	}
}
