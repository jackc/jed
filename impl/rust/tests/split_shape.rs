//! Split-point shape (spec/fileformat/format.md "Split point") — pins the tree shapes
//! the position-aware split rule produces, observed through the page_read block of a
//! full scan (= structural node count, cost.md §3). Ascending inserts take the
//! right-edge append split (byte-identical to the old always-largest-left rule, ~full
//! leaves); random-order inserts even out with a sibling or take the balanced split —
//! before the position-aware rule they splintered into [N-2 | 1] pairs and converged on a
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

/// A 1210-row table at the fixture page size (256): id bigint pk, v integer = id % 7.
/// Ascending inserts pks 0..1209 in order; shuffled inserts the permutation (i*37) mod
/// 1210 — deterministic, identical in every core. (121 rows until format v52, whose leaves
/// hold about ten times as many of these rows.)
fn split_shape_db(name: &str, shuffled: bool) -> (Database, Session) {
    split_shape_order_db(name, |i| if shuffled { (i * 37) % 1210 } else { i })
}

/// The same table with its 1210 rows inserted in the order `pk(0)`, `pk(1)`, ….
fn split_shape_order_db(name: &str, pk: impl Fn(i64) -> i64) -> (Database, Session) {
    let path = tmp(name);
    let _ = std::fs::remove_file(&path);
    let database = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        skip_fsync: true,
        page_size: 256,
        ..Default::default()
    })
    .unwrap();
    let mut db = database.session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id bigint PRIMARY KEY, v integer)");
    for i in 0..1210 {
        let pk = pk(i);
        run(&mut db, &format!("INSERT INTO t VALUES ({pk}, {})", pk % 7));
    }
    (database, db)
}

#[test]
fn split_shape_costs_are_pinned() {
    // The same logical table costs nearly the same full scan whichever order built it:
    // ascending packs ~full (append splits), shuffled lands a few nodes behind (balanced
    // splits). Under the old always-largest-left rule the shuffled tree splintered into
    // hundreds of near-empty nodes and this cost exploded. The counts dropped from v23
    // (268/278/156) with the v24 B+tree (format.md): interior nodes are a record-free
    // separator skeleton with far higher fan-out, so a full scan touches fewer pages.
    let (_, mut asc) = split_shape_db("split_shape_asc.jed", false);
    assert_eq!(cost(&mut asc, "SELECT count(*) FROM t"), 2436);
    let (shuf_db, mut shuf) = split_shape_db("split_shape_shuf.jed", true);
    assert_eq!(cost(&mut shuf, "SELECT count(*) FROM t"), 2436);

    // Evening out with a sibling (format.md): a leaf that overflows away from the tree's right edge
    // shares its records with a sibling that has room before it splits, which is why the shuffled
    // tree costs exactly what the ascending one does (2443 before the rule). Descending inserts
    // overflow at each leaf's left edge and even out with the right sibling; the zigzag order
    // (0, 1209, 1, 1208, …) grows two fronts that meet in the middle, so its overflows find room on
    // either side. Both land one node behind.
    let (_, mut desc) = split_shape_order_db("split_shape_desc.jed", |i| 1209 - i);
    assert_eq!(cost(&mut desc, "SELECT count(*) FROM t"), 2437);
    let (_, mut zigzag) = split_shape_order_db("split_shape_zigzag.jed", |i| {
        if i % 2 == 0 { i / 2 } else { 1209 - i / 2 }
    });
    assert_eq!(cost(&mut zigzag, "SELECT count(*) FROM t"), 2437);

    // Sorted index build (indexes.md §1) packs the index tree like the ascending case;
    // the build charges only its table scan, and the bounded lookup's cost pins the
    // index path's shape (pk ≡ 3 mod 7 in [0,1209] ⇒ 173 admitted rows for v = 3). The
    // lookup rose from v23's 100: a B+tree point lookup always descends to a leaf
    // (interior records could terminate a v23 descent early), so each of the 17 table
    // fetches pays the full root→leaf path. Re-pinned for format v52 (the compact leaf), and again
    // when evening out left the shuffled table tree one level shorter (1045 before).
    assert_eq!(cost(&mut shuf, "CREATE INDEX t_v_idx ON t (v)"), 1225);
    assert_eq!(cost(&mut shuf, "SELECT id FROM t WHERE v = 3"), 872);

    // The complete INSERT mutation guard: the committed shape above already contains table-leaf
    // and table-interior splits plus a split secondary index. In one working root, overwrite an
    // existing table row/index entry, append enough rows to split both trees again, and pin the
    // resulting exact node counts through cost. Rollback must restore the byte-exact committed image
    // and its original costs. This fails if working-tree mutation aliases the committed root.
    let before = shuf_db.to_image(256, 1).unwrap();
    shuf.begin(true).unwrap();
    run(&mut shuf, "UPDATE t SET v = 42 WHERE id = 1209");
    for id in 1210..=2209 {
        run(
            &mut shuf,
            &format!("INSERT INTO t VALUES ({id}, {})", id % 7),
        );
    }
    assert_eq!(cost(&mut shuf, "SELECT count(*) FROM t"), 4450);
    assert_eq!(cost(&mut shuf, "SELECT id FROM t WHERE v = 3"), 1907);
    shuf.rollback().unwrap();
    assert_eq!(shuf_db.to_image(256, 1).unwrap(), before);
    assert_eq!(cost(&mut shuf, "SELECT count(*) FROM t"), 2436);
    assert_eq!(cost(&mut shuf, "SELECT id FROM t WHERE v = 3"), 872);
}
