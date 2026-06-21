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

// pkTableDB is CREATE TABLE t (id i32 PRIMARY KEY, v i16) with 20 rows (id 3's v
// is NULL) — enough to span more than one data page at page_size 256.
func pkTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i16)")
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
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i16)")
	return db
}

// compositePKTableDB has a COMPOSITE primary key (constraints.md §3) — the stored key is
// the concatenation of the members' encodings (4-byte i32 then 2-byte i16,
// encoding.md §2.3). Rows insert in ascending tuple order (the tree shape is
// order-sensitive), with a negative first component and first-component ties broken by
// the second.
func compositePKTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (a i32, b i16, v i16, PRIMARY KEY (a, b))")
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
	run(t, db, "CREATE TABLE t (a i32, b i32, u uuid, PRIMARY KEY (b, a))")
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
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32, w i32, "+
		"UNIQUE (v), CONSTRAINT wv UNIQUE (w, v))")
	run(t, db, "CREATE INDEX nu ON t (v)")
	run(t, db, "CREATE UNIQUE INDEX uq ON t (w)")
	run(t, db, "INSERT INTO t VALUES (1, 10, 100), (2, NULL, 200), (3, NULL, 300)")
	return db
}

// fkTableDB exercises FOREIGN KEY constraints (v11 — spec/design/constraints.md §6): a child
// table c with four FKs — a default-PK reference, a named UNIQUE reference, a composite UNIQUE
// reference with ON DELETE RESTRICT, and a self-reference — pinning the catalog foreign-key list
// (the name + local/ref ordinals + actions byte). Must match the Ruby reference's FK_TABLE
// (spec/fileformat/verify.rb).
func fkTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE p (pid i32 PRIMARY KEY, code i32 UNIQUE, a i32, b i32, UNIQUE (a, b))")
	run(t, db, "INSERT INTO p VALUES (1, 100, 10, 20), (2, 200, 30, 40)")
	run(t, db, "CREATE TABLE c (id i32 PRIMARY KEY, pid i32, pcode i32, x i32, y i32, mgr i32, "+
		"FOREIGN KEY (pid) REFERENCES p (pid), "+
		"CONSTRAINT c_code_fk FOREIGN KEY (pcode) REFERENCES p (code), "+
		"FOREIGN KEY (x, y) REFERENCES p (a, b) ON DELETE RESTRICT, "+
		"FOREIGN KEY (mgr) REFERENCES c (id))")
	run(t, db, "INSERT INTO c VALUES (10, 1, 100, 10, 20, NULL), (11, 2, 200, 30, 40, 10)")
	return db
}

// arrayTableDB has ARRAY (T[]) columns (v10 — spec/design/array.md): pins the catalog array-column
// entry (type_code 15 + the element-type descriptor, §3) and the compact value body (§4). An
// i32[] (fixed-width elements: no per-element length prefix) and a text[]; row 2 has an EMPTY
// array (ndim=0), row 3 a NULL element (the HAS_NULLS bitmap) and a whole-value NULL array (the
// lone 0x01 tag). Must match the Ruby reference's ARRAY_TABLE (spec/fileformat/verify.rb).
func arrayTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, xs i32[], tags text[])")
	run(t, db, "INSERT INTO t VALUES (1, ARRAY[10, 20, 30], ARRAY['a', 'b'])")
	run(t, db, "INSERT INTO t VALUES (2, '{40,50}', '{}')")
	run(t, db, "INSERT INTO t VALUES (3, ARRAY[1, NULL, 3], NULL)")
	// Row 4 pins the §12 shapes: a 2-D i32[] and a custom-lower-bound text[] (the lb i32 field).
	run(t, db, "INSERT INTO t VALUES (4, ARRAY[ARRAY[10,20],ARRAY[30,40]], '[2:3]={x,y}')")
	return db
}

// rangeTableDB is CREATE TABLE t (id i32 PRIMARY KEY, r i32range, br i64range) with rows exercising
// every range flags bit + the canonical-[) storage (spec/design/ranges.md §4): a finite [), an
// inclusive-upper literal that canonicalizes ([1,5] → [1,6)), the EMPTY range, infinite bounds
// (lower-only, both), a NULL range, an exclusive-lower literal with infinite upper ((5,) → [6,)), and
// a singleton ([1,1] → [1,2)). Pins range_table.jed cross-core.
func rangeTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, r i32range, br i64range)")
	run(t, db, "INSERT INTO t VALUES (1, '[1,5)', '[10,20)')")
	run(t, db, "INSERT INTO t VALUES (2, '[1,5]', NULL)")
	run(t, db, "INSERT INTO t VALUES (3, 'empty', '(,100)')")
	run(t, db, "INSERT INTO t VALUES (4, '(,)', '(5,)')")
	run(t, db, "INSERT INTO t VALUES (5, NULL, '[1,1]')")
	return db
}

// rangePKTableDB: an i32range PRIMARY KEY — the first CONTAINER key (encoding.md §2.11). The
// range-bounds key (empty/±∞/inclusivity framing around the i32 element key) lands in the key slot.
// Rows are inserted in ASCENDING range_total_cmp order to match verify.rb's ascending-key tree builder.
func rangePKTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (k i32range PRIMARY KEY, v i32)")
	run(t, db, "INSERT INTO t VALUES ('empty', 0)")
	run(t, db, "INSERT INTO t VALUES ('(,5)', 1)")
	run(t, db, "INSERT INTO t VALUES ('(,)', 2)")
	run(t, db, "INSERT INTO t VALUES ('[1,5)', 3)")
	run(t, db, "INSERT INTO t VALUES ('[2,4)', 4)")
	run(t, db, "INSERT INTO t VALUES ('[2,)', 5)")
	return db
}

// ginArrayTableDB has a GIN inverted index (v13 — the per-index index_kind byte, spec/design/gin.md):
// i_nums_gin over an i32[] column (kind 1) beside an ordinary ordered index i_n over a scalar
// column (kind 0 — a btree index cannot sit on the array column). Rows exercise term dedup (row 2's
// duplicate 20), an empty and a NULL whole-value array (rows 3/4 → no entries), and a NULL element
// (row 5). Rows are inserted before the indexes so each builds via the sorted-bulk path, matching
// the Ruby reference's GIN_ARRAY_TABLE.
func ginArrayTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, nums i32[], n i32)")
	run(t, db, "INSERT INTO t VALUES (1, '{10,20,30}', 1), (2, '{20,20,40}', 2), (3, '{}', 3), (4, NULL, 4), (5, '{10,NULL,50}', 5)")
	run(t, db, "CREATE INDEX i_n ON t (n)")
	run(t, db, "CREATE INDEX i_nums_gin ON t USING gin (nums)")
	return db
}

// ginUuidTableDB has a GIN index over a uuid[] column (kind 1) — the non-integer GIN element-type
// golden (spec/design/gin.md §3/§4): each GIN term is the element's 16-byte uuid-raw16 key encoding,
// so entries are encode_uuid(term) ‖ storage_key (empty payload). Rows mirror ginArrayTableDB: term
// dedup (row 2's duplicate bb), an empty and a NULL whole-value array (rows 3/4 → no entries), and a
// NULL element (row 5). An ordinary ordered index i_n sits beside it (kind 0).
func ginUuidTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, tags uuid[], n i32)")
	run(t, db, "INSERT INTO t VALUES "+
		"(1, '{00000000-0000-0000-0000-0000000000aa,00000000-0000-0000-0000-0000000000bb,00000000-0000-0000-0000-0000000000cc}', 1), "+
		"(2, '{00000000-0000-0000-0000-0000000000bb,00000000-0000-0000-0000-0000000000bb,00000000-0000-0000-0000-0000000000dd}', 2), "+
		"(3, '{}', 3), "+
		"(4, NULL, 4), "+
		"(5, '{00000000-0000-0000-0000-0000000000aa,NULL,00000000-0000-0000-0000-0000000000ee}', 5)")
	run(t, db, "CREATE INDEX i_n ON t (n)")
	run(t, db, "CREATE INDEX i_tags_gin ON t USING gin (tags)")
	return db
}

// nopkTableDB has no primary key — exercises the stored synthetic i64 rowid key.
func nopkTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE r (a i16, b i64)")
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
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, pad text)")
	for i := int64(1); i <= 18; i++ {
		run(t, db, fmt.Sprintf("INSERT INTO t VALUES (%d, 'row-%02d-%s')", i, i, strings.Repeat("x", 48)))
	}
	return db
}

// textTableDB has a text column — exercises the value codec's text branch (u16 length +
// UTF-8 bytes): the empty string, an embedded quote, a 2-byte char (é), a NULL text value,
// and a 4-byte astral char (😀). The PK stays i32 (no text key this slice).
func textTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, s text)")
	run(t, db, "INSERT INTO t VALUES (1, 'alice')")
	run(t, db, "INSERT INTO t VALUES (2, '')")
	run(t, db, "INSERT INTO t VALUES (3, 'O''Brien')")
	run(t, db, "INSERT INTO t VALUES (4, 'café')")
	run(t, db, "INSERT INTO t VALUES (5, NULL)")
	run(t, db, "INSERT INTO t VALUES (6, '😀')")
	return db
}

// boolTableDB has a boolean column — exercises the value codec's boolean branch (a single
// bool-byte, 0x00 false / 0x01 true) plus a NULL boolean. The PK stays i32 (the boolean
// PRIMARY KEY case is boolPKTableDB).
func boolTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, flag boolean)")
	run(t, db, "INSERT INTO t VALUES (1, TRUE)")
	run(t, db, "INSERT INTO t VALUES (2, FALSE)")
	run(t, db, "INSERT INTO t VALUES (3, NULL)")
	return db
}

// boolPKTableDB has a boolean PRIMARY KEY (the second golden with a NON-integer stored key,
// after uuid) — the bool-byte key encoding (bare 1 byte 0x00 false / 0x01 true, no presence
// tag since a PK is NOT NULL, spec/design/encoding.md §2.9), plus a nullable boolean value
// column. Rows go in via INSERT and the store sorts them into key (byte) order: false (0x00)
// then true (0x01). Must match spec/fileformat/verify.rb's BOOL_PK_TABLE.
func boolPKTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (k boolean PRIMARY KEY, v boolean)")
	run(t, db, "INSERT INTO t VALUES (FALSE, TRUE)")
	run(t, db, "INSERT INTO t VALUES (TRUE, NULL)")
	return db
}

// textPKTableDB is the first golden with a VARIABLE-WIDTH non-integer stored key — the
// text-terminated-escape encoding (encoding.md §2.4). The store sorts rows into key (code-point /
// byte) order: "" < "Zeta"(0x5A) < "apple"(0x61) < "banana"(0x62) < "é"(0xC3). Must match
// spec/fileformat/verify.rb's TEXT_PK_TABLE.
func textPKTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (k text PRIMARY KEY, v i32)")
	run(t, db, "INSERT INTO t VALUES ('', 4)")
	run(t, db, "INSERT INTO t VALUES ('Zeta', NULL)")
	run(t, db, "INSERT INTO t VALUES ('apple', 2)")
	run(t, db, "INSERT INTO t VALUES ('banana', 3)")
	run(t, db, "INSERT INTO t VALUES ('é', 5)")
	return db
}

// byteaPKTableDB is the bytea-terminated-escape key encoding (encoding.md §2.6) — like text but
// over raw bytes, so the embedded-0x00 escape is exercised. The store sorts into unsigned-byte
// (key) order: ” < \x00 < \x61 < \x6100ff62 < \x6161 < \x62. Must match BYTEA_PK_TABLE.
func byteaPKTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (k bytea PRIMARY KEY, v i32)")
	run(t, db, `INSERT INTO t VALUES ('\x', 5)`)
	run(t, db, `INSERT INTO t VALUES ('\x00', 6)`)
	run(t, db, `INSERT INTO t VALUES ('\x61', 1)`)
	run(t, db, `INSERT INTO t VALUES ('\x6100ff62', 4)`)
	run(t, db, `INSERT INTO t VALUES ('\x6161', 2)`)
	run(t, db, `INSERT INTO t VALUES ('\x62', 3)`)
	return db
}

// decimalPKTableDB is the first golden with a VARIABLE-WIDTH SIGNED stored key — the
// decimal-order-preserving encoding (encoding.md §2.5). The store sorts into numeric (= key)
// order: -2.5 < -0.5 < 0 < 0.25 < 1.5 < 10 < 100.50; "100.50" stores scale 2 in its value body
// but normalizes in the key. Must match spec/fileformat/verify.rb's DECIMAL_PK_TABLE.
func decimalPKTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (k decimal PRIMARY KEY, v i32)")
	run(t, db, "INSERT INTO t VALUES (-2.5, 6)")
	run(t, db, "INSERT INTO t VALUES (-0.5, 5)")
	run(t, db, "INSERT INTO t VALUES (0, 4)")
	run(t, db, "INSERT INTO t VALUES (0.25, 1)")
	run(t, db, "INSERT INTO t VALUES (1.5, 2)")
	run(t, db, "INSERT INTO t VALUES (10, 3)")
	run(t, db, "INSERT INTO t VALUES (100.50, 7)")
	return db
}

// decimalTableDB has a decimal column — exercises the value codec's decimal branch (flags +
// u16 scale + u16 ndigits + base-10^4 groups) and the catalog typmod: an unconstrained numeric
// column `d` and a constrained numeric(10,2) column `m` (whose values are already at scale 2,
// so storing them is a no-op coercion). Covers positive, negative, zero, a multi-group
// coefficient, and a NULL. The PK stays i32 (no decimal key this slice).
func decimalTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, d numeric, m numeric(10,2))")
	run(t, db, "INSERT INTO t VALUES (1, 1.50, 1.50), (2, -12345.6789, -12.34), "+
		"(3, 0.00, 0.00), (4, 100000000.000001, 100.00), (5, NULL, NULL)")
	return db
}

// byteaTableDB exercises the value codec's bytea branch (u16 length + raw bytes): a multi-
// byte value (a-f hex), the empty byte string, embedded 0x00 bytes, a high byte (0xFF), a
// NULL, and a lone 0x00. The PK stays i32 (no bytea key this slice). Literals are the `\x`
// hex input form, adapting to the bytea column (spec/design/types.md §6).
func byteaTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, b bytea)")
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
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, body text, blob bytea)")
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
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, body text, blob bytea)")
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
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, n i32 DEFAULT 0, note text DEFAULT 'none', "+
		"maybe i32 DEFAULT NULL, req i32 NOT NULL DEFAULT 7, amt numeric(6,2) DEFAULT 1.5, plain i16)")
	run(t, db, "INSERT INTO t (id) VALUES (1)")
	run(t, db, "INSERT INTO t VALUES (2, 42, 'hi', 5, 9, 2.00, 100)")
	return db
}

// defaultExprTableDB exercises EXPRESSION column defaults on disk (v8) — the catalog flags bit3
// (default_is_expr) + the expr-text written after the typmod: a `uuid DEFAULT uuidv7()`, an
// `i32 DEFAULT 1 + 1`, a CONSTANT default beside them (bit2), and a plain no-default column.
// EMPTY table — the catalog encoding is the cross-core proof; the per-row evaluation is covered
// by the conformance corpus (it is nondeterministic without an injected seed).
func defaultExprTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, g uuid DEFAULT uuidv7(), n i32 DEFAULT 1 + 1, "+
		"k i32 DEFAULT 7, plain i16)")
	return db
}

// timestampTableDB exercises the value codec's i64-instant branch (type code 8): a
// positive instant, a pre-1970 negative one, a BC-era one, the ±infinity sentinels, and a
// NULL. The literals parse to the same micros the golden stores. The PK stays i32.
func timestampTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, ts timestamp)")
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
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, ts timestamptz)")
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
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, d interval)")
	run(t, db, "INSERT INTO t VALUES (1, '1 mon 2 days 03:04:05')")
	run(t, db, "INSERT INTO t VALUES (2, '-1 day')")
	run(t, db, "INSERT INTO t VALUES (3, '0 seconds')")
	run(t, db, "INSERT INTO t VALUES (4, '1 mon')")
	run(t, db, "INSERT INTO t VALUES (5, '30 days')")
	run(t, db, "INSERT INTO t VALUES (6, NULL)")
	return db
}

// intervalPKTableDB is a golden with a fixed-width SIGNED 16-byte stored key — the
// interval-span-i128 encoding (encoding.md §2.10). Rows store in canonical-span (= key) order:
// -1 mon < -1 day < 0 < 1 sec < 1 day < 1 mon < 100 years; all spans distinct (span-equal
// intervals collide on the span key). Inserted in ascending key order to match verify.rb's
// build_tree (the split shape is order-sensitive); the out-of-order proof is in the conformance test.
func intervalPKTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (k interval PRIMARY KEY, v i32)")
	run(t, db, "INSERT INTO t VALUES ('-1 mon', 6)")
	run(t, db, "INSERT INTO t VALUES ('-1 day', 5)")
	run(t, db, "INSERT INTO t VALUES ('0 seconds', 4)")
	run(t, db, "INSERT INTO t VALUES ('1 sec', 1)")
	run(t, db, "INSERT INTO t VALUES ('1 day', 2)")
	run(t, db, "INSERT INTO t VALUES ('1 mon', 3)")
	run(t, db, "INSERT INTO t VALUES ('100 years', 7)")
	return db
}

// float64TableDB exercises the value codec's 8-byte IEEE branch (type code 12): a positive
// fraction, a negative value, +0 and -0 (the sign bit is preserved on disk — distinct bytes), both
// infinities, a canonicalized NaN (stored as the single quiet pattern 0x7FF8…000), a NULL, and
// Float64 max (a full mantissa). Finite values enter via bare numeric literals (decimal
// adaptation); the specials enter via typed literals in INSERT ... SELECT (a VALUES slot takes only
// bare literals this slice — float.md). PK is i32 (no float key this slice — float PK → 0A000).
func float64TableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, d f64)")
	run(t, db, "INSERT INTO t VALUES (1, 1.5)")
	run(t, db, "INSERT INTO t VALUES (2, -2.5)")
	run(t, db, "INSERT INTO t VALUES (3, 0.0)")
	run(t, db, "INSERT INTO t SELECT 4, f64 '-0'")
	run(t, db, "INSERT INTO t SELECT 5, f64 'Infinity'")
	run(t, db, "INSERT INTO t SELECT 6, f64 '-Infinity'")
	run(t, db, "INSERT INTO t SELECT 7, f64 'NaN'")
	run(t, db, "INSERT INTO t VALUES (8, NULL)")
	run(t, db, "INSERT INTO t SELECT 9, f64 '1.7976931348623157e308'")
	return db
}

// float32TableDB exercises the value codec's 4-byte IEEE branch (type code 13): the same
// special-value coverage as float64TableDB (canonicalized NaN → 0x7FC00000) plus 100.25 (exactly
// representable in binary32). PK is i32 (no float key this slice).
func float32TableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, r f32)")
	run(t, db, "INSERT INTO t VALUES (1, 1.5)")
	run(t, db, "INSERT INTO t VALUES (2, -2.5)")
	run(t, db, "INSERT INTO t VALUES (3, 0.0)")
	run(t, db, "INSERT INTO t SELECT 4, f32 '-0'")
	run(t, db, "INSERT INTO t SELECT 5, f32 'Infinity'")
	run(t, db, "INSERT INTO t SELECT 6, f32 '-Infinity'")
	run(t, db, "INSERT INTO t SELECT 7, f32 'NaN'")
	run(t, db, "INSERT INTO t VALUES (8, NULL)")
	run(t, db, "INSERT INTO t VALUES (9, 100.25)")
	return db
}

// dateTableDB exercises the value codec's date branch (type code 16): the 4-byte i32 day-count
// body (same int-be-signflip codec as i32). A positive date, a pre-1970 negative one, a BC-era
// one, the −infinity/+infinity sentinels (i32 min/max), and a NULL. The bare-string literals adapt
// to the date column. PK is i32 (spec/design/date.md).
func dateTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, d date)")
	run(t, db, "INSERT INTO t VALUES (1, '2024-01-15')")
	run(t, db, "INSERT INTO t VALUES (2, '1969-12-31')")
	run(t, db, "INSERT INTO t VALUES (3, '0044-03-15 BC')")
	run(t, db, "INSERT INTO t VALUES (4, '-infinity')")
	run(t, db, "INSERT INTO t VALUES (5, 'infinity')")
	run(t, db, "INSERT INTO t VALUES (6, NULL)")
	return db
}

// compositeTypeTableDB has a composite TYPE defined + persisted (v9) AND used by a column with
// stored values (S3): pins the recursive value codec — the null bitmap, a present-field body, and a
// NULL field's zero-byte omission (row 2's zip) — spec/design/composite.md §4.
func compositeTypeTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TYPE addr AS (street text NOT NULL, zip i32)")
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, home addr)")
	run(t, db, "INSERT INTO t VALUES (1, ROW('Main', 90210))")
	run(t, db, "INSERT INTO t VALUES (2, ROW('Oak', NULL))")
	return db
}

// arrayCompositeTableDB has a composite type used as an array ELEMENT type (array-of-composite,
// array.md §12 AC1): the catalog array-column entry carries a composite element descriptor
// (element_type_code 14 + "addr") and the value body recurses (an array body whose elements are
// composite bodies). Row 2's element has a NULL `zip` field (the composite null-bitmap inside an
// element); row 3 mixes a present composite element with a NULL element (the array HAS_NULLS bitmap).
func arrayCompositeTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TYPE addr AS (street text NOT NULL, zip i32)")
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, items addr[])")
	run(t, db, `INSERT INTO t VALUES (1, '{"(Main,90210)","(Side,5)"}')`)
	run(t, db, `INSERT INTO t VALUES (2, '{"(Oak,)"}')`)
	run(t, db, `INSERT INTO t VALUES (3, '{"(A,1)",NULL}')`)
	run(t, db, "INSERT INTO t VALUES (4, '{}')")
	run(t, db, "INSERT INTO t VALUES (5, NULL)")
	return db
}

// compositeArrayFieldTableDB has a composite type with an array-typed FIELD (array.md §12 — the
// mirror of array-of-composite): the catalog composite-type entry carries a code-15 array field
// (element_type_code 2 = i32) and the value body recurses (a composite body whose pts field is an
// array body). Row 2 has an empty array field {} (ndim 0); row 3 a NULL array field (the composite
// null-bitmap).
func compositeArrayFieldTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TYPE poly AS (name text, pts i32[])")
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, p poly)")
	run(t, db, "INSERT INTO t VALUES (1, ROW('a', '{10,20,30}'))")
	run(t, db, "INSERT INTO t VALUES (2, ROW('b', '{}'))")
	run(t, db, "INSERT INTO t VALUES (3, ROW('c', NULL))")
	return db
}

// nestedCompositeTableDB has nested composite types (a field whose type is another composite, by
// name) used by a column with a stored nested value (S3). point is created first (a referenced type
// must exist), but the on-disk order is name-sorted (line, point) — line sorts BEFORE the point it
// references, so the two-pass load (collect all, then resolve) is exercised; the row pins the
// recursive value codec descending through a composite field.
func nestedCompositeTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TYPE point AS (x i32 NOT NULL, y i32 NOT NULL)")
	run(t, db, "CREATE TYPE line AS (a point, b point)")
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, ln line)")
	run(t, db, "INSERT INTO t VALUES (1, ROW(ROW(1, 2), ROW(3, 4)))")
	return db
}

// sequenceTableDB pins the v12 sequence catalog entries (entry_kind 2 + name + six i64 fields +
// the flags byte) and the emission order — sequence entries before the table entry
// (spec/design/sequences.md §3). s1 is an ascending default sequence advanced three times
// (is_called true, last_value 3, the default MAXVALUE i64::MAX — a large positive i64); s2 is a
// fresh descending sequence (is_called false, negative increment/min/max/start — negative two's-
// complement i64 — plus CACHE 5 + CYCLE, exercising both flag bits and the cache field). A one-row
// table t follows, proving sequences and tables coexist in catalog order. Must match the Ruby
// reference's SEQUENCE_TABLE (spec/fileformat/verify.rb).
func sequenceTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE SEQUENCE s1")
	run(t, db, "SELECT nextval('s1')")
	run(t, db, "SELECT nextval('s1')")
	run(t, db, "SELECT nextval('s1')")
	run(t, db, "CREATE SEQUENCE s2 INCREMENT BY -2 MINVALUE -100 MAXVALUE -1 CACHE 5 CYCLE")
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)")
	run(t, db, "INSERT INTO t VALUES (1, 10)")
	return db
}

// serialTableDB pins the v13 OWNED-sequence link (the has_owner flag bit + the owner table-name/
// column-ordinal tail). The serial column id desugars to an i32 column that is NOT NULL (via the PK)
// with an expression DEFAULT nextval('t_id_seq'), and an OWNED sequence t_id_seq created alongside;
// one INSERT advances it once (is_called true, last_value 1). Must match the Ruby reference's
// SERIAL_TABLE (spec/fileformat/verify.rb), spec/design/sequences.md §12.
func serialTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id serial PRIMARY KEY, v text)")
	run(t, db, "INSERT INTO t (v) VALUES ('hello')")
	return db
}

// identityTableDB pins the v15 IDENTITY column flag bits (bit4 is_identity, bit5 identity_always)
// for both kinds, atop the same serial-shaped owned-sequence bytes. id is GENERATED ALWAYS
// (flags bit1+bit3+bit4+bit5), n is GENERATED BY DEFAULT (flags bit1+bit3+bit4); each gets an owned
// default-i64 sequence + an expression DEFAULT nextval('<seq>'). One INSERT advances both. Must
// match the Ruby reference's IDENTITY_TABLE (spec/fileformat/verify.rb), spec/design/sequences.md §13.
func identityTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	run(t, db, "CREATE TABLE t (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, "+
		"n int GENERATED BY DEFAULT AS IDENTITY, v text)")
	run(t, db, "INSERT INTO t (v) VALUES ('hi')")
	return db
}

// collationTableDB is a baked COLLATION (v17 — entry_kind 3 snapshot + per-column collations): the
// dev-root collation imported + set as the per-database default (is_default), a column with explicit
// COLLATE "dev-root" (flags bit6 + name), an un-annotated column inheriting the default (bit6 + name),
// and an explicit COLLATE "C" column (no collation). Must match the Ruby reference's COLLATION_TABLE.
func collationTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	if _, err := db.ImportCollation(devRoot(t)); err != nil {
		t.Fatalf("import dev-root: %v", err)
	}
	if err := db.SetDefaultCollation("dev-root"); err != nil {
		t.Fatalf("set default: %v", err)
	}
	run(t, db, `CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE "dev-root", `+
		`plain text, byteorder text COLLATE "C")`)
	run(t, db, `INSERT INTO t VALUES (1, 'a', 'b', 'z')`)
	run(t, db, `INSERT INTO t VALUES (2, 'z', 'a', 'a')`)
	return db
}

// collationPKTableDB: a collated text PRIMARY KEY + a collated secondary index (slice 1e,
// encoding.md §2.12) — both keys store the dev-root UCA sort key, so the B-tree iterates in
// collation order. The dev-root snapshot is baked (not the default). Must match the Ruby
// reference's COLLATION_PK_TABLE.
func collationPKTableDB(t *testing.T) *Database {
	db := WithPageSize(goldenPageSize)
	if _, err := db.ImportCollation(devRoot(t)); err != nil {
		t.Fatalf("import dev-root: %v", err)
	}
	run(t, db, `CREATE TABLE t (name text COLLATE "dev-root" PRIMARY KEY, tag text COLLATE "dev-root")`)
	run(t, db, `CREATE INDEX t_tag_idx ON t (tag)`)
	// Inserted out of collation order; stored in collation order ('a' < 'z' by the sort key).
	run(t, db, `INSERT INTO t VALUES ('z', 'a')`)
	run(t, db, `INSERT INTO t VALUES ('a', 'b')`)
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
		{"bool_pk_table.jed", boolPKTableDB},
		{"decimal_table.jed", decimalTableDB},
		{"bytea_table.jed", byteaTableDB},
		{"text_pk_table.jed", textPKTableDB},
		{"bytea_pk_table.jed", byteaPKTableDB},
		{"decimal_pk_table.jed", decimalPKTableDB},
		{"uuid_table.jed", uuidTableDB},
		{"default_table.jed", defaultTableDB},
		{"default_expr_table.jed", defaultExprTableDB},
		{"timestamp_table.jed", timestampTableDB},
		{"timestamptz_table.jed", timestamptzTableDB},
		{"interval_table.jed", intervalTableDB},
		{"interval_pk_table.jed", intervalPKTableDB},
		{"float64_table.jed", float64TableDB},
		{"float32_table.jed", float32TableDB},
		{"date_table.jed", dateTableDB},
		{"nopk_table.jed", nopkTableDB},
		{"composite_pk_table.jed", compositePKTableDB},
		{"check_table.jed", checkTableDB},
		{"index_table.jed", indexTableDB},
		{"unique_table.jed", uniqueTableDB},
		{"gin_array_table.jed", ginArrayTableDB},
		{"gin_uuid_table.jed", ginUuidTableDB},
		{"fk_table.jed", fkTableDB},
		{"composite_type_table.jed", compositeTypeTableDB},
		{"nested_composite_table.jed", nestedCompositeTableDB},
		{"array_table.jed", arrayTableDB},
		{"range_table.jed", rangeTableDB},
		{"range_pk_table.jed", rangePKTableDB},
		{"array_composite_table.jed", arrayCompositeTableDB},
		{"composite_array_field_table.jed", compositeArrayFieldTableDB},
		{"sequence_table.jed", sequenceTableDB},
		{"serial_table.jed", serialTableDB},
		{"identity_table.jed", identityTableDB},
		{"collation_table.jed", collationTableDB},
		{"collation_pk_table.jed", collationPKTableDB},
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
		{"bool_pk_table.jed", boolPKTableDB, "t"},
		{"decimal_table.jed", decimalTableDB, "t"},
		{"bytea_table.jed", byteaTableDB, "t"},
		{"text_pk_table.jed", textPKTableDB, "t"},
		{"bytea_pk_table.jed", byteaPKTableDB, "t"},
		{"decimal_pk_table.jed", decimalPKTableDB, "t"},
		{"uuid_table.jed", uuidTableDB, "t"},
		{"default_table.jed", defaultTableDB, "t"},
		{"default_expr_table.jed", defaultExprTableDB, "t"},
		{"timestamp_table.jed", timestampTableDB, "t"},
		{"timestamptz_table.jed", timestamptzTableDB, "t"},
		{"interval_table.jed", intervalTableDB, "t"},
		{"interval_pk_table.jed", intervalPKTableDB, "t"},
		{"float64_table.jed", float64TableDB, "t"},
		{"float32_table.jed", float32TableDB, "t"},
		{"date_table.jed", dateTableDB, "t"},
		{"nopk_table.jed", nopkTableDB, "r"},
		{"composite_pk_table.jed", compositePKTableDB, "t"},
		{"check_table.jed", checkTableDB, "t"},
		{"index_table.jed", indexTableDB, "t"},
		{"unique_table.jed", uniqueTableDB, "t"},
		{"gin_array_table.jed", ginArrayTableDB, "t"},
		{"gin_uuid_table.jed", ginUuidTableDB, "t"},
		{"fk_table.jed", fkTableDB, "c"},
		{"composite_type_table.jed", compositeTypeTableDB, "t"},
		{"nested_composite_table.jed", nestedCompositeTableDB, "t"},
		{"array_table.jed", arrayTableDB, "t"},
		{"range_table.jed", rangeTableDB, "t"},
		{"range_pk_table.jed", rangePKTableDB, "t"},
		{"array_composite_table.jed", arrayCompositeTableDB, "t"},
		{"composite_array_field_table.jed", compositeArrayFieldTableDB, "t"},
		{"sequence_table.jed", sequenceTableDB, "t"},
		{"serial_table.jed", serialTableDB, "t"},
		{"identity_table.jed", identityTableDB, "t"},
		{"collation_table.jed", collationTableDB, "t"},
		{"collation_pk_table.jed", collationPKTableDB, "t"},
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
	if id.Name != "id" || id.Type.ScalarTy() != Int32 || !id.PrimaryKey || !id.NotNull {
		t.Errorf("column id wrong: %+v", id)
	}
	if v.Name != "v" || v.Type.ScalarTy() != Int16 || v.PrimaryKey || v.NotNull {
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
	run(t, db, "CREATE TABLE t (id i32 PRIMARY KEY, v i16)")
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
