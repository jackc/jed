package jed

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

// textTableDB has a text column — exercises the value codec's text branch (u16 length +
// UTF-8 bytes): the empty string, an embedded quote, a 2-byte char (é), a NULL text value,
// and a 4-byte astral char (😀). The PK stays int32 (no text key this slice).
func textTableDB(t *testing.T) *Database {
	db := NewDatabase()
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, s text)")
	run(t, db, "INSERT INTO t VALUES (1, 'alice')")
	run(t, db, "INSERT INTO t VALUES (2, '')")
	run(t, db, "INSERT INTO t VALUES (3, 'O''Brien')")
	run(t, db, "INSERT INTO t VALUES (4, 'café')")
	run(t, db, "INSERT INTO t VALUES (5, NULL)")
	run(t, db, "INSERT INTO t VALUES (6, '😀')")
	return db
}

// boolTableDB has a boolean column — exercises the value codec's boolean branch (a single
// bool-byte, 0x00 false / 0x01 true) plus a NULL boolean. The PK stays int32 (no boolean
// key this slice).
func boolTableDB(t *testing.T) *Database {
	db := NewDatabase()
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, flag boolean)")
	run(t, db, "INSERT INTO t VALUES (1, TRUE)")
	run(t, db, "INSERT INTO t VALUES (2, FALSE)")
	run(t, db, "INSERT INTO t VALUES (3, NULL)")
	return db
}

// decimalTableDB has a decimal column — exercises the value codec's decimal branch (flags +
// u16 scale + u16 ndigits + base-10^4 groups) and the catalog typmod: an unconstrained numeric
// column `d` and a constrained numeric(10,2) column `m` (whose values are already at scale 2,
// so storing them is a no-op coercion). Covers positive, negative, zero, a multi-group
// coefficient, and a NULL. The PK stays int32 (no decimal key this slice).
func decimalTableDB(t *testing.T) *Database {
	db := NewDatabase()
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, d numeric, m numeric(10,2))")
	run(t, db, "INSERT INTO t VALUES (1, 1.50, 1.50), (2, -12345.6789, -12.34), "+
		"(3, 0.00, 0.00), (4, 100000000.000001, 100.00), (5, NULL, NULL)")
	return db
}

// byteaTableDB exercises the value codec's bytea branch (u16 length + raw bytes): a multi-
// byte value (a-f hex), the empty byte string, embedded 0x00 bytes, a high byte (0xFF), a
// NULL, and a lone 0x00. The PK stays int32 (no bytea key this slice). Literals are the `\x`
// hex input form, adapting to the bytea column (spec/design/types.md §6).
func byteaTableDB(t *testing.T) *Database {
	db := NewDatabase()
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, b bytea)")
	run(t, db, `INSERT INTO t VALUES (1, '\xdeadbeef')`)
	run(t, db, `INSERT INTO t VALUES (2, '\x')`)
	run(t, db, `INSERT INTO t VALUES (3, '\x000102')`)
	run(t, db, `INSERT INTO t VALUES (4, '\xff')`)
	run(t, db, "INSERT INTO t VALUES (5, NULL)")
	run(t, db, `INSERT INTO t VALUES (6, '\x00')`)
	return db
}

// uuidTableDB has a uuid PRIMARY KEY (the first golden with a NON-integer stored key — the
// load-bearing §8 cross-core key-path proof) plus a nullable uuid column. Exercises the value
// codec's fixed-16-byte uuid branch (no length prefix), the uuid key encoding (bare 16 bytes),
// a present and a NULL uuid value, and the nil/max boundary UUIDs. Must match the Ruby
// reference's UUID_TABLE (spec/fileformat/verify.rb).
func uuidTableDB(t *testing.T) *Database {
	db := NewDatabase()
	run(t, db, "CREATE TABLE t (id uuid PRIMARY KEY, ref uuid)")
	run(t, db, "INSERT INTO t VALUES "+
		"('00000000-0000-0000-0000-000000000000', '550e8400-e29b-41d4-a716-446655440000'), "+
		"('550e8400-e29b-41d4-a716-446655440000', NULL), "+
		"('f47ac10b-58cc-4372-a567-0e02b2c3d479', '00000000-0000-0000-0000-000000000000'), "+
		"('ffffffff-ffff-ffff-ffff-ffffffffffff', 'ffffffff-ffff-ffff-ffff-ffffffffffff')")
	return db
}

// defaultTableDB exercises the DEFAULT column constraint on disk — the catalog flags bit2 + the
// pre-evaluated default value (written after the typmod). Covers an int default, a text default,
// a DEFAULT NULL, a NOT NULL column with a default, a decimal default coerced to numeric(6,2),
// and a plain no-default column. Row 1 takes every default; row 2 provides all values.
func defaultTableDB(t *testing.T) *Database {
	db := NewDatabase()
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, n int32 DEFAULT 0, note text DEFAULT 'none', "+
		"maybe int32 DEFAULT NULL, req int32 NOT NULL DEFAULT 7, amt numeric(6,2) DEFAULT 1.5, plain int16)")
	run(t, db, "INSERT INTO t (id) VALUES (1)")
	run(t, db, "INSERT INTO t VALUES (2, 42, 'hi', 5, 9, 2.00, 100)")
	return db
}

// WRITE side: serializing the in-memory database reproduces the golden byte-exactly.
func TestWriteMatchesGoldens(t *testing.T) {
	cases := []struct {
		name  string
		build func(*testing.T) *Database
	}{
		{"empty_db.jed", func(*testing.T) *Database { return NewDatabase() }},
		{"one_table_empty.jed", oneTableEmptyDB},
		{"pk_table.jed", pkTableDB},
		{"text_table.jed", textTableDB},
		{"bool_table.jed", boolTableDB},
		{"decimal_table.jed", decimalTableDB},
		{"bytea_table.jed", byteaTableDB},
		{"uuid_table.jed", uuidTableDB},
		{"default_table.jed", defaultTableDB},
		{"nopk_table.jed", nopkTableDB},
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
		{"one_table_empty.jed", oneTableEmptyDB, "t"},
		{"pk_table.jed", pkTableDB, "t"},
		{"text_table.jed", textTableDB, "t"},
		{"bool_table.jed", boolTableDB, "t"},
		{"decimal_table.jed", decimalTableDB, "t"},
		{"bytea_table.jed", byteaTableDB, "t"},
		{"uuid_table.jed", uuidTableDB, "t"},
		{"default_table.jed", defaultTableDB, "t"},
		{"nopk_table.jed", nopkTableDB, "r"},
		{"torn_meta_slot0.jed", pkTableDB, "t"},
		{"torn_meta_slot1.jed", pkTableDB, "t"},
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
	empty, err := LoadDatabase(fixture(t, "empty_db.jed"))
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
	loaded, err := LoadDatabase(fixture(t, "pk_table.jed"))
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
	if rows[2][0].IsNull() || rows[2][0].Int != 3 || !rows[2][1].IsNull() {
		t.Errorf("row 3 should be (3, NULL), got %v", rows[2])
	}
}

// A no-PK table's monotonic rowid counter must be reconstructed on load, so inserts
// after a load don't collide with persisted rowids (the step-6 mutation fix).
func TestRowidCounterSurvivesLoad(t *testing.T) {
	db := nopkTableDB(t) // existing rows take rowids 0, 1, 2
	image, err := db.ToImage(8192, 1)
	if err != nil {
		t.Fatalf("serialize: %v", err)
	}
	loaded, err := LoadDatabase(image)
	if err != nil {
		t.Fatalf("load: %v", err)
	}
	// The next insert must get rowid 3, not 0 — otherwise it collides (23505).
	if _, err := Execute(loaded, "INSERT INTO r VALUES (10, 100)"); err != nil {
		t.Fatalf("insert after load should not collide: %v", err)
	}
	if got := len(loaded.RowsInKeyOrder("r")); got != 4 {
		t.Errorf("expected 4 rows after load+insert, got %d", got)
	}
}

// A column DEFAULT survives serialize→load: a fresh INSERT omitting the defaulted columns
// applies the *persisted* defaults — proving the default value (not just its byte length)
// round-trips through the catalog (constraints.md §2).
func TestDefaultSurvivesLoad(t *testing.T) {
	loaded, err := LoadDatabase(fixture(t, "default_table.jed"))
	if err != nil {
		t.Fatalf("load default_table: %v", err)
	}
	run(t, loaded, "INSERT INTO t (id) VALUES (3)")
	rows := loaded.RowsInKeyOrder("t")
	last := rows[len(rows)-1]
	// id=3 takes every persisted default: n=0, note='none', maybe=NULL, req=7, plain=NULL.
	if last[0].Int != 3 || last[1].Int != 0 || last[2].Str != "none" ||
		last[3].Kind != ValNull || last[4].Int != 7 || last[6].Kind != ValNull {
		t.Errorf("persisted defaults not applied: %v", last)
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
