//! Golden-file cross-core test (CLAUDE.md §8). The load-bearing honesty test for the
//! on-disk format: each core must (a) READ a checked-in golden into the expected
//! catalog + rows, and (b) WRITE the same logical database to bytes equal to the
//! golden EXACTLY. Because the format is deterministic, this gives
//! `rust-bytes == golden == go-bytes`, so each core can read the other's output
//! without any live cross-process exchange. Goldens are authored at page_size 256 by
//! spec/fileformat/verify.rb (the independent reference).

use jed::types::ScalarType;
use jed::value::Value;
use jed::{Database, execute};
use std::path::PathBuf;

/// The page size the goldens are authored at (small, so the hex stays reviewable).
const GOLDEN_PAGE_SIZE: u32 = 256;

/// A function that builds one of the sample databases the goldens correspond to.
type Builder = fn() -> Database;

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../spec/fileformat/fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn run(db: &mut Database, sql: &str) {
    execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message));
}

/// `CREATE TABLE t (id int32 PRIMARY KEY, v int16)` with 20 rows (id 3 has a NULL
/// value) — enough rows to span more than one data page at page_size 256.
fn pk_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)");
    for i in 1..=20i64 {
        let v = if i == 3 {
            "NULL".to_string()
        } else {
            (i * 10).to_string()
        };
        run(&mut db, &format!("INSERT INTO t VALUES ({i}, {v})"));
    }
    db
}

fn one_table_empty_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)");
    db
}

/// A table with a COMPOSITE primary key (constraints.md §3) — the stored key is the
/// concatenation of the members' encodings (4-byte int32 ‖ 2-byte int16, encoding.md §2.3).
/// Rows insert in ascending tuple order (the tree shape is order-sensitive), with a negative
/// first component and first-component ties broken by the second.
fn composite_pk_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(
        &mut db,
        "CREATE TABLE t (a int32, b int16, v int16, PRIMARY KEY (a, b))",
    );
    for (a, b, v) in [
        (-2, 5, 10),
        (1, 1, 20),
        (1, 2, 30),
        (1, 3, 40),
        (2, 0, 50),
        (2, 1, 60),
        (3, 7, 70),
        (3, 9, 80),
    ] {
        run(&mut db, &format!("INSERT INTO t VALUES ({a}, {b}, {v})"));
    }
    db
}

/// A table with CHECK constraints (constraints.md §4) — exercises the v4 catalog check
/// list: an auto-named single-column check, an explicitly-named multi-column check, and a
/// check whose persisted text exercises the token rendering (string literal with a doubled
/// quote, decimal literals, `>=`/`<=`), stored in name order
/// (price_range < t_b_check < t_note_check).
fn check_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(
        &mut db,
        "CREATE TABLE t (a int PRIMARY KEY, b int CHECK (b > 0), price numeric(8,2), \
         CONSTRAINT price_range CHECK (price >= 0.50 AND price <= 9999.99), note text, \
         CHECK (note = 'ok' OR note = 'a''b'))",
    );
    run(
        &mut db,
        "INSERT INTO t VALUES (1, 5, 1.00, 'ok'), (2, NULL, 9999.99, 'a''b'), \
         (3, 100, 0.50, 'ok')",
    );
    db
}

/// A table with SECONDARY INDEXES (v5 — spec/design/indexes.md): the catalog reshape +
/// the index trees. The PK list order (b, a) differs from declaration order (the lifted
/// composite-PK narrowing); `i_u` covers a nullable uuid column holding a NULL (the
/// encoding.md §2.2 presence tag in stored index order — NULL last), and the unnamed
/// index auto-names to `t_a_b_idx`. Index records have empty payloads (key only).
fn index_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(
        &mut db,
        "CREATE TABLE t (a int32, b int32, u uuid, PRIMARY KEY (b, a))",
    );
    run(&mut db, "CREATE INDEX i_u ON t (u)");
    run(&mut db, "CREATE INDEX ON t (a, b)");
    run(
        &mut db,
        "INSERT INTO t VALUES (1, 10, '550e8400-e29b-41d4-a716-446655440000'),          (2, 10, NULL), (3, 20, '00000000-0000-0000-0000-000000000000')",
    );
    db
}

/// A table with UNIQUE indexes (v6 — the per-index flags byte, indexes.md §8): `t_v_key`
/// (a UNIQUE constraint's auto-name) over a nullable column holding two NULLs (NULLS
/// DISTINCT — both stored), the named two-column constraint `wv`, a CREATE UNIQUE INDEX
/// `uq`, and the plain index `nu` (flags 0 beside flags 1).
fn unique_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(
        &mut db,
        "CREATE TABLE t (id int32 PRIMARY KEY, v int32, w int32, UNIQUE (v), CONSTRAINT wv UNIQUE (w, v))",
    );
    run(&mut db, "CREATE INDEX nu ON t (v)");
    run(&mut db, "CREATE UNIQUE INDEX uq ON t (w)");
    run(
        &mut db,
        "INSERT INTO t VALUES (1, 10, 100), (2, NULL, 200), (3, NULL, 300)",
    );
    db
}

/// A table with no primary key — exercises the stored synthetic int64 rowid key.
fn nopk_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(&mut db, "CREATE TABLE r (a int16, b int64)");
    for (a, b) in [(7, 70), (8, 80), (9, 90)] {
        run(&mut db, &format!("INSERT INTO r VALUES ({a}, {b})"));
    }
    db
}

/// 18 rows whose wide text padding forces a HEIGHT-2 tree (an interior node whose children are
/// themselves interior nodes) at page_size 256 — exercises interior-of-interior child pointers and
/// post-order page allocation across a deeper tree (spec/fileformat/format.md).
fn tall_tree_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, pad text)");
    for i in 1..=18i64 {
        let pad = format!("row-{i:02}-{}", "x".repeat(48));
        run(&mut db, &format!("INSERT INTO t VALUES ({i}, '{pad}')"));
    }
    db
}

/// A table with a text column — exercises the value codec's text branch (u16 length +
/// UTF-8 bytes): the empty string, an embedded quote, a 2-byte char (é), a NULL text
/// value, and a 4-byte astral char (😀). The PK stays int32 (no text key this slice).
fn text_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, s text)");
    run(&mut db, "INSERT INTO t VALUES (1, 'alice')");
    run(&mut db, "INSERT INTO t VALUES (2, '')");
    run(&mut db, "INSERT INTO t VALUES (3, 'O''Brien')");
    run(&mut db, "INSERT INTO t VALUES (4, 'café')");
    run(&mut db, "INSERT INTO t VALUES (5, NULL)");
    run(&mut db, "INSERT INTO t VALUES (6, '😀')");
    db
}

/// A table with a boolean column — exercises the value codec's boolean branch (a single
/// bool-byte, 0x00 false / 0x01 true) plus a NULL boolean. The PK stays int32 (no boolean
/// key this slice).
fn bool_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(
        &mut db,
        "CREATE TABLE t (id int32 PRIMARY KEY, flag boolean)",
    );
    run(&mut db, "INSERT INTO t VALUES (1, TRUE)");
    run(&mut db, "INSERT INTO t VALUES (2, FALSE)");
    run(&mut db, "INSERT INTO t VALUES (3, NULL)");
    db
}

/// A table with a decimal column — exercises the value codec's decimal branch (flags + u16
/// scale + u16 ndigits + base-10⁴ groups) and the catalog typmod: an unconstrained `numeric`
/// column `d` and a constrained `numeric(10,2)` column `m` (values already at scale 2, so a
/// no-op coercion). Covers positive, negative, zero, a multi-group coefficient, and a NULL.
fn decimal_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(
        &mut db,
        "CREATE TABLE t (id int32 PRIMARY KEY, d numeric, m numeric(10,2))",
    );
    run(
        &mut db,
        "INSERT INTO t VALUES (1, 1.50, 1.50), (2, -12345.6789, -12.34), \
         (3, 0.00, 0.00), (4, 100000000.000001, 100.00), (5, NULL, NULL)",
    );
    db
}

/// A table with a bytea column — exercises the value codec's bytea branch (u16 length + raw
/// bytes): a multi-byte value (a-f hex), the empty byte string, embedded 0x00 bytes, a high
/// byte (0xFF), a NULL, and a lone 0x00. The PK stays int32 (no bytea key this slice).
/// Literals are the `\x` hex input form, adapting to the bytea column (types.md §6).
fn bytea_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, b bytea)");
    run(&mut db, "INSERT INTO t VALUES (1, '\\xdeadbeef')");
    run(&mut db, "INSERT INTO t VALUES (2, '\\x')");
    run(&mut db, "INSERT INTO t VALUES (3, '\\x000102')");
    run(&mut db, "INSERT INTO t VALUES (4, '\\xff')");
    run(&mut db, "INSERT INTO t VALUES (5, NULL)");
    run(&mut db, "INSERT INTO t VALUES (6, '\\x00')");
    db
}

/// Incompressible filler (spec/fileformat/format.md "Fixtures"): xorshift32(seed "JEDB") mapped
/// to a 64-char alphabet (text) or raw bytes (bytea). High-entropy, so the LZ4 encoder never wins
/// store-smaller and the value deterministically stays PLAIN. Mirrors verify.rb's filler_text /
/// filler_bytes; each call restarts at the seed.
const ALPHA64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const FILLER_SEED: u32 = 0x4A45_4442;

fn filler_step(mut x: u32) -> u32 {
    x ^= x << 13;
    x ^= x >> 17;
    x ^ (x << 5)
}

fn filler_text(n: usize) -> String {
    let mut x = FILLER_SEED;
    let mut out = String::with_capacity(n);
    for _ in 0..n {
        x = filler_step(x);
        out.push(ALPHA64[(x % 64) as usize] as char);
    }
    out
}

fn filler_bytes_hex(n: usize) -> String {
    let mut x = FILLER_SEED;
    let mut out = String::with_capacity(n * 2);
    for _ in 0..n {
        x = filler_step(x);
        out.push_str(&format!("{:02x}", x % 256));
    }
    out
}

/// A table with large INCOMPRESSIBLE text + bytea values that spill OUT-OF-LINE PLAIN to overflow
/// pages (spec/design/large-values.md §12): at page_size 256 a ~600/300-byte value exceeds
/// RECORD_MAX (116); compression is attempted first (Slice B) but rejected by store-smaller, so
/// the record holds a 0x02 pointer and the raw bytes live in a page_type-4 chain. Row 1 spills
/// both columns (multi-page chains), row 2 stays inline, row 3 is NULL/NULL. Must match the Ruby
/// reference's OVERFLOW_TABLE (spec/fileformat/verify.rb).
fn overflow_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(
        &mut db,
        "CREATE TABLE t (id int32 PRIMARY KEY, body text, blob bytea)",
    );
    run(
        &mut db,
        &format!(
            "INSERT INTO t VALUES (1, '{}', '\\x{}')",
            filler_text(600),
            filler_bytes_hex(300)
        ),
    );
    run(&mut db, "INSERT INTO t VALUES (2, 'small', '\\xcafe')");
    run(&mut db, "INSERT INTO t VALUES (3, NULL, NULL)");
    db
}

/// A table with large COMPRESSIBLE values exercising Slice B's forms (large-values.md §13,
/// format.md "Large values", lz4.md): row 1's "x"-run text and 0xAB-run bytea both become 0x03
/// inline-compressed; row 2's half-filler/half-run text compresses to ~200 B — smaller than plain
/// but still over RECORD_MAX → 0x04 external-compressed (a chain carrying the COMPRESSED block);
/// row 3 stays inline-plain; row 4 is NULL/NULL. Must match the Ruby reference's COMPRESSED_TABLE.
fn compressed_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(
        &mut db,
        "CREATE TABLE t (id int32 PRIMARY KEY, body text, blob bytea)",
    );
    run(
        &mut db,
        &format!(
            "INSERT INTO t VALUES (1, '{}', '\\x{}')",
            "x".repeat(600),
            "ab".repeat(200)
        ),
    );
    run(
        &mut db,
        &format!(
            "INSERT INTO t VALUES (2, '{}{}', NULL)",
            filler_text(200),
            "y".repeat(200)
        ),
    );
    run(&mut db, "INSERT INTO t VALUES (3, 'tiny', '\\xcafe')");
    run(&mut db, "INSERT INTO t VALUES (4, NULL, NULL)");
    db
}

/// A table with a uuid PRIMARY KEY (the first golden with a NON-integer stored key — the
/// load-bearing §8 cross-core key-path proof) plus a nullable uuid column. Exercises the value
/// codec's fixed-16-byte uuid branch (no length prefix), the uuid key encoding (bare 16 bytes),
/// a present and a NULL uuid value, and the nil/max boundary UUIDs. Rows go in via INSERT and
/// the store sorts them into key (byte) order. Must match spec/fileformat/verify.rb's UUID_TABLE.
fn uuid_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(&mut db, "CREATE TABLE t (id uuid PRIMARY KEY, ref uuid)");
    run(
        &mut db,
        "INSERT INTO t VALUES \
         ('00000000-0000-0000-0000-000000000000', '550e8400-e29b-41d4-a716-446655440000'), \
         ('550e8400-e29b-41d4-a716-446655440000', NULL), \
         ('f47ac10b-58cc-4372-a567-0e02b2c3d479', '00000000-0000-0000-0000-000000000000'), \
         ('ffffffff-ffff-ffff-ffff-ffffffffffff', 'ffffffff-ffff-ffff-ffff-ffffffffffff')",
    );
    db
}

/// A table exercising the DEFAULT column constraint on disk — the catalog flags bit2 + the
/// pre-evaluated default value (written after the typmod). Covers an int default, a text
/// default, a DEFAULT NULL, a NOT NULL column with a default, a decimal default coerced to
/// numeric(6,2), and a plain no-default column. Row 1 takes every default; row 2 provides all.
fn default_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(
        &mut db,
        "CREATE TABLE t (id int32 PRIMARY KEY, n int32 DEFAULT 0, note text DEFAULT 'none', \
         maybe int32 DEFAULT NULL, req int32 NOT NULL DEFAULT 7, amt numeric(6,2) DEFAULT 1.5, \
         plain int16)",
    );
    run(&mut db, "INSERT INTO t (id) VALUES (1)");
    run(
        &mut db,
        "INSERT INTO t VALUES (2, 42, 'hi', 5, 9, 2.00, 100)",
    );
    db
}

/// A table with EXPRESSION column defaults (v8) — the catalog flags bit3 (default_is_expr) + the
/// expr-text written after the typmod: a `uuid DEFAULT uuidv7()`, an `int32 DEFAULT 1 + 1`, a
/// CONSTANT default beside them (bit2), and a plain no-default column. EMPTY table — the catalog
/// encoding is the cross-core proof; the per-row evaluation is covered by the conformance corpus.
fn default_expr_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(
        &mut db,
        "CREATE TABLE t (id int32 PRIMARY KEY, g uuid DEFAULT uuidv7(), n int32 DEFAULT 1 + 1, \
         k int32 DEFAULT 7, plain int16)",
    );
    db
}

/// A table with a timestamp column — exercises the value codec's int64-instant branch (type
/// code 8): a positive instant, a pre-1970 negative one, a BC-era one, the ±infinity sentinels,
/// and a NULL. The literals parse to the same micros the golden stores. The PK stays int32 (a
/// timestamp PK is supported, but the value-codec branch is the point here).
fn timestamp_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(
        &mut db,
        "CREATE TABLE t (id int32 PRIMARY KEY, ts timestamp)",
    );
    run(&mut db, "INSERT INTO t VALUES (1, '2024-01-01 12:00:00')");
    run(&mut db, "INSERT INTO t VALUES (2, '1969-12-31 23:59:59.5')");
    run(
        &mut db,
        "INSERT INTO t VALUES (3, '0001-01-01 00:00:00 BC')",
    );
    run(&mut db, "INSERT INTO t VALUES (4, '-infinity')");
    run(&mut db, "INSERT INTO t VALUES (5, 'infinity')");
    run(&mut db, "INSERT INTO t VALUES (6, NULL)");
    db
}

/// A table with a timestamptz column (type code 9) — the same 8-byte branch; the `+05` literal
/// normalizes to UTC before storage.
fn timestamptz_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(
        &mut db,
        "CREATE TABLE t (id int32 PRIMARY KEY, ts timestamptz)",
    );
    run(
        &mut db,
        "INSERT INTO t VALUES (1, '2024-01-01 12:00:00+00')",
    );
    run(
        &mut db,
        "INSERT INTO t VALUES (2, '2024-01-01 12:00:00+05')",
    );
    run(
        &mut db,
        "INSERT INTO t VALUES (3, '1969-12-31 23:59:59.5+00')",
    );
    run(&mut db, "INSERT INTO t VALUES (4, '-infinity')");
    run(&mut db, "INSERT INTO t VALUES (5, 'infinity')");
    run(&mut db, "INSERT INTO t VALUES (6, NULL)");
    db
}

/// A table with an interval column (type code 11) — the fixed 16-byte value-codec branch
/// (i32 months ‖ i32 days ‖ i64 micros). A positive multi-field value, a negative value, the
/// zero interval, a months-only `'1 mon'` vs a span-equal-but-byte-distinct `'30 days'`, and a
/// NULL. The bare-string literals adapt to the interval column. PK stays int32.
fn interval_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, d interval)");
    run(&mut db, "INSERT INTO t VALUES (1, '1 mon 2 days 03:04:05')");
    run(&mut db, "INSERT INTO t VALUES (2, '-1 day')");
    run(&mut db, "INSERT INTO t VALUES (3, '0 seconds')");
    run(&mut db, "INSERT INTO t VALUES (4, '1 mon')");
    run(&mut db, "INSERT INTO t VALUES (5, '30 days')");
    run(&mut db, "INSERT INTO t VALUES (6, NULL)");
    db
}

/// A table with a float64 column (type code 12) — the 8-byte IEEE value-codec branch. A positive
/// fraction, a negative value, +0 and -0 (the sign bit is preserved on disk — distinct bytes), both
/// infinities, a canonicalized NaN (stored as the single quiet pattern `0x7FF8…000`), a NULL, and
/// `f64::MAX` (a full mantissa). Finite values enter via bare numeric literals (decimal adaptation);
/// the specials enter via typed literals in `INSERT ... SELECT` (a VALUES slot takes only bare
/// literals this slice — float.md). PK stays int32 (float PK → 0A000).
fn float64_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, d float64)");
    run(&mut db, "INSERT INTO t VALUES (1, 1.5)");
    run(&mut db, "INSERT INTO t VALUES (2, -2.5)");
    run(&mut db, "INSERT INTO t VALUES (3, 0.0)");
    run(&mut db, "INSERT INTO t SELECT 4, float64 '-0'");
    run(&mut db, "INSERT INTO t SELECT 5, float64 'Infinity'");
    run(&mut db, "INSERT INTO t SELECT 6, float64 '-Infinity'");
    run(&mut db, "INSERT INTO t SELECT 7, float64 'NaN'");
    run(&mut db, "INSERT INTO t VALUES (8, NULL)");
    run(
        &mut db,
        "INSERT INTO t SELECT 9, float64 '1.7976931348623157e308'",
    );
    db
}

/// A table with a float32 column (type code 13) — the 4-byte IEEE branch. The same special-value
/// coverage as `float64_table_db` (canonicalized NaN → `0x7FC00000`) plus 100.25 (exactly
/// representable in binary32). PK stays int32.
fn float32_table_db() -> Database {
    let mut db = Database::with_page_size(GOLDEN_PAGE_SIZE);
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, r float32)");
    run(&mut db, "INSERT INTO t VALUES (1, 1.5)");
    run(&mut db, "INSERT INTO t VALUES (2, -2.5)");
    run(&mut db, "INSERT INTO t VALUES (3, 0.0)");
    run(&mut db, "INSERT INTO t SELECT 4, float32 '-0'");
    run(&mut db, "INSERT INTO t SELECT 5, float32 'Infinity'");
    run(&mut db, "INSERT INTO t SELECT 6, float32 '-Infinity'");
    run(&mut db, "INSERT INTO t SELECT 7, float32 'NaN'");
    run(&mut db, "INSERT INTO t VALUES (8, NULL)");
    run(&mut db, "INSERT INTO t VALUES (9, 100.25)");
    db
}

/// WRITE side: serializing the in-memory database reproduces the golden byte-exactly.
#[test]
fn write_matches_goldens() {
    let cases: &[(&str, Builder)] = &[
        ("empty_db.jed", Database::new),
        ("overflow_table.jed", overflow_table_db),
        ("compressed_table.jed", compressed_table_db),
        ("one_table_empty.jed", one_table_empty_db),
        ("pk_table.jed", pk_table_db),
        ("text_table.jed", text_table_db),
        ("bool_table.jed", bool_table_db),
        ("decimal_table.jed", decimal_table_db),
        ("bytea_table.jed", bytea_table_db),
        ("uuid_table.jed", uuid_table_db),
        ("default_table.jed", default_table_db),
        ("default_expr_table.jed", default_expr_table_db),
        ("timestamp_table.jed", timestamp_table_db),
        ("timestamptz_table.jed", timestamptz_table_db),
        ("interval_table.jed", interval_table_db),
        ("float64_table.jed", float64_table_db),
        ("float32_table.jed", float32_table_db),
        ("nopk_table.jed", nopk_table_db),
        ("composite_pk_table.jed", composite_pk_table_db),
        ("check_table.jed", check_table_db),
        ("index_table.jed", index_table_db),
        ("unique_table.jed", unique_table_db),
        ("tall_tree.jed", tall_tree_db),
    ];
    for (name, build) in cases {
        let image = build().to_image(GOLDEN_PAGE_SIZE, 1).unwrap();
        assert_eq!(image, fixture(name), "serialized bytes differ from {name}");
    }
}

/// READ side: loading a golden reproduces the same rows the builder produced. The
/// torn-meta goldens must read through the valid slot to the pk_table content.
#[test]
fn read_goldens_reproduces_rows() {
    let cases: &[(&str, Builder, &str)] = &[
        ("one_table_empty.jed", one_table_empty_db, "t"),
        ("overflow_table.jed", overflow_table_db, "t"),
        ("compressed_table.jed", compressed_table_db, "t"),
        ("pk_table.jed", pk_table_db, "t"),
        ("text_table.jed", text_table_db, "t"),
        ("bool_table.jed", bool_table_db, "t"),
        ("decimal_table.jed", decimal_table_db, "t"),
        ("bytea_table.jed", bytea_table_db, "t"),
        ("uuid_table.jed", uuid_table_db, "t"),
        ("default_table.jed", default_table_db, "t"),
        ("default_expr_table.jed", default_expr_table_db, "t"),
        ("timestamp_table.jed", timestamp_table_db, "t"),
        ("timestamptz_table.jed", timestamptz_table_db, "t"),
        ("interval_table.jed", interval_table_db, "t"),
        ("float64_table.jed", float64_table_db, "t"),
        ("float32_table.jed", float32_table_db, "t"),
        ("nopk_table.jed", nopk_table_db, "r"),
        ("composite_pk_table.jed", composite_pk_table_db, "t"),
        ("check_table.jed", check_table_db, "t"),
        ("index_table.jed", index_table_db, "t"),
        ("unique_table.jed", unique_table_db, "t"),
        ("tall_tree.jed", tall_tree_db, "t"),
        ("torn_meta_slot0.jed", pk_table_db, "t"),
        ("torn_meta_slot1.jed", pk_table_db, "t"),
    ];
    for (name, build, table) in cases {
        let loaded = Database::from_image(&fixture(name))
            .unwrap_or_else(|e| panic!("load {name}: {}", e.message));
        let expected = build();
        assert_eq!(
            loaded.rows_in_key_order(table),
            expected.rows_in_key_order(table),
            "rows from {name} differ",
        );
    }

    // Empty database: zero tables, and a missing table reads as None.
    let empty = Database::from_image(&fixture("empty_db.jed")).unwrap();
    assert!(empty.table("t").is_none());
}

/// READ side, catalog detail: column names, types, and flags survive exactly (a read
/// bug in an unexercised flag would otherwise slip past a rows-only check).
#[test]
fn read_golden_reconstructs_catalog() {
    let loaded = Database::from_image(&fixture("pk_table.jed")).unwrap();
    let t = loaded.table("t").expect("table t");
    assert_eq!(t.name, "t");
    assert_eq!(t.columns.len(), 2);

    assert_eq!(t.columns[0].name, "id");
    assert_eq!(t.columns[0].ty, ScalarType::Int32);
    assert!(t.columns[0].primary_key);
    assert!(t.columns[0].not_null);

    assert_eq!(t.columns[1].name, "v");
    assert_eq!(t.columns[1].ty, ScalarType::Int16);
    assert!(!t.columns[1].primary_key);
    assert!(!t.columns[1].not_null);

    // A NULL value round-trips (id 3's v).
    let rows = loaded.rows_in_key_order("t").unwrap();
    assert_eq!(rows[2], vec![Value::Int(3), Value::Null]);
}

/// A column DEFAULT survives serialize→load: after loading the golden, a fresh INSERT that
/// omits the defaulted columns applies the *persisted* defaults — proving the default value
/// (not just its byte length) round-trips through the catalog (constraints.md §2).
#[test]
fn default_survives_load() {
    let mut loaded = Database::from_image(&fixture("default_table.jed")).unwrap();
    run(&mut loaded, "INSERT INTO t (id) VALUES (3)");
    let rows = loaded.rows_in_key_order("t").unwrap();
    let last = rows.last().expect("a row");
    // id=3 (last in key order) takes every persisted default: n=0, note='none', maybe=NULL,
    // req=7, plain=NULL (and amt=1.50, not asserted here).
    assert_eq!(last[0], Value::Int(3));
    assert_eq!(last[1], Value::Int(0));
    assert_eq!(last[2], Value::Text("none".to_string()));
    assert_eq!(last[3], Value::Null);
    assert_eq!(last[4], Value::Int(7));
    assert_eq!(last[6], Value::Null);
}

/// A no-PK table's monotonic rowid counter must be reconstructed on load, so inserts
/// after a load don't collide with persisted rowids (the step-6 mutation fix).
#[test]
fn rowid_counter_survives_serialize_and_load() {
    let db = nopk_table_db(); // existing rows take rowids 0, 1, 2 (built at GOLDEN_PAGE_SIZE)
    let image = db.to_image(GOLDEN_PAGE_SIZE, 1).unwrap();
    let mut loaded = Database::from_image(&image).unwrap();
    // The next insert must get rowid 3, not 0 — otherwise it collides (23505).
    execute(&mut loaded, "INSERT INTO r VALUES (10, 100)").expect("insert after load");
    assert_eq!(loaded.rows_in_key_order("r").unwrap().len(), 4);
}

/// The default 8 KiB page size also round-trips (goldens stay at 256 for reviewable hex, but the
/// real default must work too). Built at 8192 so the in-memory tree is sized for it (the
/// page-backed B-tree's fan-out tracks the page size — spec/fileformat/format.md).
#[test]
fn round_trip_at_default_page_size() {
    let mut db = Database::with_page_size(8192);
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)");
    for i in 1..=20i64 {
        let v = if i == 3 {
            "NULL".to_string()
        } else {
            (i * 10).to_string()
        };
        run(&mut db, &format!("INSERT INTO t VALUES ({i}, {v})"));
    }
    let image = db.to_image(8192, 1).unwrap();
    let loaded = Database::from_image(&image).unwrap();
    assert_eq!(
        loaded.rows_in_key_order("t"),
        db.rows_in_key_order("t"),
        "8 KiB round trip preserves rows",
    );
    // Re-serializing the loaded database yields identical bytes (determinism).
    assert_eq!(loaded.to_image(8192, 1).unwrap(), image);
}
