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
	"strings"
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
	db := WithPageSize(goldenPageSize)
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
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)")
	return db
}

// compositePKTableDB has a COMPOSITE primary key (constraints.md §3) — the stored key is
// the concatenation of the members' encodings (4-byte int32 then 2-byte int16,
// encoding.md §2.3). Rows insert in ascending tuple order (the tree shape is
// order-sensitive), with a negative first component and first-component ties broken by
// the second.
func compositePKTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (a int32, b int16, v int16, PRIMARY KEY (a, b))")
	for _, abv := range [][3]int64{
		{-2, 5, 10},
		{1, 1, 20},
		{1, 2, 30},
		{1, 3, 40},
		{2, 0, 50},
		{2, 1, 60},
		{3, 7, 70},
		{3, 9, 80},
	} {
		run(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, %d, %d)", abv[0], abv[1], abv[2]))
	}
	return db
}

// checkTableDB has CHECK constraints (constraints.md §4) — exercises the v4 catalog check
// list: an auto-named single-column check, an explicitly-named multi-column check, and a
// check whose persisted text exercises the token rendering (string literal with a doubled
// quote, decimal literals, >=/<=), stored in name order
// (price_range < t_b_check < t_note_check).
func checkTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (a int PRIMARY KEY, b int CHECK (b > 0), price numeric(8,2), "+
		"CONSTRAINT price_range CHECK (price >= 0.50 AND price <= 9999.99), note text, "+
		"CHECK (note = 'ok' OR note = 'a''b'))")
	run(t, db, "INSERT INTO t VALUES (1, 5, 1.00, 'ok'), (2, NULL, 9999.99, 'a''b'), "+
		"(3, 100, 0.50, 'ok')")
	return db
}

// indexTableDB has SECONDARY INDEXES (v5 — spec/design/indexes.md): the catalog reshape +
// the index trees. The PK list order (b, a) differs from declaration order (the lifted
// composite-PK narrowing); i_u covers a nullable uuid column holding a NULL (the
// encoding.md §2.2 presence tag in stored index order — NULL last), and the unnamed index
// auto-names to t_a_b_idx. Index records have empty payloads (key only).
func indexTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (a int32, b int32, u uuid, PRIMARY KEY (b, a))")
	run(t, db, "CREATE INDEX i_u ON t (u)")
	run(t, db, "CREATE INDEX ON t (a, b)")
	run(t, db, "INSERT INTO t VALUES (1, 10, '550e8400-e29b-41d4-a716-446655440000'), "+
		"(2, 10, NULL), (3, 20, '00000000-0000-0000-0000-000000000000')")
	return db
}

// uniqueTableDB has UNIQUE indexes (v6 — the per-index flags byte, indexes.md §8):
// t_v_key (a UNIQUE constraint's auto-name) over a nullable column holding two NULLs
// (NULLS DISTINCT — both stored), the named two-column constraint wv, a CREATE UNIQUE
// INDEX uq, and the plain index nu (flags 0 beside flags 1).
func uniqueTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, v int32, w int32, "+
		"UNIQUE (v), CONSTRAINT wv UNIQUE (w, v))")
	run(t, db, "CREATE INDEX nu ON t (v)")
	run(t, db, "CREATE UNIQUE INDEX uq ON t (w)")
	run(t, db, "INSERT INTO t VALUES (1, 10, 100), (2, NULL, 200), (3, NULL, 300)")
	return db
}

// nopkTableDB has no primary key — exercises the stored synthetic int64 rowid key.
func nopkTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE r (a int16, b int64)")
	for _, ab := range [][2]int64{{7, 70}, {8, 80}, {9, 90}} {
		run(t, db, fmt.Sprintf("INSERT INTO r VALUES (%d, %d)", ab[0], ab[1]))
	}
	return db
}

// tallTreeDB's wide text padding forces a HEIGHT-2 tree (an interior node whose children are
// themselves interior nodes) at page_size 256 — exercises interior-of-interior child pointers and
// post-order page allocation across a deeper tree (spec/fileformat/format.md).
func tallTreeDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, pad text)")
	for i := int64(1); i <= 18; i++ {
		run(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, 'row-%02d-%s')", i, i, strings.Repeat("x", 48)))
	}
	return db
}

// textTableDB has a text column — exercises the value codec's text branch (u16 length +
// UTF-8 bytes): the empty string, an embedded quote, a 2-byte char (é), a NULL text value,
// and a 4-byte astral char (😀). The PK stays int32 (no text key this slice).
func textTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
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
	db := WithPageSize(goldenPageSize)
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
	db := WithPageSize(goldenPageSize)
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
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, b bytea)")
	run(t, db, `INSERT INTO t VALUES (1, '\xdeadbeef')`)
	run(t, db, `INSERT INTO t VALUES (2, '\x')`)
	run(t, db, `INSERT INTO t VALUES (3, '\x000102')`)
	run(t, db, `INSERT INTO t VALUES (4, '\xff')`)
	run(t, db, "INSERT INTO t VALUES (5, NULL)")
	run(t, db, `INSERT INTO t VALUES (6, '\x00')`)
	return db
}

// Incompressible filler (spec/fileformat/format.md "Fixtures"): xorshift32(seed "JEDB") mapped
// to a 64-char alphabet (text) or raw bytes (bytea hex literals). High-entropy, so the LZ4
// encoder never wins store-smaller and the value deterministically stays PLAIN. Mirrors
// verify.rb's filler_text/filler_bytes; each call restarts at the seed.
const fillerAlpha64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"

func fillerStep(x uint32) uint32 {
	x ^= x << 13
	x ^= x >> 17
	return x ^ (x << 5)
}

func fillerText(n int) string {
	x := uint32(0x4A454442)
	var b strings.Builder
	for i := 0; i < n; i++ {
		x = fillerStep(x)
		b.WriteByte(fillerAlpha64[x%64])
	}
	return b.String()
}

func fillerBytesHex(n int) string {
	x := uint32(0x4A454442)
	var b strings.Builder
	for i := 0; i < n; i++ {
		x = fillerStep(x)
		fmt.Fprintf(&b, "%02x", x%256)
	}
	return b.String()
}

// overflowTableDB has large INCOMPRESSIBLE text + bytea values that spill OUT-OF-LINE PLAIN to
// overflow pages (spec/design/large-values.md §12): at page_size 256 a ~600/300-byte value
// exceeds RECORD_MAX (116); compression is attempted first (Slice B) but rejected by
// store-smaller, so the record holds a 0x02 pointer and the raw bytes live in a page_type-4
// chain. Row 1 spills both columns (multi-page chains), row 2 stays inline, row 3 is NULL/NULL.
// Must match the Ruby reference's OVERFLOW_TABLE (spec/fileformat/verify.rb).
func overflowTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, body text, blob bytea)")
	run(t, db, fmt.Sprintf("INSERT INTO t VALUES (1, '%s', '\\x%s')", fillerText(600), fillerBytesHex(300)))
	run(t, db, `INSERT INTO t VALUES (2, 'small', '\xcafe')`)
	run(t, db, "INSERT INTO t VALUES (3, NULL, NULL)")
	return db
}

// compressedTableDB has large COMPRESSIBLE values exercising Slice B's forms (large-values.md
// §13, format.md "Large values", lz4.md): row 1's "x"-run text and 0xAB-run bytea both become
// 0x03 inline-compressed; row 2's half-filler/half-run text compresses to ~200 B — smaller than
// plain but still over RECORD_MAX → 0x04 external-compressed (a chain carrying the COMPRESSED
// block); row 3 stays inline-plain; row 4 is NULL/NULL. Must match the Ruby reference's
// COMPRESSED_TABLE (spec/fileformat/verify.rb).
func compressedTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, body text, blob bytea)")
	run(t, db, fmt.Sprintf("INSERT INTO t VALUES (1, '%s', '\\x%s')", strings.Repeat("x", 600), strings.Repeat("ab", 200)))
	run(t, db, fmt.Sprintf("INSERT INTO t VALUES (2, '%s%s', NULL)", fillerText(200), strings.Repeat("y", 200)))
	run(t, db, `INSERT INTO t VALUES (3, 'tiny', '\xcafe')`)
	run(t, db, "INSERT INTO t VALUES (4, NULL, NULL)")
	return db
}

// uuidTableDB has a uuid PRIMARY KEY (the first golden with a NON-integer stored key — the
// load-bearing §8 cross-core key-path proof) plus a nullable uuid column. Exercises the value
// codec's fixed-16-byte uuid branch (no length prefix), the uuid key encoding (bare 16 bytes),
// a present and a NULL uuid value, and the nil/max boundary UUIDs. Must match the Ruby
// reference's UUID_TABLE (spec/fileformat/verify.rb).
func uuidTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
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
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, n int32 DEFAULT 0, note text DEFAULT 'none', "+
		"maybe int32 DEFAULT NULL, req int32 NOT NULL DEFAULT 7, amt numeric(6,2) DEFAULT 1.5, plain int16)")
	run(t, db, "INSERT INTO t (id) VALUES (1)")
	run(t, db, "INSERT INTO t VALUES (2, 42, 'hi', 5, 9, 2.00, 100)")
	return db
}

// defaultExprTableDB exercises EXPRESSION column defaults on disk (v8) — the catalog flags bit3
// (default_is_expr) + the expr-text written after the typmod: a `uuid DEFAULT uuidv7()`, an
// `int32 DEFAULT 1 + 1`, a CONSTANT default beside them (bit2), and a plain no-default column.
// EMPTY table — the catalog encoding is the cross-core proof; the per-row evaluation is covered
// by the conformance corpus (it is nondeterministic without an injected seed).
func defaultExprTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, g uuid DEFAULT uuidv7(), n int32 DEFAULT 1 + 1, "+
		"k int32 DEFAULT 7, plain int16)")
	return db
}

// timestampTableDB exercises the value codec's int64-instant branch (type code 8): a
// positive instant, a pre-1970 negative one, a BC-era one, the ±infinity sentinels, and a
// NULL. The literals parse to the same micros the golden stores. The PK stays int32.
func timestampTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, ts timestamp)")
	run(t, db, "INSERT INTO t VALUES (1, '2024-01-01 12:00:00')")
	run(t, db, "INSERT INTO t VALUES (2, '1969-12-31 23:59:59.5')")
	run(t, db, "INSERT INTO t VALUES (3, '0001-01-01 00:00:00 BC')")
	run(t, db, "INSERT INTO t VALUES (4, '-infinity')")
	run(t, db, "INSERT INTO t VALUES (5, 'infinity')")
	run(t, db, "INSERT INTO t VALUES (6, NULL)")
	return db
}

// timestamptzTableDB exercises the same 8-byte branch under type code 9; the +05 literal
// normalizes to UTC before storage.
func timestamptzTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, ts timestamptz)")
	run(t, db, "INSERT INTO t VALUES (1, '2024-01-01 12:00:00+00')")
	run(t, db, "INSERT INTO t VALUES (2, '2024-01-01 12:00:00+05')")
	run(t, db, "INSERT INTO t VALUES (3, '1969-12-31 23:59:59.5+00')")
	run(t, db, "INSERT INTO t VALUES (4, '-infinity')")
	run(t, db, "INSERT INTO t VALUES (5, 'infinity')")
	run(t, db, "INSERT INTO t VALUES (6, NULL)")
	return db
}

// intervalTableDB exercises the value codec's fixed 16-byte interval branch (type code 11):
// a positive multi-field value, a negative value, the zero interval, a months-only '1 mon'
// vs a span-equal-but-byte-distinct '30 days', and a NULL. The bare-string literals adapt.
func intervalTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, d interval)")
	run(t, db, "INSERT INTO t VALUES (1, '1 mon 2 days 03:04:05')")
	run(t, db, "INSERT INTO t VALUES (2, '-1 day')")
	run(t, db, "INSERT INTO t VALUES (3, '0 seconds')")
	run(t, db, "INSERT INTO t VALUES (4, '1 mon')")
	run(t, db, "INSERT INTO t VALUES (5, '30 days')")
	run(t, db, "INSERT INTO t VALUES (6, NULL)")
	return db
}

// float64TableDB exercises the value codec's 8-byte IEEE branch (type code 12): a positive
// fraction, a negative value, +0 and -0 (the sign bit is preserved on disk — distinct bytes), both
// infinities, a canonicalized NaN (stored as the single quiet pattern 0x7FF8…000), a NULL, and
// Float64 max (a full mantissa). Finite values enter via bare numeric literals (decimal
// adaptation); the specials enter via typed literals in INSERT ... SELECT (a VALUES slot takes only
// bare literals this slice — float.md). PK is int32 (no float key this slice — float PK → 0A000).
func float64TableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, d float64)")
	run(t, db, "INSERT INTO t VALUES (1, 1.5)")
	run(t, db, "INSERT INTO t VALUES (2, -2.5)")
	run(t, db, "INSERT INTO t VALUES (3, 0.0)")
	run(t, db, "INSERT INTO t SELECT 4, float64 '-0'")
	run(t, db, "INSERT INTO t SELECT 5, float64 'Infinity'")
	run(t, db, "INSERT INTO t SELECT 6, float64 '-Infinity'")
	run(t, db, "INSERT INTO t SELECT 7, float64 'NaN'")
	run(t, db, "INSERT INTO t VALUES (8, NULL)")
	run(t, db, "INSERT INTO t SELECT 9, float64 '1.7976931348623157e308'")
	return db
}

// float32TableDB exercises the value codec's 4-byte IEEE branch (type code 13): the same
// special-value coverage as float64TableDB (canonicalized NaN → 0x7FC00000) plus 100.25 (exactly
// representable in binary32). PK is int32 (no float key this slice).
func float32TableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, r float32)")
	run(t, db, "INSERT INTO t VALUES (1, 1.5)")
	run(t, db, "INSERT INTO t VALUES (2, -2.5)")
	run(t, db, "INSERT INTO t VALUES (3, 0.0)")
	run(t, db, "INSERT INTO t SELECT 4, float32 '-0'")
	run(t, db, "INSERT INTO t SELECT 5, float32 'Infinity'")
	run(t, db, "INSERT INTO t SELECT 6, float32 '-Infinity'")
	run(t, db, "INSERT INTO t SELECT 7, float32 'NaN'")
	run(t, db, "INSERT INTO t VALUES (8, NULL)")
	run(t, db, "INSERT INTO t VALUES (9, 100.25)")
	return db
}

// WRITE side: serializing the in-memory database reproduces the golden byte-exactly.
func TestWriteMatchesGoldens(t *testing.T) {
	cases := []struct {
		name  string
		build func(*testing.T) *Database
	}{
		{"empty_db.jed", func(*testing.T) *Database { return WithPageSize(goldenPageSize) }},
		{"overflow_table.jed", overflowTableDB},
		{"compressed_table.jed", compressedTableDB},
		{"one_table_empty.jed", oneTableEmptyDB},
		{"pk_table.jed", pkTableDB},
		{"text_table.jed", textTableDB},
		{"bool_table.jed", boolTableDB},
		{"decimal_table.jed", decimalTableDB},
		{"bytea_table.jed", byteaTableDB},
		{"uuid_table.jed", uuidTableDB},
		{"default_table.jed", defaultTableDB},
		{"default_expr_table.jed", defaultExprTableDB},
		{"timestamp_table.jed", timestampTableDB},
		{"timestamptz_table.jed", timestamptzTableDB},
		{"interval_table.jed", intervalTableDB},
		{"float64_table.jed", float64TableDB},
		{"float32_table.jed", float32TableDB},
		{"nopk_table.jed", nopkTableDB},
		{"composite_pk_table.jed", compositePKTableDB},
		{"check_table.jed", checkTableDB},
		{"index_table.jed", indexTableDB},
		{"unique_table.jed", uniqueTableDB},
		{"tall_tree.jed", tallTreeDB},
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
		{"overflow_table.jed", overflowTableDB, "t"},
		{"compressed_table.jed", compressedTableDB, "t"},
		{"pk_table.jed", pkTableDB, "t"},
		{"text_table.jed", textTableDB, "t"},
		{"bool_table.jed", boolTableDB, "t"},
		{"decimal_table.jed", decimalTableDB, "t"},
		{"bytea_table.jed", byteaTableDB, "t"},
		{"uuid_table.jed", uuidTableDB, "t"},
		{"default_table.jed", defaultTableDB, "t"},
		{"default_expr_table.jed", defaultExprTableDB, "t"},
		{"timestamp_table.jed", timestampTableDB, "t"},
		{"timestamptz_table.jed", timestamptzTableDB, "t"},
		{"interval_table.jed", intervalTableDB, "t"},
		{"float64_table.jed", float64TableDB, "t"},
		{"float32_table.jed", float32TableDB, "t"},
		{"nopk_table.jed", nopkTableDB, "r"},
		{"composite_pk_table.jed", compositePKTableDB, "t"},
		{"check_table.jed", checkTableDB, "t"},
		{"index_table.jed", indexTableDB, "t"},
		{"unique_table.jed", uniqueTableDB, "t"},
		{"tall_tree.jed", tallTreeDB, "t"},
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
	db := nopkTableDB(t) // existing rows take rowids 0, 1, 2 (built at goldenPageSize)
	image, err := db.ToImage(goldenPageSize, 1)
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
	// Built at 8192 so the in-memory tree is sized for it (fan-out tracks the page size — format.md).
	db := WithPageSize(8192)
	run(t, db, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)")
	for i := int64(1); i <= 20; i++ {
		v := fmt.Sprintf("%d", i*10)
		if i == 3 {
			v = "NULL"
		}
		run(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, %s)", i, v))
	}
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
	if _, ok := scalarForTypeCode(14); ok {
		t.Errorf("type code 14 should be unknown")
	}
}

func TestSerializeIsDeterministic(t *testing.T) {
	db := pkTableDB(t)
	a, _ := db.ToImage(goldenPageSize, 1)
	b, _ := db.ToImage(goldenPageSize, 1)
	if !bytes.Equal(a, b) {
		t.Errorf("serializing the same database twice produced different bytes")
	}
}

func TestCorruptImageIsRejected(t *testing.T) {
	db := pkTableDB(t)
	image, _ := db.ToImage(goldenPageSize, 1)
	image[0] ^= 0xFF              // smash slot 0 magic
	image[goldenPageSize] ^= 0xFF // smash slot 1 magic
	if _, err := LoadDatabase(image); err == nil {
		t.Errorf("expected a data_corrupted error")
	} else if ee, ok := err.(*EngineError); !ok || ee.Code() != "XX001" {
		t.Errorf("expected XX001, got %v", err)
	}
}
