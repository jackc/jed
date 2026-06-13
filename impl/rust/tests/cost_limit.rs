//! Cost ceiling + deterministic abort (CLAUDE.md §13; spec/design/cost.md §6). A caller sets
//! `max_cost` on the handle; the instant a statement's accrued execution cost reaches it,
//! execution aborts with `54P01`. The conformance corpus
//! (spec/conformance/suites/resource/cost_limit.test) pins the cross-core abort points on small
//! tables; this exercises what it cannot — that the bound is on *actual* accrued cost, so a cheap
//! primary-key lookup survives a ceiling a full scan would blow, and that the abort threads through
//! SELECT / DELETE / UPDATE and a pathological expression.

use jed::{Database, Outcome, execute};

/// A table of `n` rows (id int32 PRIMARY KEY, v int32; v == id).
fn table(n: i64) -> Database {
    let mut db = Database::new();
    execute(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, v int32)").unwrap();
    let mut sql = String::from("INSERT INTO t VALUES ");
    for i in 1..=n {
        if i > 1 {
            sql.push(',');
        }
        sql.push_str(&format!("({i},{i})"));
    }
    execute(&mut db, &sql).unwrap();
    db
}

fn cost(db: &mut Database, sql: &str) -> i64 {
    match execute(db, sql).unwrap() {
        Outcome::Query { cost, .. } => cost,
        Outcome::Statement { cost, .. } => cost,
    }
}

/// Assert running `sql` aborts with `54P01` (cost limit exceeded).
fn assert_aborts(db: &mut Database, sql: &str) {
    match execute(db, sql) {
        Err(e) => assert_eq!(
            e.code(),
            "54P01",
            "expected cost-limit abort, got {}",
            e.code()
        ),
        Ok(_) => panic!("expected cost-limit abort, but `{sql}` succeeded"),
    }
}

#[test]
fn unlimited_by_default() {
    let mut db = table(100);
    // No ceiling set: a full scan runs to completion however expensive.
    assert_eq!(db.max_cost(), 0);
    let _ = cost(&mut db, "SELECT * FROM t");
}

#[test]
fn ceiling_above_cost_succeeds_below_aborts() {
    let mut db = table(50);
    // Measure the true cost of the full scan with no ceiling.
    let full = cost(&mut db, "SELECT v FROM t");
    assert!(
        full > 10,
        "expected a non-trivial full-scan cost, got {full}"
    );

    // A ceiling comfortably above the real cost lets it through, unchanged.
    db.set_max_cost(full + 100);
    assert_eq!(cost(&mut db, "SELECT v FROM t"), full);

    // A ceiling below the real cost aborts deterministically.
    db.set_max_cost(full / 2);
    assert_aborts(&mut db, "SELECT v FROM t");

    // Clearing the ceiling restores unlimited execution.
    db.set_max_cost(0);
    assert_eq!(cost(&mut db, "SELECT v FROM t"), full);
}

#[test]
fn ceiling_at_exact_cost_aborts() {
    let mut db = table(20);
    let full = cost(&mut db, "SELECT v FROM t");
    // The ceiling is the first *disallowed* value — accrued reaching it aborts (CLAUDE.md §13
    // "the instant accrued cost reaches it"). So a ceiling equal to the true cost aborts...
    db.set_max_cost(full);
    assert_aborts(&mut db, "SELECT v FROM t");
    // ...but one above it succeeds.
    db.set_max_cost(full + 1);
    assert_eq!(cost(&mut db, "SELECT v FROM t"), full);
}

#[test]
fn point_lookup_survives_a_ceiling_a_full_scan_blows() {
    let mut db = table(200);
    let full = cost(&mut db, "SELECT v FROM t");
    let lookup = cost(&mut db, "SELECT v FROM t WHERE id = 100");
    assert!(
        lookup * 4 < full,
        "point lookup ({lookup}) should be far cheaper than the full scan ({full})"
    );

    // A ceiling between the two: the cheap primary-key lookup runs, the full scan aborts. The
    // bound is on real accrued cost, not on the table size.
    let ceiling = (lookup + full) / 2;
    db.set_max_cost(ceiling);
    assert_eq!(cost(&mut db, "SELECT v FROM t WHERE id = 100"), lookup);
    assert_aborts(&mut db, "SELECT v FROM t");
}

#[test]
fn abort_threads_through_delete_and_update() {
    // DELETE and UPDATE scan + filter every row, so a low ceiling aborts them too — and the abort
    // is a normal error, so it rolls back (autocommit): the table is left untouched.
    let mut db = table(50);
    let scan_cost = cost(&mut db, "SELECT v FROM t");
    db.set_max_cost(scan_cost / 2);

    assert_aborts(&mut db, "DELETE FROM t WHERE v > 0");
    assert_aborts(&mut db, "UPDATE t SET v = v + 1 WHERE v > 0");

    // Nothing was mutated (the aborted statements rolled back).
    db.set_max_cost(0);
    assert_eq!(cost(&mut db, "SELECT v FROM t"), scan_cost);
    let n = match execute(&mut db, "SELECT v FROM t").unwrap() {
        Outcome::Query { rows, .. } => rows.len(),
        _ => unreachable!(),
    };
    assert_eq!(n, 50);
}

#[test]
fn pathological_expression_aborts_on_one_row() {
    // A single row with a deeply repeated expression accrues operator_eval per interior node; the
    // per-node eval guard (cost.md §6) stops it even though only one row is scanned.
    let mut db = table(1);
    // 1 + 1 + 1 + ... (many Adds) over the one row.
    let expr = vec!["1"; 80].join(" + ");
    let sql = format!("SELECT {expr} FROM t");
    let big = cost(&mut db, &sql);
    db.set_max_cost(big / 2);
    assert_aborts(&mut db, &sql);
}

#[test]
fn empty_bound_under_a_tiny_ceiling_succeeds() {
    // A provably-empty primary-key bound (contradictory range) reads no page and no row
    // (cost.md §3), so it accrues nothing and stays under even a ceiling of 1. (A point-lookup
    // *miss* like `id = 999` differs — it still visits the leaf the key would live in, charging
    // one page_read, so it is NOT zero-cost.)
    let mut db = table(10);
    db.set_max_cost(1);
    match execute(&mut db, "SELECT v FROM t WHERE id > 5 AND id < 5").unwrap() {
        Outcome::Query { rows, cost, .. } => {
            assert!(rows.is_empty());
            assert_eq!(
                cost, 0,
                "a provably-empty bound should accrue 0, got {cost}"
            );
        }
        _ => unreachable!(),
    }
}
