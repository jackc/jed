//! Large values held by reference in eager-lane buffers (spec/design/large-values.md §16). A
//! touched text column that is not a key column stays an unfetched reference in the eager lane's
//! relation buffer and resolves only when an expression reads it. With every chain payload corrupt
//! on disk, a query raises `XX001` exactly when an expression reads a corrupt value, and succeeds
//! when none does. Mirrors impl/go/buffered_large_values_test.go and
//! impl/ts/tests/buffered_large_values.test.ts. Uses lazy_large_values.rs's `seed` /
//! `corrupt_overflow_payloads`.

use super::lazy_large_values::{PAGE_SIZE, corrupt_overflow_payloads, remove_db, seed, tmp};
use jed::{CreateOptions, Database, OpenOptions, Outcome, Session, SessionOptions};

/// Queries over `seed`'s table, whose rows 1 and 2 hold the corrupt chains. The expected rows
/// (rendered, sorted) when no expression reads a corrupt value; `None` expects `XX001`.
const CASES: &[(&str, Option<&[&str]>)] = &[
    // Aggregate: the CASE reads body only for the intact rows 3 and 4, the FILTER only for them.
    (
        "SELECT sum(CASE WHEN id > 2 THEN length(body) ELSE 0 END) FROM t",
        Some(&["604"]),
    ),
    (
        "SELECT count(body) FILTER (WHERE id > 2) FROM t",
        Some(&["2"]),
    ),
    ("SELECT sum(length(body)) FROM t", None),
    // Join: b.body is no join key, so it is read only where the CASE takes it.
    (
        "SELECT a.id, CASE WHEN a.id > 2 THEN length(b.body) END FROM t a JOIN t b ON a.id = b.id",
        Some(&["1|NULL", "2|NULL", "3|600", "4|4"]),
    ),
    (
        "SELECT a.id, length(b.body) FROM t a JOIN t b ON a.id = b.id",
        None,
    ),
    // Window: the window reads only its key and the projection reads body only for rows 3 and 4.
    (
        "SELECT id, CASE WHEN id > 2 THEN length(body) END, row_number() OVER (ORDER BY id) FROM t",
        Some(&["1|NULL|1", "2|NULL|2", "3|600|3", "4|4|4"]),
    ),
    (
        "SELECT id, lag(length(body)) OVER (ORDER BY id) FROM t",
        None,
    ),
    // A correlated subquery reads o.body into its primary-key bound without evaluating it, so body is
    // a key column: resolved when t is scanned, the corrupt rows raise XX001.
    (
        "SELECT id, (SELECT count(*) FROM u WHERE u.k = o.body), row_number() OVER (ORDER BY id) FROM t o",
        None,
    ),
];

/// `seed` plus a text-keyed table the correlated case probes.
fn buffered_seed(db: &mut Session) {
    seed(db);
    db.query_outcome("CREATE TABLE u (k text PRIMARY KEY)", &[])
        .unwrap();
    db.query_outcome("INSERT INTO u VALUES ('tiny')", &[])
        .unwrap();
}

fn render_sorted(rows: &[Vec<jed::Value>]) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .map(|r| r.iter().map(|v| v.render()).collect::<Vec<_>>().join("|"))
        .collect();
    out.sort();
    out
}

fn outcome(db: &mut Session, sql: &str) -> jed::Result<(Vec<Vec<jed::Value>>, i64)> {
    match db.query_outcome(sql, &[])? {
        Outcome::Query { rows, cost, .. } => Ok((rows, cost)),
        _ => panic!("{sql}: expected a query result"),
    }
}

fn create_file(path: &std::path::Path) -> Session {
    let _ = std::fs::remove_file(path);
    Database::create(CreateOptions {
        path: Some(path.to_path_buf()),
        skip_fsync: true,
        page_size: PAGE_SIZE,
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default())
}

fn open_file(path: &std::path::Path) -> Session {
    Database::open_with_options(
        path,
        OpenOptions {
            skip_fsync: true,
            ..Default::default()
        },
    )
    .unwrap()
    .session(SessionOptions::default())
}

#[test]
fn buffered_large_values_resolve_when_read() {
    let path = tmp("jed_buffered.jed");
    {
        let mut db = create_file(&path);
        buffered_seed(&mut db);
    }
    corrupt_overflow_payloads(&path);
    let mut db = open_file(&path);
    // An unlimited work_mem keeps aggregation and hash joins in the eager lane (memory.md §5.4).
    db.set_work_mem(0);
    for &(sql, ok) in CASES {
        let got = outcome(&mut db, sql);
        match ok {
            None => {
                let err = got.err().unwrap_or_else(|| panic!("{sql}: want XX001"));
                assert_eq!(err.code(), "XX001", "{sql}");
            }
            Some(want) => {
                let (rows, _) = got.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
                assert_eq!(render_sorted(&rows), want, "{sql}");
            }
        }
    }
    drop(db);
    remove_db(&path);
}

/// The same queries cost the same and return the same rows on disk-read rows (buffered references)
/// as on resident rows.
#[test]
fn buffered_large_values_match_resident() {
    let path = tmp("jed_buffered_intact.jed");
    {
        let mut db = create_file(&path);
        buffered_seed(&mut db);
    }
    let mut disk = open_file(&path);
    disk.set_work_mem(0);
    let resident_path = tmp("jed_buffered_resident.jed");
    let mut resident = create_file(&resident_path);
    resident.set_work_mem(0);
    resident.query_outcome("BEGIN", &[]).unwrap();
    buffered_seed(&mut resident);
    for &(sql, _) in CASES {
        let (a_rows, a_cost) = outcome(&mut disk, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        let (b_rows, b_cost) =
            outcome(&mut resident, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        assert_eq!(render_sorted(&a_rows), render_sorted(&b_rows), "{sql}");
        assert_eq!(a_cost, b_cost, "{sql}");
    }
    drop(disk);
    drop(resident);
    remove_db(&path);
    remove_db(&resident_path);
}
