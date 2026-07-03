//! Array as a KEY — the parts the PG-clean oracle corpus cannot express (encoding.md §2.14, the
//! `array-elements-terminated` rule). The 1-D PRIMARY KEY surface (point lookup, 23505, ordering,
//! secondary index, UNIQUE, FK) agrees with PostgreSQL and is oracle-checked in `types/array_key.test`;
//! this file covers only what that corpus cannot:
//!   (a) the MULTIDIM / CUSTOM-LOWER-BOUND key tiebreak, where jed's consistent `array_cmp` order
//!       (encoding.md §2.14) deliberately differs from PostgreSQL's single-column ORDER BY (an
//!       abbreviated-key artifact — array.md §5), so it cannot be oracle-pinned;
//!   (b) the keyable-element gate — a `float`-element array PRIMARY KEY IS keyable (the §2.8 lift —
//!       `f64[]`/`f32[]`), while a composite-element array key is still rejected `0A000` (composite is
//!       not yet keyable).

use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

fn rows(db: &mut Session, sql: &str) -> Vec<String> {
    match db.execute(sql, &[]).unwrap() {
        Outcome::Query { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect::<Vec<_>>().join("|"))
            .collect(),
        other => panic!("expected query, got {other:?}"),
    }
}

fn err(db: &mut Session, sql: &str) -> String {
    match db.execute(sql, &[]) {
        Err(e) => e.code().to_string(),
        Ok(o) => panic!("expected error for {sql}, got {o:?}"),
    }
}

// --- (a) multidim / custom-lower-bound array-key ordering (jed's array_cmp, NOT PG's ORDER BY) ----

#[test]
fn multidim_and_lower_bound_key_order() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.execute("CREATE TABLE m (k i32[] PRIMARY KEY)", &[])
        .unwrap();
    // Same flattened elements / count but different shape, plus a custom lower bound. jed's array key
    // reproduces array_cmp: equal element prefix → fewer elements → smaller ndim → smaller lower
    // bound. So {1,2,3} (lb 1) < [2:4]={1,2,3} (lb 2) < {1,2,3,4} (1-D, count 4) < {{1,2},{3,4}}
    // (2-D, count 4). PostgreSQL's ORDER BY would put the 2-D value FIRST among the count-4 pair (the
    // abbreviated-key artifact jed avoids), so this order is jed-defined, not oracle-checked.
    for v in ["{1,2,3,4}", "{{1,2},{3,4}}", "{1,2,3}", "[2:4]={1,2,3}"] {
        db.execute(&format!("INSERT INTO m VALUES ('{v}')"), &[])
            .unwrap();
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

// --- (b) the keyable-element gate: float-element arrays ARE keyable; composite-element arrays are not

#[test]
fn float_element_array_key_is_keyable() {
    // A f64[] PRIMARY KEY is now allowed (the §2.8 float-key lift): the array key recurses into the
    // float-order-preserving element key, so the store iterates in array_cmp order — element-wise by
    // the float total order (-0=+0, NaN largest), shorter-prefix first.
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.execute("CREATE TABLE m (k f64[] PRIMARY KEY)", &[])
        .unwrap();
    // The '{…}' array literal coerces each element through f64 input, so the specials (NaN/Infinity)
    // arrive without an INSERT ... SELECT (which is 0A000 into an array column this slice).
    for v in ["{1.5,2.5}", "{1.5}", "{-Infinity}", "{NaN}", "{1.5,2.0}"] {
        db.execute(&format!("INSERT INTO m VALUES ('{v}')"), &[])
            .unwrap();
    }
    assert_eq!(
        rows(&mut db, "SELECT k FROM m ORDER BY k"),
        vec![
            "{-Infinity}".to_string(), // -Inf < everything finite
            "{1.5}".to_string(),       // shorter prefix sorts before {1.5,…}
            "{1.5,2}".to_string(),     // 2.0 renders as 2
            "{1.5,2.5}".to_string(),
            "{NaN}".to_string(), // NaN is the largest float
        ]
    );
}

#[test]
fn float_element_array_multidim_key_order() {
    // Multidim/lower-bound float-element array key tiebreak (jed's array_cmp, NOT PG's ORDER BY —
    // the abbreviated-key artifact §2.14/array.md §5). Same finite f64 element prefix → fewer elements
    // → smaller ndim → smaller lower bound, identical to the i32 case (a) but over float elements.
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.execute("CREATE TABLE m (k f64[] PRIMARY KEY)", &[])
        .unwrap();
    for v in [
        "{1.5,2.5,3.5,4.5}",
        "{{1.5,2.5},{3.5,4.5}}",
        "{1.5,2.5,3.5}",
        "[2:4]={1.5,2.5,3.5}",
    ] {
        db.execute(&format!("INSERT INTO m VALUES ('{v}')"), &[])
            .unwrap();
    }
    assert_eq!(
        rows(&mut db, "SELECT k FROM m ORDER BY k"),
        vec![
            "{1.5,2.5,3.5}".to_string(),         // lb 1, count 3
            "[2:4]={1.5,2.5,3.5}".to_string(),   // same elements/count, larger lower bound
            "{1.5,2.5,3.5,4.5}".to_string(),     // 1-D, count 4
            "{{1.5,2.5},{3.5,4.5}}".to_string(), // 2-D, count 4 (PG would sort this first)
        ]
    );
}

#[test]
fn composite_element_array_keys_are_rejected() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    // A composite-element array key is 0A000 (composite is not yet keyable — composite.md §6).
    db.execute("CREATE TYPE addr AS (street text, zip i32)", &[])
        .unwrap();
    assert_eq!(
        err(&mut db, "CREATE TABLE bad2 (k addr[] PRIMARY KEY)"),
        "0A000"
    );
    // float-element arrays, by contrast, ARE accepted everywhere a key is taken.
    db.execute("CREATE TABLE ok (id i32 PRIMARY KEY, k f32[] UNIQUE)", &[])
        .unwrap();
    db.execute("CREATE TABLE ok2 (id i32 PRIMARY KEY, k f64[])", &[])
        .unwrap();
    db.execute("CREATE INDEX ix ON ok2 (k)", &[]).unwrap();
}
