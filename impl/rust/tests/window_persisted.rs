//! A windowed aggregate whose ARGUMENT column is referenced ONLY inside the OVER call must still read
//! that column from a persisted (lazily-faulted) leaf. The touched-set collector has to descend into
//! each window function's args / FILTER (spec/design/window.md §5.2; large-values.md §14) — otherwise
//! the lazy/masked scan leaves the operand column unfetched and the aggregate folds NULL. This is the
//! on-disk read-path regression the in-memory conformance corpus cannot express (CLAUDE.md §10): it
//! only surfaced through the window_running_sum benchmark, which reads a committed file. Mirrored in
//! Go (window_persisted_test.go) and TS (tests/window_persisted.test.ts).

use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

const PAGE_SIZE: u32 = 256;

fn query_rows(db: &mut Session, sql: &str) -> Vec<Vec<jed::Value>> {
    match db.query_outcome(sql, &[]).unwrap() {
        Outcome::Query { rows, .. } => rows,
        _ => panic!("expected a query result"),
    }
}

/// A small-page file table whose `amount` column is not referenced outside the window functions
/// under test, committed + reopened so its rows fault in lazily. 40 rows over 256-byte pages span
/// several leaves, so the masked scan is genuinely exercised.
fn seed_persisted_window(path: &std::path::Path) -> Session {
    let _ = std::fs::remove_file(path);
    {
        let mut db = Database::create(CreateOptions {
            path: Some(std::path::PathBuf::from(path)),
            page_size: PAGE_SIZE,
        })
        .unwrap()
        .session(SessionOptions::default());
        db.query_outcome(
            "CREATE TABLE t (id i32 PRIMARY KEY, grp i32, amount i32)",
            &[],
        )
        .unwrap();
        for i in 1..=40 {
            db.query_outcome(
                &format!("INSERT INTO t VALUES ({i}, {}, {})", i % 3, i * 10),
                &[],
            )
            .unwrap();
        }
        drop(db);
    }
    Database::open(path)
        .unwrap()
        .session(SessionOptions::default())
}

#[test]
fn running_aggregate_over_persisted_column() {
    let path = std::env::temp_dir().join("jed_window_persisted_running.jed");
    let mut db = seed_persisted_window(&path);

    // Running SUM over the default frame — amount enters the touched set ONLY through the window arg.
    let rows = query_rows(
        &mut db,
        "SELECT id, sum(amount) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
    );
    assert_eq!(rows.len(), 40);
    let mut want: i64 = 0;
    for (i, r) in rows.iter().enumerate() {
        want += ((i + 1) * 10) as i64;
        assert_eq!(
            r[1].render(),
            want.to_string(),
            "row {} running sum — operand column read as NULL?",
            i + 1
        );
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

/// The windowed TOP-N optimization (spec/design/window.md §5.2) reads only the first OFFSET+LIMIT
/// scan rows, then folds the window over that prefix — over a persisted file it must still resolve the
/// operand column from the faulted leaves it touches. The on-disk read-path check the in-memory corpus
/// (window/topn.test) cannot express — the window_running_sum benchmark's regression twin.
#[test]
fn top_n_over_persisted_column() {
    let path = std::env::temp_dir().join("jed_window_persisted_topn.jed");
    let mut db = seed_persisted_window(&path);

    let rows = query_rows(
        &mut db,
        "SELECT id, sum(amount) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id LIMIT 5",
    );
    assert_eq!(rows.len(), 5);
    let mut want: i64 = 0;
    for (i, r) in rows.iter().enumerate() {
        want += ((i + 1) * 10) as i64;
        assert_eq!(
            r[1].render(),
            want.to_string(),
            "row {} running sum — operand column read as NULL from disk?",
            i + 1
        );
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn filter_and_offset_over_persisted_column() {
    let path = std::env::temp_dir().join("jed_window_persisted_filter.jed");
    let mut db = seed_persisted_window(&path);

    // A bounded moving MAX (frame path) plus an offset function whose value is a persisted column.
    let rows = query_rows(
        &mut db,
        "SELECT id, max(amount) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), lag(amount) OVER (ORDER BY id) FROM t ORDER BY id LIMIT 3",
    );
    let want_max = ["10", "20", "30"];
    let want_lag = ["NULL", "10", "20"];
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(r[1].render(), want_max[i], "row {} moving max", i + 1);
        assert_eq!(r[2].render(), want_lag[i], "row {} lag", i + 1);
    }

    // FILTER routes its predicate column through spec.filter; a running SUM of amount for ids whose
    // amount is a multiple of 20 (amounts 10,20,30,40 → only 20 and 40 pass → running NULL,20,20,60).
    let f = query_rows(
        &mut db,
        "SELECT id, sum(amount) FILTER (WHERE amount % 20 = 0) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id LIMIT 4",
    );
    let want_f = ["NULL", "20", "20", "60"];
    for (i, r) in f.iter().enumerate() {
        assert_eq!(r[1].render(), want_f[i], "filter row {}", i + 1);
    }
    drop(db);
    let _ = std::fs::remove_file(&path);
}
