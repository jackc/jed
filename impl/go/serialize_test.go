package jed

// Database ↔ bytes — Database.Serialize / DeserializeDatabase (spec/design/api.md §2.8). Both are
// host-API acts the SQL corpus cannot reach, so their contract lives here (CLAUDE.md §10): an export
// is the from-scratch image of the committed snapshot at its version, it round-trips rows and costs, a
// raw file's bytes load, the loaded copy is independent of the bytes and of the source, and the error
// cases are 42704/0A000/55000/XX001. Mirrors impl/rust/tests/serialize.rs and
// impl/ts/tests/serialize.test.ts.

import (
	"bytes"
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

func serializeMemory(t *testing.T) *Database {
	t.Helper()
	db, err := CreateDatabase(CreateOptions{PageSize: compactPage})
	if err != nil {
		t.Fatal(err)
	}
	return db
}

func mustSerialize(t *testing.T, db *Database, name string) []byte {
	t.Helper()
	out, err := db.Serialize(name)
	if err != nil {
		t.Fatalf("serialize %s: %v", name, err)
	}
	return out
}

func mustDeserialize(t *testing.T, data []byte, opts DeserializeOptions) *Database {
	t.Helper()
	db, err := DeserializeDatabase(data, opts)
	if err != nil {
		t.Fatalf("deserialize: %v", err)
	}
	return db
}

func serializeExec(t *testing.T, db *Database, sql string) {
	t.Helper()
	if _, err := queryOutcome(db, sql, nil); err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
}

func TestSerializeRoundTripKeepsRowsCostsAndVersion(t *testing.T) {
	db := serializeMemory(t)
	growAndShrink(t, db)
	data := mustSerialize(t, db, "main")
	if len(data)%compactPage != 0 || uint64(len(data)) >= uint64(db.core.storage.pageCount)*compactPage {
		t.Fatalf("export of %d bytes is not a clean image (page count %d)", len(data), db.core.storage.pageCount)
	}
	image, err := db.ToImage(compactPage, db.Version())
	if err != nil || !bytes.Equal(data, image) {
		t.Fatalf("export differs from the from-scratch image (%v)", err)
	}

	cp := mustDeserialize(t, data, DeserializeOptions{})
	if cp.Version() != db.Version() || cp.PageSize() != compactPage {
		t.Fatalf("copy version %d page %d, want %d %d", cp.Version(), cp.PageSize(), db.Version(), compactPage)
	}
	gotRows, gotCost := rowsAndCost(t, cp, compactProbe)
	wantRows, wantCost := rowsAndCost(t, db, compactProbe)
	if !reflect.DeepEqual(gotRows, wantRows) || gotCost != wantCost {
		t.Fatalf("copy probe %v/%d, want %v/%d", gotRows, gotCost, wantRows, wantCost)
	}
	if again := mustSerialize(t, cp, "main"); !bytes.Equal(again, data) {
		t.Fatal("a clean image re-exports to different bytes")
	}

	// The copy is independent of its source in both directions.
	serializeExec(t, cp, "DELETE FROM t")
	if n := countRows(t, db, "t"); n != 100 {
		t.Fatalf("source has %d rows after the copy's delete", n)
	}
	if cp.Version() != db.Version()+1 {
		t.Fatalf("copy version %d after one commit, source %d", cp.Version(), db.Version())
	}
	serializeExec(t, db, "DELETE FROM t WHERE id > 50")
	if n := countRows(t, cp, "t"); n != 0 {
		t.Fatalf("copy has %d rows", n)
	}
}

func TestSerializeExcludesUncommittedChanges(t *testing.T) {
	db := serializeMemory(t)
	s := db.Session(SessionOptions{})
	defer s.Close()
	for _, sql := range []string{"CREATE TABLE t (id i32 PRIMARY KEY)", "INSERT INTO t VALUES (1)"} {
		if _, err := queryOutcome(s, sql, nil); err != nil {
			t.Fatal(err)
		}
	}
	if err := s.Begin(true); err != nil {
		t.Fatal(err)
	}
	if _, err := queryOutcome(s, "INSERT INTO t VALUES (2)", nil); err != nil {
		t.Fatal(err)
	}
	// The writer holds the gate; the export never waits for it.
	data := mustSerialize(t, db, "main")
	if err := s.Commit(); err != nil {
		t.Fatal(err)
	}
	if n := countRows(t, mustDeserialize(t, data, DeserializeOptions{}), "t"); n != 1 {
		t.Fatalf("copy has %d rows, want the committed 1", n)
	}
}

func TestSerializeEmptyDatabase(t *testing.T) {
	data := mustSerialize(t, serializeMemory(t), "main")
	if len(data) != 3*compactPage {
		t.Fatalf("empty export is %d bytes, want two meta slots and the catalog", len(data))
	}
	cp := mustDeserialize(t, data, DeserializeOptions{})
	if rows, _ := rowsAndCost(t, cp, "SELECT count(*) FROM jed_tables"); cp.Version() != 0 || rows[0][0].Int != 0 {
		t.Fatalf("empty copy: version %d", cp.Version())
	}
}

func TestDeserializeRawFileBytes(t *testing.T) {
	path := filepath.Join(t.TempDir(), "serialize.jed")
	db := createCompactFile(t, path)
	growAndShrink(t, db)
	exported := mustSerialize(t, db, "main")
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if bytes.Equal(raw, exported) {
		t.Fatal("the file should still hold its dead pages")
	}
	cp := mustDeserialize(t, raw, DeserializeOptions{})
	if cp.Version() != db.Version() {
		t.Fatalf("copy version %d, want %d", cp.Version(), db.Version())
	}
	gotRows, gotCost := rowsAndCost(t, cp, compactProbe)
	wantRows, wantCost := rowsAndCost(t, db, compactProbe)
	if !reflect.DeepEqual(gotRows, wantRows) || gotCost != wantCost {
		t.Fatal("the copy of the raw file reads differently")
	}
	if !bytes.Equal(mustSerialize(t, cp, "main"), exported) {
		t.Fatal("the copy re-exports differently")
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestSerializeAttachmentsByName(t *testing.T) {
	db := serializeMemory(t)
	if err := db.Attach("scratch", AttachMemory(), false); err != nil {
		t.Fatal(err)
	}
	serializeExec(t, db, "CREATE TABLE scratch.u (id i32 PRIMARY KEY)")
	serializeExec(t, db, "INSERT INTO scratch.u VALUES (7)")
	cp := mustDeserialize(t, mustSerialize(t, db, "scratch"), DeserializeOptions{})
	if rows, _ := rowsAndCost(t, cp, "SELECT id FROM u"); len(rows) != 1 || rows[0][0].Int != 7 {
		t.Fatalf("attachment copy rows %v", rows)
	}
	for _, name := range []string{"nope", "temp"} {
		_, err := db.Serialize(name)
		if c := compactCode(t, err); c != "42704" {
			t.Fatalf("serialize %s: %s, want 42704", name, c)
		}
	}
}

func TestSerializeEncryptedAndMalformedInputsFail(t *testing.T) {
	path := filepath.Join(t.TempDir(), "encrypted.jed")
	db := encCreate(t, path, encKey())
	_, err := db.Serialize("main")
	if c := compactCode(t, err); c != "0A000" {
		t.Fatalf("serialize encrypted: %s, want 0A000", c)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	_, err = DeserializeDatabase(raw, DeserializeOptions{})
	if c := compactCode(t, err); c != "55000" {
		t.Fatalf("deserialize encrypted: %s, want 55000", c)
	}
	for _, data := range [][]byte{[]byte("not a database"), nil} {
		_, err = DeserializeDatabase(data, DeserializeOptions{})
		if c := compactCode(t, err); c != "XX001" {
			t.Fatalf("deserialize %q: %s, want XX001", data, c)
		}
	}
}

func TestDeserializeStorageLimit(t *testing.T) {
	db := serializeMemory(t)
	growAndShrink(t, db)
	data := mustSerialize(t, db, "main")
	cp := mustDeserialize(t, data, DeserializeOptions{MaxStorageBytes: int64(len(data))})
	_, err := queryOutcome(cp, "INSERT INTO t SELECT g, repeat('y', 300), 0 FROM generate_series(10000, 12000) g", nil)
	if c := compactCode(t, err); c != "54P06" {
		t.Fatalf("insert past the limit: %s, want 54P06", c)
	}
}
