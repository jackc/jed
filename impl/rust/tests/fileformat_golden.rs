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
    let mut db = Database::new();
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
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, v int16)");
    db
}

/// A table with no primary key — exercises the stored synthetic int64 rowid key.
fn nopk_table_db() -> Database {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE r (a int16, b int64)");
    for (a, b) in [(7, 70), (8, 80), (9, 90)] {
        run(&mut db, &format!("INSERT INTO r VALUES ({a}, {b})"));
    }
    db
}

/// A table with a text column — exercises the value codec's text branch (u16 length +
/// UTF-8 bytes): the empty string, an embedded quote, a 2-byte char (é), a NULL text
/// value, and a 4-byte astral char (😀). The PK stays int32 (no text key this slice).
fn text_table_db() -> Database {
    let mut db = Database::new();
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
    let mut db = Database::new();
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
    let mut db = Database::new();
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

/// WRITE side: serializing the in-memory database reproduces the golden byte-exactly.
#[test]
fn write_matches_goldens() {
    let cases: &[(&str, Builder)] = &[
        ("empty_db.jed", Database::new),
        ("one_table_empty.jed", one_table_empty_db),
        ("pk_table.jed", pk_table_db),
        ("text_table.jed", text_table_db),
        ("bool_table.jed", bool_table_db),
        ("decimal_table.jed", decimal_table_db),
        ("nopk_table.jed", nopk_table_db),
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
        ("pk_table.jed", pk_table_db, "t"),
        ("text_table.jed", text_table_db, "t"),
        ("bool_table.jed", bool_table_db, "t"),
        ("decimal_table.jed", decimal_table_db, "t"),
        ("nopk_table.jed", nopk_table_db, "r"),
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

/// A no-PK table's monotonic rowid counter must be reconstructed on load, so inserts
/// after a load don't collide with persisted rowids (the step-6 mutation fix).
#[test]
fn rowid_counter_survives_serialize_and_load() {
    let db = nopk_table_db(); // existing rows take rowids 0, 1, 2
    let image = db.to_image(8192, 1).unwrap();
    let mut loaded = Database::from_image(&image).unwrap();
    // The next insert must get rowid 3, not 0 — otherwise it collides (23505).
    execute(&mut loaded, "INSERT INTO r VALUES (10, 100)").expect("insert after load");
    assert_eq!(loaded.rows_in_key_order("r").unwrap().len(), 4);
}

/// The default 8 KiB page size also round-trips (goldens stay at 256 for reviewable
/// hex, but the real default must work too).
#[test]
fn round_trip_at_default_page_size() {
    let db = pk_table_db();
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
