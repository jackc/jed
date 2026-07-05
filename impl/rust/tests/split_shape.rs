//! Split-point shape (spec/fileformat/format.md "Split point") — pins the tree shapes
//! the position-aware split rule produces, observed through the page_read block of a
//! full scan (= structural node count, cost.md §3). Ascending inserts take the
//! right-edge append split (byte-identical to the old always-largest-left rule, ~full
//! leaves); random-order inserts take the balanced split and settle near the classic
//! ~2/3 fill — before this rule they splintered into [N-2 | 1] pairs and converged on a
//! few-percent fill (the spec/design/benchmarks.md finding). CREATE INDEX inserts its
//! entries sorted (indexes.md §1), so a built index packs like the ascending case.
//! Mirrored in impl/go/split_shape_test.go and impl/ts/tests/split_shape.test.ts.

use std::path::PathBuf;

use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

fn tmp(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

fn run(db: &mut Session, sql: &str) -> Outcome {
    db.query_outcome(sql, &[])
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message))
}

fn cost(db: &mut Session, sql: &str) -> i64 {
    match run(db, sql) {
        Outcome::Statement { cost, .. } => cost,
        Outcome::Query { cost, .. } => cost,
    }
}

/// A 121-row table at the fixture page size (256): id bigint pk, v integer = id % 7.
/// Ascending inserts pks 0..120 in order; shuffled inserts the permutation (i*37) mod
/// 121 — deterministic, identical in every core.
fn split_shape_db(name: &str, shuffled: bool) -> Session {
    let path = tmp(name);
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        skip_fsync: true,
        page_size: 256,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id bigint PRIMARY KEY, v integer)");
    for i in 0..121 {
        let pk = if shuffled { (i * 37) % 121 } else { i };
        run(&mut db, &format!("INSERT INTO t VALUES ({pk}, {})", pk % 7));
    }
    db
}

#[test]
fn split_shape_costs_are_pinned() {
    // The same logical table costs nearly the same full scan whichever order built it:
    // ascending packs ~full (append splits), shuffled lands a few nodes behind (balanced
    // splits). Under the old always-largest-left rule the shuffled tree splintered into
    // hundreds of near-empty nodes and this cost exploded. The counts dropped from v23
    // (268/278/156) with the v24 B+tree (format.md): interior nodes are a record-free
    // separator skeleton with far higher fan-out, so a full scan touches fewer pages.
    let mut asc = split_shape_db("split_shape_asc.jed", false);
    assert_eq!(cost(&mut asc, "SELECT count(*) FROM t"), 258);
    let mut shuf = split_shape_db("split_shape_shuf.jed", true);
    assert_eq!(cost(&mut shuf, "SELECT count(*) FROM t"), 265);

    // Sorted index build (indexes.md §1) packs the index tree like the ascending case;
    // the build charges only its table scan, and the bounded lookup's cost pins the
    // index path's shape (pk ≡ 3 mod 7 in [0,120] ⇒ 17 admitted rows for v = 3). The
    // lookup rose from v23's 100: a B+tree point lookup always descends to a leaf
    // (interior records could terminate a v23 descent early), so each of the 17 table
    // fetches pays the full root→leaf path.
    assert_eq!(cost(&mut shuf, "CREATE INDEX t_v_idx ON t (v)"), 143);
    assert_eq!(cost(&mut shuf, "SELECT id FROM t WHERE v = 3"), 105);
}
