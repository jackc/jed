//! Array as a KEY — the parts the PG-clean oracle corpus cannot express (encoding.md §2.14, the
//! `array-elements-terminated` rule). The 1-D PRIMARY KEY surface (point lookup, 23505, ordering,
//! secondary index, UNIQUE, FK) agrees with PostgreSQL and is oracle-checked in `types/array_key.test`;
//! this file covers only what that corpus cannot:
//!   (a) the MULTIDIM / CUSTOM-LOWER-BOUND key tiebreak, where jed's consistent `array_cmp` order
//!       (encoding.md §2.14) deliberately differs from PostgreSQL's single-column ORDER BY (an
//!       abbreviated-key artifact — array.md §5), so it cannot be oracle-pinned;
//!   (b) the keyable-element gate — a `float`-element or composite-element array PRIMARY KEY is
//!       rejected `0A000` (jed's determinism / composite-key narrowing), where PostgreSQL allows it.

use jed::{Database, Outcome, execute};

fn rows(db: &mut Database, sql: &str) -> Vec<String> {
    match execute(db, sql).unwrap() {
        Outcome::Query { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect::<Vec<_>>().join("|"))
            .collect(),
        other => panic!("expected query, got {other:?}"),
    }
}

fn err(db: &mut Database, sql: &str) -> String {
    match execute(db, sql) {
        Err(e) => e.code().to_string(),
        Ok(o) => panic!("expected error for {sql}, got {o:?}"),
    }
}

// --- (a) multidim / custom-lower-bound array-key ordering (jed's array_cmp, NOT PG's ORDER BY) ----

#[test]
fn multidim_and_lower_bound_key_order() {
    let mut db = Database::new();
    execute(&mut db, "CREATE TABLE m (k i32[] PRIMARY KEY)").unwrap();
    // Same flattened elements / count but different shape, plus a custom lower bound. jed's array key
    // reproduces array_cmp: equal element prefix → fewer elements → smaller ndim → smaller lower
    // bound. So {1,2,3} (lb 1) < [2:4]={1,2,3} (lb 2) < {1,2,3,4} (1-D, count 4) < {{1,2},{3,4}}
    // (2-D, count 4). PostgreSQL's ORDER BY would put the 2-D value FIRST among the count-4 pair (the
    // abbreviated-key artifact jed avoids), so this order is jed-defined, not oracle-checked.
    for v in ["{1,2,3,4}", "{{1,2},{3,4}}", "{1,2,3}", "[2:4]={1,2,3}"] {
        execute(&mut db, &format!("INSERT INTO m VALUES ('{v}')")).unwrap();
    }
    assert_eq!(
        rows(&mut db, "SELECT k FROM m ORDER BY k"),
        vec![
            "{1,2,3}".to_string(),
            "[2:4]={1,2,3}".to_string(),
            "{1,2,3,4}".to_string(),
            "{{1,2},{3,4}}".to_string(),
        ]
    );
}

// --- (b) the keyable-element gate: float / composite element arrays are NOT keyable -----------------

#[test]
fn non_keyable_element_array_keys_are_rejected() {
    let mut db = Database::new();
    // A float-element array key is 0A000 (float is the determinism carve-out — encoding.md §2.8); PG
    // allows a float8[] PRIMARY KEY.
    assert_eq!(
        err(&mut db, "CREATE TABLE bad (k f64[] PRIMARY KEY)"),
        "0A000"
    );
    // A composite-element array key is 0A000 (composite is not yet keyable — composite.md §6).
    execute(&mut db, "CREATE TYPE addr AS (street text, zip i32)").unwrap();
    assert_eq!(
        err(&mut db, "CREATE TABLE bad2 (k addr[] PRIMARY KEY)"),
        "0A000"
    );
    // The same gate applies to a secondary index and a UNIQUE constraint.
    assert_eq!(
        err(
            &mut db,
            "CREATE TABLE bad3 (id i32 PRIMARY KEY, k f64[] UNIQUE)"
        ),
        "0A000"
    );
    execute(&mut db, "CREATE TABLE ok (id i32 PRIMARY KEY, k f64[])").unwrap();
    assert_eq!(err(&mut db, "CREATE INDEX ix ON ok (k)"), "0A000");
}
