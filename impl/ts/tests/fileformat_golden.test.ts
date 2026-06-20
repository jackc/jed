// Golden-file cross-core test (CLAUDE.md §8). The load-bearing honesty test for the
// on-disk format: this core must (a) READ a checked-in golden into the expected catalog
// + rows, and (b) WRITE the same logical database to bytes equal to the golden EXACTLY.
// Because the format is deterministic, rust-bytes == go-bytes == golden == ts-bytes, so
// every core reads the others' output. Goldens are authored at page_size 256 by
// spec/fileformat/verify.rb (the independent Ruby reference).

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { Database, execute } from "../src/lib.ts";
import { compileCollation } from "../src/collation.ts";
import { crc32Ieee, loadDatabase, toImage } from "../src/format.ts";
import { scalarT } from "../src/types.ts";
import { specPath } from "./tomlmini.ts";
import { bytesEqual, fillerBytesHex, fillerText } from "./util.ts";

const GOLDEN_PAGE_SIZE = 256;

function fixture(name: string): Uint8Array {
  // Copy into a fresh, zero-offset Uint8Array (Node Buffers can be pool-backed slices).
  return new Uint8Array(readFileSync(specPath(`fileformat/fixtures/${name}`)));
}

function run(db: Database, sql: string): void {
  execute(db, sql);
}

// goldenDb is an in-memory handle serializing at the golden page size. The page-backed B-tree's
// fan-out tracks the page size (spec/fileformat/format.md), so the in-memory tree must be built at
// the size it will serialize to.
function goldenDb(): Database {
  const db = new Database();
  db.pageSize = GOLDEN_PAGE_SIZE;
  return db;
}

// pkTableDB: CREATE TABLE t (id i32 PRIMARY KEY, v i16) with 20 rows (id 3's v is
// NULL) — enough to span more than one data page at page_size 256.
function pkTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i16)");
  for (let i = 1; i <= 20; i++) {
    const v = i === 3 ? "NULL" : `${i * 10}`;
    run(db, `INSERT INTO t VALUES (${i}, ${v})`);
  }
  return db;
}

function oneTableEmptyDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i16)");
  return db;
}

// compositePKTableDB has a COMPOSITE primary key (constraints.md §3) — the stored key is
// the concatenation of the members' encodings (4-byte i32 then 2-byte i16,
// encoding.md §2.3). Rows insert in ascending tuple order (the tree shape is
// order-sensitive), with a negative first component and first-component ties broken by
// the second.
function compositePKTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (a i32, b i16, v i16, PRIMARY KEY (a, b))");
  for (const [a, b, v] of [
    [-2, 5, 10], [1, 1, 20], [1, 2, 30], [1, 3, 40],
    [2, 0, 50], [2, 1, 60], [3, 7, 70], [3, 9, 80],
  ]) {
    run(db, `INSERT INTO t VALUES (${a}, ${b}, ${v})`);
  }
  return db;
}

// checkTableDB has CHECK constraints (constraints.md §4) — exercises the v4 catalog check
// list: an auto-named single-column check, an explicitly-named multi-column check, and a
// check whose persisted text exercises the token rendering (string literal with a doubled
// quote, decimal literals, >=/<=), stored in name order
// (price_range < t_b_check < t_note_check).
function checkTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (a int PRIMARY KEY, b int CHECK (b > 0), price numeric(8,2), " +
    "CONSTRAINT price_range CHECK (price >= 0.50 AND price <= 9999.99), note text, " +
    "CHECK (note = 'ok' OR note = 'a''b'))");
  run(db, "INSERT INTO t VALUES (1, 5, 1.00, 'ok'), (2, NULL, 9999.99, 'a''b'), " +
    "(3, 100, 0.50, 'ok')");
  return db;
}

// indexTableDB has SECONDARY INDEXES (v5 — spec/design/indexes.md): the catalog reshape +
// the index trees. The PK list order (b, a) differs from declaration order (the lifted
// composite-PK narrowing); i_u covers a nullable uuid column holding a NULL (the
// encoding.md §2.2 presence tag in stored index order — NULL last), and the unnamed index
// auto-names to t_a_b_idx. Index records have empty payloads (key only).
function indexTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (a i32, b i32, u uuid, PRIMARY KEY (b, a))");
  run(db, "CREATE INDEX i_u ON t (u)");
  run(db, "CREATE INDEX ON t (a, b)");
  run(db, "INSERT INTO t VALUES (1, 10, '550e8400-e29b-41d4-a716-446655440000'), " +
    "(2, 10, NULL), (3, 20, '00000000-0000-0000-0000-000000000000')");
  return db;
}

// uniqueTableDB has UNIQUE indexes (v6 — the per-index flags byte, indexes.md §8):
// t_v_key (a UNIQUE constraint's auto-name) over a nullable column holding two NULLs
// (NULLS DISTINCT — both stored), the named two-column constraint wv, a CREATE UNIQUE
// INDEX uq, and the plain index nu (flags 0 beside flags 1).
function uniqueTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32, w i32, " +
    "UNIQUE (v), CONSTRAINT wv UNIQUE (w, v))");
  run(db, "CREATE INDEX nu ON t (v)");
  run(db, "CREATE UNIQUE INDEX uq ON t (w)");
  run(db, "INSERT INTO t VALUES (1, 10, 100), (2, NULL, 200), (3, NULL, 300)");
  return db;
}

// fkTableDB has FOREIGN KEY constraints (v11 — spec/design/constraints.md §6): pins the catalog
// foreign-key list. Parent `p` (a PK + two UNIQUE constraints, the FK targets); child `c` with
// four FKs covering every shape — a named FK to the UNIQUE column (c_code_fk), a self-reference to
// the PK (c_mgr_fkey), an auto-named FK to the PK (c_pid_fkey), and an auto-named COMPOSITE FK to
// the two-column UNIQUE with ON DELETE RESTRICT (c_x_y_fkey, the lone non-zero actions byte). Must
// match the Ruby reference's FK_TABLE (spec/fileformat/verify.rb).
function fkTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE p (pid i32 PRIMARY KEY, code i32 UNIQUE, a i32, b i32, UNIQUE (a, b))");
  run(db, "INSERT INTO p VALUES (1, 100, 10, 20), (2, 200, 30, 40)");
  run(db, "CREATE TABLE c (id i32 PRIMARY KEY, pid i32, pcode i32, x i32, y i32, mgr i32, " +
    "FOREIGN KEY (pid) REFERENCES p (pid), " +
    "CONSTRAINT c_code_fk FOREIGN KEY (pcode) REFERENCES p (code), " +
    "FOREIGN KEY (x, y) REFERENCES p (a, b) ON DELETE RESTRICT, " +
    "FOREIGN KEY (mgr) REFERENCES c (id))");
  run(db, "INSERT INTO c VALUES (10, 1, 100, 10, 20, NULL), (11, 2, 200, 30, 40, 10)");
  return db;
}

// arrayTableDB has ARRAY (T[]) columns (v10 — spec/design/array.md): pins the catalog array-column
// entry (type_code 15 + the element-type descriptor, §3) and the compact value body (§4). An
// i32[] (fixed-width elements: no per-element length prefix) and a text[]; row 2 has an EMPTY
// array (ndim=0), row 3 a NULL element (the HAS_NULLS bitmap) and a whole-value NULL array (the
// lone 0x01 tag). Must match the Ruby reference's ARRAY_TABLE (spec/fileformat/verify.rb).
function arrayTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, xs i32[], tags text[])");
  run(db, "INSERT INTO t VALUES (1, ARRAY[10, 20, 30], ARRAY['a', 'b'])");
  run(db, "INSERT INTO t VALUES (2, '{40,50}', '{}')");
  run(db, "INSERT INTO t VALUES (3, ARRAY[1, NULL, 3], NULL)");
  // Row 4 pins the §12 shapes: a 2-D i32[] and a custom-lower-bound text[] (the lb i32 field).
  run(db, "INSERT INTO t VALUES (4, ARRAY[ARRAY[10,20],ARRAY[30,40]], '[2:3]={x,y}')");
  return db;
}

// rangeTableDB has RANGE columns (v15 — spec/design/ranges.md): pins the catalog range-column entry
// (type_code 17 + the one-byte element descriptor, §3) and the compact value body (the flags byte +
// present bound bodies, §4). Two discrete range columns — an i32range and an i64range — over rows
// exercising every flags bit and the canonical-`[)` storage: a finite `[)`, an inclusive-upper
// literal that canonicalizes (`[1,5]` → `[1,6)`), the EMPTY range, infinite bounds (lower-only,
// both), a NULL range, an exclusive-lower literal with infinite upper (`(5,)` → `[6,)`), and a
// singleton (`[1,1]` → `[1,2)`). Must match the Ruby reference's RANGE_TABLE (spec/fileformat/verify.rb).
function rangeTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, r i32range, br i64range)");
  run(db, "INSERT INTO t VALUES (1, '[1,5)', '[10,20)')");
  run(db, "INSERT INTO t VALUES (2, '[1,5]', NULL)");
  run(db, "INSERT INTO t VALUES (3, 'empty', '(,100)')");
  run(db, "INSERT INTO t VALUES (4, '(,)', '(5,)')");
  run(db, "INSERT INTO t VALUES (5, NULL, '[1,1]')");
  return db;
}

// rangePkTableDB: an i32range PRIMARY KEY — the first CONTAINER key (encoding.md §2.11). The
// range-bounds key (empty/±∞/inclusivity framing around the i32 element key) lands in the key slot.
// Rows are inserted in ASCENDING range_total_cmp order to match verify.rb's ascending-key tree builder.
function rangePkTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (k i32range PRIMARY KEY, v i32)");
  run(db, "INSERT INTO t VALUES ('empty', 0)");
  run(db, "INSERT INTO t VALUES ('(,5)', 1)");
  run(db, "INSERT INTO t VALUES ('(,)', 2)");
  run(db, "INSERT INTO t VALUES ('[1,5)', 3)");
  run(db, "INSERT INTO t VALUES ('[2,4)', 4)");
  run(db, "INSERT INTO t VALUES ('[2,)', 5)");
  return db;
}

// ginArrayTableDB has a GIN inverted index (v13 — the per-index index_kind byte, spec/design/gin.md):
// i_nums_gin over an i32[] column (kind 1) beside an ordinary ordered index i_n over a scalar
// column (kind 0 — a btree index cannot sit on the array column). Rows exercise term dedup (row 2's
// duplicate 20), an empty and a NULL whole-value array (rows 3/4 → no entries), and a NULL element
// (row 5). Rows are inserted before the indexes so each builds via the sorted-bulk path, matching
// the Ruby reference's GIN_ARRAY_TABLE.
function ginArrayTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, nums i32[], n i32)");
  run(
    db,
    "INSERT INTO t VALUES (1, '{10,20,30}', 1), (2, '{20,20,40}', 2), (3, '{}', 3), (4, NULL, 4), (5, '{10,NULL,50}', 5)",
  );
  run(db, "CREATE INDEX i_n ON t (n)");
  run(db, "CREATE INDEX i_nums_gin ON t USING gin (nums)");
  return db;
}

// ginUuidTableDB has a GIN index over a uuid[] column (kind 1) — the non-integer GIN element-type
// golden (spec/design/gin.md §3/§4): each GIN term is the element's 16-byte uuid-raw16 key encoding,
// so entries are encode_uuid(term) ‖ storage_key (empty payload). Rows mirror ginArrayTableDB: term
// dedup (row 2's duplicate bb), an empty and a NULL whole-value array (rows 3/4 → no entries), and a
// NULL element (row 5). An ordinary ordered index i_n sits beside it (kind 0).
function ginUuidTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, tags uuid[], n i32)");
  run(
    db,
    "INSERT INTO t VALUES " +
      "(1, '{00000000-0000-0000-0000-0000000000aa,00000000-0000-0000-0000-0000000000bb,00000000-0000-0000-0000-0000000000cc}', 1), " +
      "(2, '{00000000-0000-0000-0000-0000000000bb,00000000-0000-0000-0000-0000000000bb,00000000-0000-0000-0000-0000000000dd}', 2), " +
      "(3, '{}', 3), " +
      "(4, NULL, 4), " +
      "(5, '{00000000-0000-0000-0000-0000000000aa,NULL,00000000-0000-0000-0000-0000000000ee}', 5)",
  );
  run(db, "CREATE INDEX i_n ON t (n)");
  run(db, "CREATE INDEX i_tags_gin ON t USING gin (tags)");
  return db;
}

// nopkTableDB has no primary key — exercises the stored synthetic i64 rowid key.
function nopkTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE r (a i16, b i64)");
  for (const [a, b] of [[7, 70], [8, 80], [9, 90]]) {
    run(db, `INSERT INTO r VALUES (${a}, ${b})`);
  }
  return db;
}

// tallTreeDB's wide text padding forces a HEIGHT-2 tree (an interior node whose children are
// themselves interior nodes) at page_size 256 — exercises interior-of-interior child pointers and
// post-order page allocation across a deeper tree (spec/fileformat/format.md).
function tallTreeDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, pad text)");
  for (let i = 1; i <= 18; i++) {
    const pad = `row-${String(i).padStart(2, "0")}-${"x".repeat(48)}`;
    run(db, `INSERT INTO t VALUES (${i}, '${pad}')`);
  }
  return db;
}

// textTableDB has a text column — exercises the value codec's text branch (u16 length +
// UTF-8 bytes): the empty string, an embedded quote, a 2-byte char (é), a NULL text value,
// and a 4-byte astral char (😀). The PK stays i32 (no text key this slice).
function textTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, s text)");
  run(db, "INSERT INTO t VALUES (1, 'alice')");
  run(db, "INSERT INTO t VALUES (2, '')");
  run(db, "INSERT INTO t VALUES (3, 'O''Brien')");
  run(db, "INSERT INTO t VALUES (4, 'café')");
  run(db, "INSERT INTO t VALUES (5, NULL)");
  run(db, "INSERT INTO t VALUES (6, '😀')");
  return db;
}

// boolTableDB has a boolean column — exercises the value codec's boolean branch (a single
// bool-byte, 0x00 false / 0x01 true) plus a NULL boolean. The PK stays i32 (the boolean
// PRIMARY KEY case is boolPkTableDB).
function boolTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, flag boolean)");
  run(db, "INSERT INTO t VALUES (1, TRUE)");
  run(db, "INSERT INTO t VALUES (2, FALSE)");
  run(db, "INSERT INTO t VALUES (3, NULL)");
  return db;
}

// boolPkTableDB has a boolean PRIMARY KEY (the second golden with a NON-integer stored key,
// after uuid) — the bool-byte key encoding (bare 1 byte 0x00 false / 0x01 true, no presence
// tag since a PK is NOT NULL, spec/design/encoding.md §2.9), plus a nullable boolean value
// column. Rows go in via INSERT and the store sorts them into key (byte) order: false (0x00)
// then true (0x01). Must match spec/fileformat/verify.rb's BOOL_PK_TABLE.
function boolPkTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (k boolean PRIMARY KEY, v boolean)");
  run(db, "INSERT INTO t VALUES (FALSE, TRUE)");
  run(db, "INSERT INTO t VALUES (TRUE, NULL)");
  return db;
}

// textPkTableDB is the first golden with a VARIABLE-WIDTH non-integer stored key — the
// text-terminated-escape encoding (encoding.md §2.4). The store sorts rows into key (code-point /
// byte) order: "" < "Zeta"(0x5A) < "apple"(0x61) < "banana"(0x62) < "é"(0xC3). Must match
// spec/fileformat/verify.rb's TEXT_PK_TABLE.
function textPkTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (k text PRIMARY KEY, v i32)");
  run(db, "INSERT INTO t VALUES ('', 4)");
  run(db, "INSERT INTO t VALUES ('Zeta', NULL)");
  run(db, "INSERT INTO t VALUES ('apple', 2)");
  run(db, "INSERT INTO t VALUES ('banana', 3)");
  run(db, "INSERT INTO t VALUES ('é', 5)");
  return db;
}

// byteaPkTableDB is the bytea-terminated-escape key encoding (encoding.md §2.6) — like text but
// over raw bytes, so the embedded-0x00 escape is exercised. The store sorts into unsigned-byte
// (key) order: '' < \x00 < \x61 < \x6100ff62 < \x6161 < \x62. Must match BYTEA_PK_TABLE.
function byteaPkTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (k bytea PRIMARY KEY, v i32)");
  run(db, "INSERT INTO t VALUES ('\\x', 5)");
  run(db, "INSERT INTO t VALUES ('\\x00', 6)");
  run(db, "INSERT INTO t VALUES ('\\x61', 1)");
  run(db, "INSERT INTO t VALUES ('\\x6100ff62', 4)");
  run(db, "INSERT INTO t VALUES ('\\x6161', 2)");
  run(db, "INSERT INTO t VALUES ('\\x62', 3)");
  return db;
}

// decimalPkTableDB is the first golden with a VARIABLE-WIDTH SIGNED stored key — the
// decimal-order-preserving encoding (encoding.md §2.5). The store sorts into numeric (= key)
// order: -2.5 < -0.5 < 0 < 0.25 < 1.5 < 10 < 100.50; "100.50" stores scale 2 in its value body
// but normalizes in the key. Must match spec/fileformat/verify.rb's DECIMAL_PK_TABLE.
function decimalPkTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (k decimal PRIMARY KEY, v i32)");
  run(db, "INSERT INTO t VALUES (-2.5, 6)");
  run(db, "INSERT INTO t VALUES (-0.5, 5)");
  run(db, "INSERT INTO t VALUES (0, 4)");
  run(db, "INSERT INTO t VALUES (0.25, 1)");
  run(db, "INSERT INTO t VALUES (1.5, 2)");
  run(db, "INSERT INTO t VALUES (10, 3)");
  run(db, "INSERT INTO t VALUES (100.50, 7)");
  return db;
}

// decimalTableDB has a decimal column — exercises the value codec's decimal branch (flags +
// u16 scale + u16 ndigits + base-10^4 groups) and the catalog typmod: an unconstrained numeric
// column `d` and a constrained numeric(10,2) column `m` (values already at scale 2, a no-op
// coercion). Covers positive, negative, zero, a multi-group coefficient, and a NULL.
function decimalTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, d numeric, m numeric(10,2))");
  run(db, "INSERT INTO t VALUES (1, 1.50, 1.50), (2, -12345.6789, -12.34), " +
    "(3, 0.00, 0.00), (4, 100000000.000001, 100.00), (5, NULL, NULL)");
  return db;
}

// byteaTableDB exercises the value codec's bytea branch (u16 length + raw bytes): a multi-
// byte value (a-f hex), the empty byte string, embedded 0x00 bytes, a high byte (0xFF), a
// NULL, and a lone 0x00. The PK stays i32 (no bytea key this slice). Literals are the `\x`
// hex input form, adapting to the bytea column (spec/design/types.md §6).
function byteaTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, b bytea)");
  run(db, "INSERT INTO t VALUES (1, '\\xdeadbeef')");
  run(db, "INSERT INTO t VALUES (2, '\\x')");
  run(db, "INSERT INTO t VALUES (3, '\\x000102')");
  run(db, "INSERT INTO t VALUES (4, '\\xff')");
  run(db, "INSERT INTO t VALUES (5, NULL)");
  run(db, "INSERT INTO t VALUES (6, '\\x00')");
  return db;
}

// overflowTableDB has large INCOMPRESSIBLE text + bytea values that spill OUT-OF-LINE PLAIN to
// overflow pages (spec/design/large-values.md §12): at page_size 256 a ~600/300-byte value
// exceeds RECORD_MAX (116); compression is attempted first (Slice B) but rejected by
// store-smaller, so the record holds a 0x02 pointer and the raw bytes live in a page_type-4
// chain. Row 1 spills both columns (multi-page chains), row 2 stays inline, row 3 is NULL/NULL.
// Must match the Ruby reference's OVERFLOW_TABLE (spec/fileformat/verify.rb).
function overflowTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, body text, blob bytea)");
  run(db, `INSERT INTO t VALUES (1, '${fillerText(600)}', '\\x${fillerBytesHex(300)}')`);
  run(db, "INSERT INTO t VALUES (2, 'small', '\\xcafe')");
  run(db, "INSERT INTO t VALUES (3, NULL, NULL)");
  return db;
}

// compressedTableDB has large COMPRESSIBLE values exercising Slice B's forms (large-values.md
// §13, format.md "Large values", lz4.md): row 1's "x"-run text and 0xAB-run bytea both become
// 0x03 inline-compressed; row 2's half-filler/half-run text compresses to ~200 B — smaller than
// plain but still over RECORD_MAX → 0x04 external-compressed (a chain carrying the COMPRESSED
// block); row 3 stays inline-plain; row 4 is NULL/NULL. Must match the Ruby reference's
// COMPRESSED_TABLE (spec/fileformat/verify.rb).
function compressedTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, body text, blob bytea)");
  run(db, `INSERT INTO t VALUES (1, '${"x".repeat(600)}', '\\x${"ab".repeat(200)}')`);
  run(db, `INSERT INTO t VALUES (2, '${fillerText(200)}${"y".repeat(200)}', NULL)`);
  run(db, "INSERT INTO t VALUES (3, 'tiny', '\\xcafe')");
  run(db, "INSERT INTO t VALUES (4, NULL, NULL)");
  return db;
}

// uuidTableDB has a uuid PRIMARY KEY (the first golden with a NON-integer stored key — the
// load-bearing §8 cross-core key-path proof) plus a nullable uuid column. Exercises the value
// codec's fixed-16-byte uuid branch (no length prefix), the uuid key encoding (bare 16 bytes),
// a present and a NULL uuid value, and the nil/max boundary UUIDs. Must match the Ruby
// reference's UUID_TABLE (spec/fileformat/verify.rb).
function uuidTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id uuid PRIMARY KEY, ref uuid)");
  run(
    db,
    "INSERT INTO t VALUES " +
      "('00000000-0000-0000-0000-000000000000', '550e8400-e29b-41d4-a716-446655440000'), " +
      "('550e8400-e29b-41d4-a716-446655440000', NULL), " +
      "('f47ac10b-58cc-4372-a567-0e02b2c3d479', '00000000-0000-0000-0000-000000000000'), " +
      "('ffffffff-ffff-ffff-ffff-ffffffffffff', 'ffffffff-ffff-ffff-ffff-ffffffffffff')",
  );
  return db;
}

// defaultTableDB exercises the DEFAULT column constraint on disk — the catalog flags bit2 + the
// pre-evaluated default value (written after the typmod). Covers an int default, a text default,
// a DEFAULT NULL, a NOT NULL column with a default, a decimal default coerced to numeric(6,2),
// and a plain no-default column. Row 1 takes every default; row 2 provides all values.
function defaultTableDB(): Database {
  const db = goldenDb();
  run(
    db,
    "CREATE TABLE t (id i32 PRIMARY KEY, n i32 DEFAULT 0, note text DEFAULT 'none', " +
      "maybe i32 DEFAULT NULL, req i32 NOT NULL DEFAULT 7, amt numeric(6,2) DEFAULT 1.5, plain i16)",
  );
  run(db, "INSERT INTO t (id) VALUES (1)");
  run(db, "INSERT INTO t VALUES (2, 42, 'hi', 5, 9, 2.00, 100)");
  return db;
}

// defaultExprTableDB exercises EXPRESSION column defaults on disk (v8) — the catalog flags bit3
// (default_is_expr) + the expr-text written after the typmod: a `uuid DEFAULT uuidv7()`, an
// `i32 DEFAULT 1 + 1`, a CONSTANT default beside them (bit2), and a plain no-default column.
// EMPTY table — the catalog encoding is the cross-core proof; the per-row evaluation is covered
// by the conformance corpus.
function defaultExprTableDB(): Database {
  const db = goldenDb();
  run(
    db,
    "CREATE TABLE t (id i32 PRIMARY KEY, g uuid DEFAULT uuidv7(), n i32 DEFAULT 1 + 1, " +
      "k i32 DEFAULT 7, plain i16)",
  );
  return db;
}

// timestampTableDB exercises the value codec's i64-instant branch (type code 8): a positive
// instant, a pre-1970 negative one, a BC-era one, the ±infinity sentinels, and a NULL. The
// literals parse to the same micros the golden stores. The PK stays i32.
function timestampTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, ts timestamp)");
  run(db, "INSERT INTO t VALUES (1, '2024-01-01 12:00:00')");
  run(db, "INSERT INTO t VALUES (2, '1969-12-31 23:59:59.5')");
  run(db, "INSERT INTO t VALUES (3, '0001-01-01 00:00:00 BC')");
  run(db, "INSERT INTO t VALUES (4, '-infinity')");
  run(db, "INSERT INTO t VALUES (5, 'infinity')");
  run(db, "INSERT INTO t VALUES (6, NULL)");
  return db;
}

// timestamptzTableDB exercises the same 8-byte branch under type code 9; the +05 literal
// normalizes to UTC before storage.
function timestamptzTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, ts timestamptz)");
  run(db, "INSERT INTO t VALUES (1, '2024-01-01 12:00:00+00')");
  run(db, "INSERT INTO t VALUES (2, '2024-01-01 12:00:00+05')");
  run(db, "INSERT INTO t VALUES (3, '1969-12-31 23:59:59.5+00')");
  run(db, "INSERT INTO t VALUES (4, '-infinity')");
  run(db, "INSERT INTO t VALUES (5, 'infinity')");
  run(db, "INSERT INTO t VALUES (6, NULL)");
  return db;
}

// intervalTableDB exercises the value codec's fixed 16-byte interval branch (type code 11): a
// positive multi-field value, a negative value, the zero interval, a months-only '1 mon' vs a
// span-equal-but-byte-distinct '30 days', and a NULL. The bare-string literals adapt.
function intervalTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, d interval)");
  run(db, "INSERT INTO t VALUES (1, '1 mon 2 days 03:04:05')");
  run(db, "INSERT INTO t VALUES (2, '-1 day')");
  run(db, "INSERT INTO t VALUES (3, '0 seconds')");
  run(db, "INSERT INTO t VALUES (4, '1 mon')");
  run(db, "INSERT INTO t VALUES (5, '30 days')");
  run(db, "INSERT INTO t VALUES (6, NULL)");
  return db;
}

// intervalPkTableDB is a golden with a fixed-width SIGNED 16-byte stored key — the
// interval-span-i128 encoding (encoding.md §2.10). Rows store in canonical-span (= key) order:
// -1 mon < -1 day < 0 < 1 sec < 1 day < 1 mon < 100 years; all spans distinct (span-equal intervals
// collide on the span key). Inserted in ascending key order to match verify.rb's build_tree (the
// split shape is order-sensitive); the out-of-order proof is in the conformance test.
function intervalPkTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (k interval PRIMARY KEY, v i32)");
  run(db, "INSERT INTO t VALUES ('-1 mon', 6)");
  run(db, "INSERT INTO t VALUES ('-1 day', 5)");
  run(db, "INSERT INTO t VALUES ('0 seconds', 4)");
  run(db, "INSERT INTO t VALUES ('1 sec', 1)");
  run(db, "INSERT INTO t VALUES ('1 day', 2)");
  run(db, "INSERT INTO t VALUES ('1 mon', 3)");
  run(db, "INSERT INTO t VALUES ('100 years', 7)");
  return db;
}

// float64TableDB exercises the value codec's 8-byte IEEE branch (type code 12): a positive
// fraction, a negative value, +0 and -0 (the sign bit is preserved on disk — distinct bytes), both
// infinities, a canonicalized NaN (stored as the single quiet pattern 0x7FF8…000), a NULL, and
// Float64 max (a full mantissa). Finite values enter via bare numeric literals (decimal adaptation);
// the specials enter via typed literals in INSERT ... SELECT (a VALUES slot takes only bare literals
// this slice — float.md). PK is i32 (no float key this slice — float PK → 0A000).
function float64TableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, d f64)");
  run(db, "INSERT INTO t VALUES (1, 1.5)");
  run(db, "INSERT INTO t VALUES (2, -2.5)");
  run(db, "INSERT INTO t VALUES (3, 0.0)");
  run(db, "INSERT INTO t SELECT 4, f64 '-0'");
  run(db, "INSERT INTO t SELECT 5, f64 'Infinity'");
  run(db, "INSERT INTO t SELECT 6, f64 '-Infinity'");
  run(db, "INSERT INTO t SELECT 7, f64 'NaN'");
  run(db, "INSERT INTO t VALUES (8, NULL)");
  run(db, "INSERT INTO t SELECT 9, f64 '1.7976931348623157e308'");
  return db;
}

// float32TableDB exercises the value codec's 4-byte IEEE branch (type code 13): the same
// special-value coverage as float64TableDB (canonicalized NaN → 0x7FC00000) plus 100.25 (exactly
// representable in binary32). PK is i32 (no float key this slice).
function float32TableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, r f32)");
  run(db, "INSERT INTO t VALUES (1, 1.5)");
  run(db, "INSERT INTO t VALUES (2, -2.5)");
  run(db, "INSERT INTO t VALUES (3, 0.0)");
  run(db, "INSERT INTO t SELECT 4, f32 '-0'");
  run(db, "INSERT INTO t SELECT 5, f32 'Infinity'");
  run(db, "INSERT INTO t SELECT 6, f32 '-Infinity'");
  run(db, "INSERT INTO t SELECT 7, f32 'NaN'");
  run(db, "INSERT INTO t VALUES (8, NULL)");
  run(db, "INSERT INTO t VALUES (9, 100.25)");
  return db;
}

// dateTableDB exercises the value codec's date branch (type code 16): the 4-byte i32 day-count
// body (same int-be-signflip codec as i32). A positive date, a pre-1970 negative one, a BC-era
// one, the −infinity/+infinity sentinels (i32 min/max), and a NULL. The bare-string literals adapt
// to the date column. PK is i32 (spec/design/date.md).
function dateTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, d date)");
  run(db, "INSERT INTO t VALUES (1, '2024-01-15')");
  run(db, "INSERT INTO t VALUES (2, '1969-12-31')");
  run(db, "INSERT INTO t VALUES (3, '0044-03-15 BC')");
  run(db, "INSERT INTO t VALUES (4, '-infinity')");
  run(db, "INSERT INTO t VALUES (5, 'infinity')");
  run(db, "INSERT INTO t VALUES (6, NULL)");
  return db;
}

// compositeTypeTableDB: a composite TYPE defined + persisted (v9), used by a stored composite COLUMN
// (S3 — the recursive value codec). Exercises the kind-tagged catalog (a composite-type entry, kind
// 1, before the table entry, kind 0), a composite column (type_code 14), and the value codec's null
// bitmap + present-field bodies (row 2's NULL zip field).
function compositeTypeTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TYPE addr AS (street text NOT NULL, zip i32)");
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, home addr)");
  run(db, "INSERT INTO t VALUES (1, ROW('Main', 90210))");
  run(db, "INSERT INTO t VALUES (2, ROW('Oak', NULL))");
  return db;
}

// arrayCompositeTableDB: a composite type used as an array ELEMENT type (array-of-composite, array.md
// §12 AC1). The catalog array-column entry carries a composite element descriptor (element_type_code
// 14 + "addr") and the value body recurses (an array body whose elements are composite bodies). Row
// 2's element has a NULL `zip` field (the composite null-bitmap inside an element); row 3 mixes a
// present composite element with a NULL element (the array HAS_NULLS bitmap).
function arrayCompositeTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TYPE addr AS (street text NOT NULL, zip i32)");
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, items addr[])");
  run(db, `INSERT INTO t VALUES (1, '{"(Main,90210)","(Side,5)"}')`);
  run(db, `INSERT INTO t VALUES (2, '{"(Oak,)"}')`);
  run(db, `INSERT INTO t VALUES (3, '{"(A,1)",NULL}')`);
  run(db, "INSERT INTO t VALUES (4, '{}')");
  run(db, "INSERT INTO t VALUES (5, NULL)");
  return db;
}

// compositeArrayFieldTableDB: a composite type with an array-typed FIELD (array.md §12 — the mirror
// of array-of-composite). The catalog composite-type entry carries a code-15 array field
// (element_type_code 2 = i32) and the value body recurses (a composite body whose `pts` field is an
// array body). Row 2 has an empty array field {} (ndim 0); row 3 a NULL array field (the composite
// null-bitmap).
function compositeArrayFieldTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TYPE poly AS (name text, pts i32[])");
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, p poly)");
  run(db, "INSERT INTO t VALUES (1, ROW('a', '{10,20,30}'))");
  run(db, "INSERT INTO t VALUES (2, ROW('b', '{}'))");
  run(db, "INSERT INTO t VALUES (3, ROW('c', NULL))");
  return db;
}

// nestedCompositeTableDB: nested composite types (a field whose type is another composite, by name)
// used by a column with a stored nested value (S3). `point` is created first (a referenced type must
// exist), but the on-disk order is name-sorted (`line`, `point`) — `line` sorts BEFORE the `point` it
// references, so the two-pass load is exercised; the row pins the recursive value codec descending
// through a composite field.
function nestedCompositeTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TYPE point AS (x i32 NOT NULL, y i32 NOT NULL)");
  run(db, "CREATE TYPE line AS (a point, b point)");
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, ln line)");
  run(db, "INSERT INTO t VALUES (1, ROW(ROW(1, 2), ROW(3, 4)))");
  return db;
}

// sequenceTableDB: two sequences (v12) — `s1` ascending, advanced 3 times (is_called, last_value 3),
// `s2` descending/fresh with a non-default cache + cycle — plus a one-row table, pinning the sequence
// catalog entry (entry_kind 2) and the catalog emission order (sequences before tables).
function sequenceTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE SEQUENCE s1");
  run(db, "SELECT nextval('s1')");
  run(db, "SELECT nextval('s1')");
  run(db, "SELECT nextval('s1')");
  run(db, "CREATE SEQUENCE s2 INCREMENT BY -2 MINVALUE -100 MAXVALUE -1 CACHE 5 CYCLE");
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  run(db, "INSERT INTO t VALUES (1, 10)");
  return db;
}

// serialTableDB (v13): the OWNED-sequence link (the has_owner flag bit + the owner table-name/
// column-ordinal tail). The serial column id desugars to an i32 column that is NOT NULL (via the PK)
// with an expression DEFAULT nextval('t_id_seq'), and an OWNED sequence t_id_seq created alongside;
// one INSERT advances it once. Must match the Ruby reference's SERIAL_TABLE (spec/design/sequences.md §12).
function serialTableDB(): Database {
  const db = goldenDb();
  run(db, "CREATE TABLE t (id serial PRIMARY KEY, v text)");
  run(db, "INSERT INTO t (v) VALUES ('hello')");
  return db;
}

// identityTableDB (v15): the two IDENTITY column flag bits (bit4 is_identity, bit5 identity_always)
// for both kinds, atop the same serial-shaped owned-sequence bytes. id is GENERATED ALWAYS (flags
// bit1+bit3+bit4+bit5), n is GENERATED BY DEFAULT (flags bit1+bit3+bit4); each gets an owned
// default-i64 sequence + an expression DEFAULT nextval('<seq>'). One INSERT advances both. Must
// match the Ruby reference's IDENTITY_TABLE (spec/fileformat/verify.rb), spec/design/sequences.md §13.
function identityTableDB(): Database {
  const db = goldenDb();
  run(
    db,
    "CREATE TABLE t (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, " +
      "n int GENERATED BY DEFAULT AS IDENTITY, v text)",
  );
  run(db, "INSERT INTO t (v) VALUES ('hi')");
  return db;
}

// collationTableDB is a baked COLLATION (v17 — entry_kind 3 snapshot + per-column collations): the
// dev-root collation imported + set as the per-database default (is_default), a column with explicit
// COLLATE "dev-root" (flags bit6 + name), an un-annotated column inheriting the default (bit6 + name),
// and an explicit COLLATE "C" column (no collation). Must match the Ruby reference's COLLATION_TABLE.
function collationTableDB(): Database {
  const db = goldenDb();
  db.importCollation(
    compileCollation(
      "dev-root",
      readFileSync(specPath("collation/fixtures/dev-root.allkeys"), "utf8"),
    ),
  );
  db.setDefaultCollation("dev-root");
  run(
    db,
    `CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE "dev-root", ` +
      `plain text, byteorder text COLLATE "C")`,
  );
  run(db, `INSERT INTO t VALUES (1, 'a', 'b', 'z')`);
  run(db, `INSERT INTO t VALUES (2, 'z', 'a', 'a')`);
  return db;
}

// WRITE side: serializing the in-memory database reproduces the golden byte-exactly.
test("write matches goldens (byte-identical to Rust/Go/Ruby)", () => {
  const cases: { name: string; build: () => Database }[] = [
    { name: "empty_db.jed", build: () => goldenDb() },
    { name: "overflow_table.jed", build: overflowTableDB },
    { name: "compressed_table.jed", build: compressedTableDB },
    { name: "one_table_empty.jed", build: oneTableEmptyDB },
    { name: "pk_table.jed", build: pkTableDB },
    { name: "text_table.jed", build: textTableDB },
    { name: "bool_table.jed", build: boolTableDB },
    { name: "bool_pk_table.jed", build: boolPkTableDB },
    { name: "decimal_table.jed", build: decimalTableDB },
    { name: "bytea_table.jed", build: byteaTableDB },
    { name: "text_pk_table.jed", build: textPkTableDB },
    { name: "bytea_pk_table.jed", build: byteaPkTableDB },
    { name: "decimal_pk_table.jed", build: decimalPkTableDB },
    { name: "uuid_table.jed", build: uuidTableDB },
    { name: "default_table.jed", build: defaultTableDB },
    { name: "default_expr_table.jed", build: defaultExprTableDB },
    { name: "timestamp_table.jed", build: timestampTableDB },
    { name: "date_table.jed", build: dateTableDB },
    { name: "timestamptz_table.jed", build: timestamptzTableDB },
    { name: "interval_table.jed", build: intervalTableDB },
    { name: "interval_pk_table.jed", build: intervalPkTableDB },
    { name: "float64_table.jed", build: float64TableDB },
    { name: "float32_table.jed", build: float32TableDB },
    { name: "nopk_table.jed", build: nopkTableDB },
    { name: "composite_pk_table.jed", build: compositePKTableDB },
    { name: "check_table.jed", build: checkTableDB },
    { name: "index_table.jed", build: indexTableDB },
    { name: "unique_table.jed", build: uniqueTableDB },
    { name: "gin_array_table.jed", build: ginArrayTableDB },
    { name: "gin_uuid_table.jed", build: ginUuidTableDB },
    { name: "fk_table.jed", build: fkTableDB },
    { name: "composite_type_table.jed", build: compositeTypeTableDB },
    { name: "nested_composite_table.jed", build: nestedCompositeTableDB },
    { name: "sequence_table.jed", build: sequenceTableDB },
    { name: "serial_table.jed", build: serialTableDB },
    { name: "identity_table.jed", build: identityTableDB },
    { name: "collation_table.jed", build: collationTableDB },
    { name: "array_table.jed", build: arrayTableDB },
    { name: "range_table.jed", build: rangeTableDB },
    { name: "range_pk_table.jed", build: rangePkTableDB },
    { name: "array_composite_table.jed", build: arrayCompositeTableDB },
    { name: "composite_array_field_table.jed", build: compositeArrayFieldTableDB },
    { name: "tall_tree.jed", build: tallTreeDB },
  ];
  for (const c of cases) {
    const image = toImage(c.build(), GOLDEN_PAGE_SIZE, 1n);
    const want = fixture(c.name);
    assert.ok(
      bytesEqual(image, want),
      `${c.name}: serialized bytes differ (got ${image.length} B, want ${want.length} B)`,
    );
  }
});

// READ side: loading a golden reproduces the same rows the builder produced. The
// torn-meta goldens must read through the valid slot to the pk_table content.
test("read goldens reproduces rows", () => {
  const cases: { name: string; build: () => Database; table: string }[] = [
    { name: "one_table_empty.jed", build: oneTableEmptyDB, table: "t" },
    { name: "overflow_table.jed", build: overflowTableDB, table: "t" },
    { name: "compressed_table.jed", build: compressedTableDB, table: "t" },
    { name: "pk_table.jed", build: pkTableDB, table: "t" },
    { name: "text_table.jed", build: textTableDB, table: "t" },
    { name: "bool_table.jed", build: boolTableDB, table: "t" },
    { name: "bool_pk_table.jed", build: boolPkTableDB, table: "t" },
    { name: "decimal_table.jed", build: decimalTableDB, table: "t" },
    { name: "bytea_table.jed", build: byteaTableDB, table: "t" },
    { name: "text_pk_table.jed", build: textPkTableDB, table: "t" },
    { name: "bytea_pk_table.jed", build: byteaPkTableDB, table: "t" },
    { name: "decimal_pk_table.jed", build: decimalPkTableDB, table: "t" },
    { name: "uuid_table.jed", build: uuidTableDB, table: "t" },
    { name: "default_table.jed", build: defaultTableDB, table: "t" },
    { name: "default_expr_table.jed", build: defaultExprTableDB, table: "t" },
    { name: "timestamp_table.jed", build: timestampTableDB, table: "t" },
    { name: "date_table.jed", build: dateTableDB, table: "t" },
    { name: "timestamptz_table.jed", build: timestamptzTableDB, table: "t" },
    { name: "interval_table.jed", build: intervalTableDB, table: "t" },
    { name: "interval_pk_table.jed", build: intervalPkTableDB, table: "t" },
    { name: "float64_table.jed", build: float64TableDB, table: "t" },
    { name: "float32_table.jed", build: float32TableDB, table: "t" },
    { name: "nopk_table.jed", build: nopkTableDB, table: "r" },
    { name: "composite_pk_table.jed", build: compositePKTableDB, table: "t" },
    { name: "check_table.jed", build: checkTableDB, table: "t" },
    { name: "index_table.jed", build: indexTableDB, table: "t" },
    { name: "unique_table.jed", build: uniqueTableDB, table: "t" },
    { name: "gin_array_table.jed", build: ginArrayTableDB, table: "t" },
    { name: "gin_uuid_table.jed", build: ginUuidTableDB, table: "t" },
    { name: "fk_table.jed", build: fkTableDB, table: "c" },
    { name: "composite_type_table.jed", build: compositeTypeTableDB, table: "t" },
    { name: "nested_composite_table.jed", build: nestedCompositeTableDB, table: "t" },
    { name: "sequence_table.jed", build: sequenceTableDB, table: "t" },
    { name: "serial_table.jed", build: serialTableDB, table: "t" },
    { name: "identity_table.jed", build: identityTableDB, table: "t" },
    { name: "collation_table.jed", build: collationTableDB, table: "t" },
    { name: "array_table.jed", build: arrayTableDB, table: "t" },
    { name: "range_table.jed", build: rangeTableDB, table: "t" },
    { name: "range_pk_table.jed", build: rangePkTableDB, table: "t" },
    { name: "array_composite_table.jed", build: arrayCompositeTableDB, table: "t" },
    { name: "composite_array_field_table.jed", build: compositeArrayFieldTableDB, table: "t" },
    { name: "tall_tree.jed", build: tallTreeDB, table: "t" },
    { name: "torn_meta_slot0.jed", build: pkTableDB, table: "t" },
    { name: "torn_meta_slot1.jed", build: pkTableDB, table: "t" },
  ];
  for (const c of cases) {
    const loaded = loadDatabase(fixture(c.name));
    assert.deepStrictEqual(
      loaded.rowsInKeyOrder(c.table),
      c.build().rowsInKeyOrder(c.table),
      `${c.name}: rows`,
    );
  }
  // Empty database: zero tables, and a missing table reads as absent.
  const empty = loadDatabase(fixture("empty_db.jed"));
  assert.equal(empty.table("t"), undefined, "empty_db should have no tables");
});

// READ side, catalog detail: column names, types, and flags survive exactly.
test("read golden reconstructs catalog", () => {
  const loaded = loadDatabase(fixture("pk_table.jed"));
  const tbl = loaded.table("t");
  assert.ok(tbl, "table t missing");
  assert.equal(tbl!.name, "t");
  assert.equal(tbl!.columns.length, 2);
  const [id, v] = tbl!.columns;
  assert.deepStrictEqual(id, { name: "id", type: scalarT("i32"), decimal: null, primaryKey: true, notNull: true, default: null, defaultExpr: null, identity: null, collation: null });
  assert.deepStrictEqual(v, { name: "v", type: scalarT("i16"), decimal: null, primaryKey: false, notNull: false, default: null, defaultExpr: null, identity: null, collation: null });
  // A NULL value round-trips (id 3's v).
  const rows = loaded.rowsInKeyOrder("t");
  assert.deepStrictEqual(rows[2], [{ kind: "int", int: 3n }, { kind: "null" }]);
});

// A no-PK table's monotonic rowid counter must be reconstructed on load, so inserts
// after a load don't collide with persisted rowids (the step-6 mutation fix).
test("rowid counter survives load", () => {
  const image = toImage(nopkTableDB(), GOLDEN_PAGE_SIZE, 1n); // existing rows take rowids 0, 1, 2
  const loaded = loadDatabase(image);
  // The next insert must get rowid 3, not 0 — otherwise it collides (23505).
  execute(loaded, "INSERT INTO r VALUES (10, 100)");
  assert.equal(loaded.rowsInKeyOrder("r").length, 4);
});

// A column DEFAULT survives serialize→load: a fresh INSERT omitting the defaulted columns
// applies the *persisted* defaults — proving the default value (not just its byte length)
// round-trips through the catalog (constraints.md §2).
test("default survives load", () => {
  const loaded = loadDatabase(fixture("default_table.jed"));
  run(loaded, "INSERT INTO t (id) VALUES (3)");
  const rows = loaded.rowsInKeyOrder("t")!;
  const last = rows[rows.length - 1]!;
  // id=3 takes every persisted default: n=0, note='none', maybe=NULL, req=7, plain=NULL.
  assert.deepStrictEqual(last[0], { kind: "int", int: 3n });
  assert.deepStrictEqual(last[1], { kind: "int", int: 0n });
  assert.deepStrictEqual(last[2], { kind: "text", text: "none" });
  assert.deepStrictEqual(last[3], { kind: "null" });
  assert.deepStrictEqual(last[4], { kind: "int", int: 7n });
  assert.deepStrictEqual(last[6], { kind: "null" });
});

// The default 8 KiB page size also round-trips, and re-serializing is deterministic. Built at 8192
// so the in-memory tree is sized for it (fan-out tracks the page size — format.md).
test("round trip at default page size", () => {
  const db = new Database();
  db.pageSize = 8192;
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i16)");
  for (let i = 1; i <= 20; i++) {
    const v = i === 3 ? "NULL" : `${i * 10}`;
    run(db, `INSERT INTO t VALUES (${i}, ${v})`);
  }
  const image = toImage(db, 8192, 1n);
  const loaded = loadDatabase(image);
  assert.deepStrictEqual(loaded.rowsInKeyOrder("t"), db.rowsInKeyOrder("t"));
  assert.ok(bytesEqual(toImage(loaded, 8192, 1n), image), "re-serialized bytes differ");
});

test("crc32 known vector", () => {
  assert.equal(crc32Ieee(new TextEncoder().encode("123456789")), 0xcbf43926);
});

test("serialize is deterministic", () => {
  const db = pkTableDB();
  assert.ok(bytesEqual(toImage(db, GOLDEN_PAGE_SIZE, 1n), toImage(db, GOLDEN_PAGE_SIZE, 1n)));
});

test("corrupt image is rejected with XX001", () => {
  const image = toImage(pkTableDB(), GOLDEN_PAGE_SIZE, 1n);
  image[0] ^= 0xff; // smash slot 0 magic
  image[GOLDEN_PAGE_SIZE] ^= 0xff; // smash slot 1 magic
  assert.throws(
    () => loadDatabase(image),
    (e: unknown) => e instanceof Error && e.message.startsWith("XX001"),
  );
});
