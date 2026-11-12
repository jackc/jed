//! The stored size of a `numeric(p,s)` column (spec/fileformat/format.md v51, spec/design/decimal.md
//! §6a): a column with `p ≤ 18` is a fixed-width leaf slot of the width of the integer type that
//! holds `10^p`, so a table of them is exactly as large as its integer twin; a wider or
//! unconstrained column keeps the variable-width codec. Sizes are internal state the corpus cannot
//! read; what the values are is pinned PostgreSQL-clean in
//! spec/conformance/suites/types/decimal_fixed_width.test, and the bytes by the
//! `decimal_fixed_table.jed` golden.

use jed::{CreateOptions, Database, Outcome, SessionOptions, Value};

const ROWS: i64 = 5000;

/// The committed size of a `ROWS`-row table `(id i64 PRIMARY KEY, v <ty>)` whose `v` is `expr` of
/// the row number `g`.
fn table_bytes(ty: &str, expr: &str) -> i64 {
    let db = Database::create(CreateOptions::default()).unwrap();
    let mut s = db.session(SessionOptions::default());
    s.execute(&format!("CREATE TABLE t (id i64 PRIMARY KEY, v {ty})"), &[])
        .unwrap();
    s.execute(
        &format!("INSERT INTO t SELECT g, {expr} FROM generate_series(1, {ROWS}) g"),
        &[],
    )
    .unwrap();
    // The values survive the stored form: every row reads back.
    match s.query_outcome("SELECT count(v) FROM t", &[]).unwrap() {
        Outcome::Query { rows, .. } => assert_eq!(rows[0][0], Value::Int(ROWS)),
        other => panic!("{other:?}"),
    }
    db.storage_bytes("main").unwrap()
}

#[test]
fn a_fixed_width_decimal_column_is_the_size_of_its_integer_twin() {
    // Each slot width against the integer column of the same width, holding the same integers.
    assert_eq!(
        table_bytes("numeric(4,2)", "(g % 9999)::numeric / 100"),
        table_bytes("i16", "g % 9999")
    );
    assert_eq!(
        table_bytes("numeric(9,2)", "(g * 7919)::numeric / 100"),
        table_bytes("i32", "g * 7919")
    );
    assert_eq!(
        table_bytes("numeric(18,2)", "(g * 7919)::numeric / 100"),
        table_bytes("i64", "g * 7919")
    );
    // The precision picks the width: 4 → 2 bytes, 5 → 4 bytes, 9 → 4 bytes, 10 → 8 bytes. Since
    // format v52 a leaf stores such a column as base plus offsets when that is smaller, so the same
    // nearby values take the same offset width whatever the slot's: a wider slot is never smaller,
    // and no longer strictly larger.
    let two = table_bytes("numeric(4,0)", "g % 9999");
    let four = table_bytes("numeric(5,0)", "g % 9999");
    let eight = table_bytes("numeric(10,0)", "g % 9999");
    assert!(two <= four && four <= eight);
    // Values spread across the slot's range do not narrow: the full 8-byte slot shows.
    let spread = table_bytes("numeric(18,0)", "(g % 2 * 2 - 1) * g * 100000000000");
    assert!(eight < spread, "{eight} vs {spread}");
    assert_eq!(four, table_bytes("numeric(9,0)", "g % 9999"));
    assert_eq!(eight, table_bytes("numeric(18,0)", "g % 9999"));
}

#[test]
fn a_wide_or_unconstrained_decimal_column_keeps_the_variable_codec() {
    let expr = "(g * 7919)::numeric / 100";
    let fixed = table_bytes("numeric(18,2)", expr);
    let wide = table_bytes("numeric(19,2)", expr);
    let unconstrained = table_bytes("numeric", expr);
    // p = 19 stores the same values in the same self-describing form as an unconstrained column,
    // and both are larger than the 8-byte slot.
    assert_eq!(wide, table_bytes("numeric(38,2)", expr));
    assert!(fixed < wide, "{fixed} vs {wide}");
    assert!(fixed < unconstrained, "{fixed} vs {unconstrained}");
}
