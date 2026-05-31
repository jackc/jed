package abide

// Golden-file cross-core test (CLAUDE.md §8). The load-bearing honesty test for the
// on-disk format: each core must (a) READ a checked-in golden into the expected
// catalog + rows, and (b) WRITE the same logical database to bytes equal to the
// golden EXACTLY. Because the format is deterministic, this gives
// rust-bytes == golden == go-bytes, so each core reads the other's output without any
// live cross-process exchange. Goldens are authored at page_size 256 by
// spec/fileformat/verify.rb (the independent reference).

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

// goldenPageSize is the (small, reviewable) page size the goldens are authored at.
const goldenPageSize = 256

func fixture(t *testing.T, name string) []byte {
	t.Helper()
	dir, _ := os.Getwd()
	for {
		p := filepath.Join(dir, "spec", "fileformat", "fixtures", name)
		if b, err := os.ReadFile(p); err == nil {
			return b
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			t.Fatalf("could not locate spec/fileformat/fixtures/%s", name)
		}
		dir = parent
	}
}

func run(t *testing.T, db *Database, sql string) {
	t.Helper()
	if _, err := Execute(db, sql); err != nil {
		t.Fatalf("%q: %v", sql, err)
	}
}

// pkTableDB is CREATE TABLE t (id int32 PRIMARY KEY, v int16) with 20 rows (id 3's v
// is NULL) — enough to span more than one data page at page_size 256.
func pkTableDB(t *testing.T) *Database {
	db := NewDatabase()
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)")
	for i := int64(1); i <= 20; i++ {
		v := fmt.Sprintf("%d", i*10)
		if i == 3 {
			v = "NULL"
		}
		run(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, %s)", i, v))
	}
	return db
}

func oneTableEmptyDB(t *testing.T) *Database {
	db := NewDatabase()
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)")
	return db
}

// nopkTableDB has no primary key — exercises the stored synthetic int64 rowid key.
func nopkTableDB(t *testing.T) *Database {
	db := NewDatabase()
	run(t, db, "CREATE TABLE r (a int16, b int64)")
	for _, ab := range [][2]int64{{7, 70}, {8, 80}, {9, 90}} {
		run(t, db, fmt.Sprintf("INSERT INTO r VALUES (%d, %d)", ab[0], ab[1]))
	}
	return db
}

// WRITE side: serializing the in-memory database reproduces the golden byte-exactly.
func TestWriteMatchesGoldens(t *testing.T) {
	cases := []struct {
		name  string
		build func(*testing.T) *Database
	}{
		{"empty_db.adb", func(*testing.T) *Database { return NewDatabase() }},
		{"one_table_empty.adb", oneTableEmptyDB},
		{"pk_table.adb", pkTableDB},
		{"nopk_table.adb", nopkTableDB},
	}
	for _, c := range cases {
		image, err := c.build(t).ToImage(goldenPageSize, 1)
		if err != nil {
			t.Fatalf("%s: serialize: %v", c.name, err)
		}
		if want := fixture(t, c.name); !bytes.Equal(image, want) {
			t.Errorf("%s: serialized bytes differ (got %d B, want %d B)", c.name, len(image), len(want))
		}
	}
}

// READ side: loading a golden reproduces the same rows the builder produced. The
// torn-meta goldens must read through the valid slot to the pk_table content.
func TestReadGoldensReproducesRows(t *testing.T) {
	cases := []struct {
		name  string
		build func(*testing.T) *Database
		table string
	}{
		{"one_table_empty.adb", oneTableEmptyDB, "t"},
		{"pk_table.adb", pkTableDB, "t"},
		{"nopk_table.adb", nopkTableDB, "r"},
		{"torn_meta_slot0.adb", pkTableDB, "t"},
		{"torn_meta_slot1.adb", pkTableDB, "t"},
	}
	for _, c := range cases {
		loaded, err := LoadDatabase(fixture(t, c.name))
		if err != nil {
			t.Fatalf("load %s: %v", c.name, err)
		}
		got := loaded.RowsInKeyOrder(c.table)
		want := c.build(t).RowsInKeyOrder(c.table)
		if !reflect.DeepEqual(got, want) {
			t.Errorf("%s: rows differ\n  got:  %v\n  want: %v", c.name, got, want)
		}
	}

	// Empty database: zero tables, and a missing table reads as absent.
	empty, err := LoadDatabase(fixture(t, "empty_db.adb"))
	if err != nil {
		t.Fatalf("load empty_db: %v", err)
	}
	if _, ok := empty.Table("t"); ok {
		t.Errorf("empty_db should have no tables")
	}
}

// READ side, catalog detail: column names, types, and flags survive exactly (a read
// bug in an unexercised flag would otherwise slip past a rows-only check).
func TestReadGoldenReconstructsCatalog(t *testing.T) {
	loaded, err := LoadDatabase(fixture(t, "pk_table.adb"))
	if err != nil {
		t.Fatalf("load pk_table: %v", err)
	}
	tbl, ok := loaded.Table("t")
	if !ok {
		t.Fatalf("table t missing")
	}
	if tbl.Name != "t" || len(tbl.Columns) != 2 {
		t.Fatalf("unexpected table shape: %+v", tbl)
	}
	id, v := tbl.Columns[0], tbl.Columns[1]
	if id.Name != "id" || id.Type != Int32 || !id.PrimaryKey || !id.NotNull {
		t.Errorf("column id wrong: %+v", id)
	}
	if v.Name != "v" || v.Type != Int16 || v.PrimaryKey || v.NotNull {
		t.Errorf("column v wrong: %+v", v)
	}
	// A NULL value round-trips (id 3's v).
	rows := loaded.RowsInKeyOrder("t")
	if rows[2][0].Null || rows[2][0].Int != 3 || !rows[2][1].Null {
		t.Errorf("row 3 should be (3, NULL), got %v", rows[2])
	}
}

// The default 8 KiB page size also round-trips (goldens stay at 256 for reviewable
// hex, but the real default must work too).
func TestRoundTripAtDefaultPageSize(t *testing.T) {
	db := pkTableDB(t)
	image, err := db.ToImage(8192, 1)
	if err != nil {
		t.Fatalf("serialize: %v", err)
	}
	loaded, err := LoadDatabase(image)
	if err != nil {
		t.Fatalf("load: %v", err)
	}
	if !reflect.DeepEqual(loaded.RowsInKeyOrder("t"), db.RowsInKeyOrder("t")) {
		t.Errorf("8 KiB round trip changed rows")
	}
	// Re-serializing the loaded database yields identical bytes (determinism).
	again, err := loaded.ToImage(8192, 1)
	if err != nil {
		t.Fatalf("re-serialize: %v", err)
	}
	if !bytes.Equal(again, image) {
		t.Errorf("re-serialized bytes differ from the original")
	}
}

// Format-internal unit tests: the CRC vector, type-code mapping, determinism.
func TestCRC32KnownVector(t *testing.T) {
	if got := crc32IEEE([]byte("123456789")); got != 0xCBF43926 {
		t.Errorf("crc32(\"123456789\") = %#08x, want 0xCBF43926", got)
	}
}

func TestTypeCodesRoundTrip(t *testing.T) {
	for _, ty := range AllScalarTypes() {
		got, ok := scalarForTypeCode(typeCodeForScalar(ty))
		if !ok || got != ty {
			t.Errorf("type code round trip failed for %v", ty)
		}
	}
	if _, ok := scalarForTypeCode(0); ok {
		t.Errorf("type code 0 (reserved) should be unknown")
	}
	if _, ok := scalarForTypeCode(9); ok {
		t.Errorf("type code 9 should be unknown")
	}
}

func TestSerializeIsDeterministic(t *testing.T) {
	db := pkTableDB(t)
	a, _ := db.ToImage(8192, 1)
	b, _ := db.ToImage(8192, 1)
	if !bytes.Equal(a, b) {
		t.Errorf("serializing the same database twice produced different bytes")
	}
}

func TestCorruptImageIsRejected(t *testing.T) {
	db := pkTableDB(t)
	image, _ := db.ToImage(8192, 1)
	image[0] ^= 0xFF    // smash slot 0 magic
	image[8192] ^= 0xFF // smash slot 1 magic
	if _, err := LoadDatabase(image); err == nil {
		t.Errorf("expected a data_corrupted error")
	} else if ee, ok := err.(*EngineError); !ok || ee.Code() != "XX001" {
		t.Errorf("expected XX001, got %v", err)
	}
}
