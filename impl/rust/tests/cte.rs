//! Common table expressions — `WITH name [(cols)] AS [NOT] MATERIALIZED (query) [, …] <query>`,
//! non-recursive (spec/design/cte.md). The row/name/error assertions and the inline/materialize
//! cost contract live in the shared conformance corpus (spec/conformance/suites/cte/*.test). What
//! remains here is the MATERIALIZED / NOT MATERIALIZED hint cost split (13/23), which the corpus
//! pins by rows but NOT by cost.

use jed::{Database, execute};

fn db_with(stmts: &[&str]) -> Database {
    let mut db = Database::new();
    for s in stmts {
        execute(&mut db, s).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn cost(db: &mut Database, sql: &str) -> i64 {
    execute(db, sql)
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message))
        .cost()
}

/// A 3-row, single-node table `t(id, n)` = {(1,10),(2,20),(3,30)}.
fn t3() -> Database {
    db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, n int32)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
    ])
}

#[test]
fn materialized_hint_forces_buffering() {
    let mut db = t3();
    // MATERIALIZED forces a single-reference CTE to buffer: body once (7) + 3 cte_scan_row + 3
    // row_produced = 13 (vs the inlined 10).
    assert_eq!(
        cost(
            &mut db,
            "WITH c AS MATERIALIZED (SELECT id FROM t) SELECT id FROM c ORDER BY id"
        ),
        13
    );
    // NOT MATERIALIZED forces a two-reference CTE to inline (each reference re-runs the body): two
    // bodies (2x7) + 9 row_produced = 23 (vs the materialized 22).
    assert_eq!(
        cost(
            &mut db,
            "WITH c AS NOT MATERIALIZED (SELECT id FROM t) SELECT a.id, b.id FROM c a CROSS JOIN c b"
        ),
        23
    );
}
