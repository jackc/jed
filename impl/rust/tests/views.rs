// Views through the host API (spec/design/views.md). The corpus pins view semantics through the
// materialized execute path; these per-core tests pin only what it cannot express: a prepared
// statement over a view never serves a stale plan (across OR REPLACE and renames too), the lazy
// `query()` lanes classify a read of a `nextval` view as a write, function EXECUTE is checked
// against the reader through a view body, views survive a file commit + reopen, and the
// dependency-order tooling accessor the CLI dump uses.

use jed::value::Value;
use jed::{CreateOptions, Database, Outcome, Privilege, PrivilegeSet, Session, SessionOptions};

fn mem() -> Session {
    Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default())
}

fn exec(s: &mut Session, sql: &str) {
    s.query_outcome(sql, &[])
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message));
}

fn rows(s: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match s
        .query_outcome(sql, &[])
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message))
    {
        Outcome::Query { rows, .. } => rows,
        other => panic!("expected rows from {sql:?}, got {other:?}"),
    }
}

fn code(s: &mut Session, sql: &str) -> String {
    s.query_outcome(sql, &[])
        .expect_err("expected an error")
        .code()
        .to_string()
}

fn drain(s: &mut Session, stmt: &jed::PreparedStatement) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    let mut r = s.query_prepared(stmt, &[]).unwrap();
    for row in &mut r {
        out.push(row);
    }
    r.error().unwrap();
    out
}

/// A prepared statement over a view re-plans after the view is dropped and recreated with another
/// definition, and after its underlying table changes (views.md §4: a view is never cached as a
/// stale plan — the view expands at every planning).
#[test]
fn prepared_view_query_never_serves_a_stale_plan() {
    let mut s = mem();
    exec(&mut s, "CREATE TABLE t (a i32 PRIMARY KEY, b i32)");
    exec(&mut s, "INSERT INTO t VALUES (1, 10), (2, 20)");
    exec(&mut s, "CREATE VIEW v AS SELECT a, b FROM t WHERE a = 1");
    let stmt = s.prepare("SELECT b FROM v").unwrap();
    assert_eq!(drain(&mut s, &stmt), vec![vec![Value::Int(10)]]);
    assert_eq!(drain(&mut s, &stmt), vec![vec![Value::Int(10)]]);

    exec(&mut s, "DROP VIEW v");
    exec(&mut s, "CREATE VIEW v AS SELECT a, b FROM t WHERE a = 2");
    assert_eq!(drain(&mut s, &stmt), vec![vec![Value::Int(20)]]);

    exec(&mut s, "UPDATE t SET b = 21 WHERE a = 2");
    assert_eq!(drain(&mut s, &stmt), vec![vec![Value::Int(21)]]);

    exec(&mut s, "ALTER TABLE t RENAME COLUMN b TO bb");
    assert_eq!(drain(&mut s, &stmt), vec![vec![Value::Int(21)]]);

    exec(&mut s, "DROP VIEW v");
    assert_eq!(
        s.query_prepared(&stmt, &[])
            .err()
            .map(|e| e.code().to_string()),
        Some("42P01".to_string())
    );
}

/// A prepared statement over a view follows `CREATE OR REPLACE VIEW` and a view rename (views.md
/// §1.2/§7.1): the replacement's rows come back on the next execution, and a renamed view is gone
/// under its old name while a view that reads it keeps working.
#[test]
fn prepared_view_query_follows_replace_and_rename() {
    let mut s = mem();
    exec(&mut s, "CREATE TABLE t (a i32 PRIMARY KEY, b i32)");
    exec(&mut s, "INSERT INTO t VALUES (1, 10), (2, 20)");
    exec(&mut s, "CREATE VIEW v AS SELECT a, b FROM t WHERE a = 1");
    exec(&mut s, "CREATE VIEW w AS SELECT b FROM v");
    let direct = s.prepare("SELECT b FROM v").unwrap();
    let through = s.prepare("SELECT b FROM w").unwrap();
    assert_eq!(drain(&mut s, &direct), vec![vec![Value::Int(10)]]);
    assert_eq!(drain(&mut s, &through), vec![vec![Value::Int(10)]]);

    exec(
        &mut s,
        "CREATE OR REPLACE VIEW v AS SELECT a, b, a + b AS c FROM t WHERE a = 2",
    );
    assert_eq!(drain(&mut s, &direct), vec![vec![Value::Int(20)]]);
    assert_eq!(drain(&mut s, &through), vec![vec![Value::Int(20)]]);

    exec(&mut s, "ALTER VIEW v RENAME TO v2");
    assert_eq!(
        s.query_prepared(&direct, &[])
            .err()
            .map(|e| e.code().to_string()),
        Some("42P01".to_string())
    );
    assert_eq!(drain(&mut s, &through), vec![vec![Value::Int(20)]]);

    exec(&mut s, "ALTER VIEW v2 RENAME COLUMN b TO bb");
    assert_eq!(drain(&mut s, &through), vec![vec![Value::Int(20)]]);
    assert_eq!(
        rows(&mut s, "SELECT bb FROM v2"),
        vec![vec![Value::Int(20)]]
    );
}

/// The lazy `query()` lanes classify a read of a view whose body calls `nextval` as a write
/// (views.md §4.1, sequences.md §4): inside a READ ONLY block it is 25006, and in autocommit it
/// advances the sequence exactly once per read.
#[test]
fn query_lane_sees_a_nextval_view_as_a_write() {
    let mut s = mem();
    exec(&mut s, "CREATE SEQUENCE s");
    exec(&mut s, "CREATE VIEW vs AS SELECT nextval('s') AS n");
    let first: Vec<Vec<Value>> = s.query("SELECT n FROM vs", &[]).unwrap().collect();
    assert_eq!(first, vec![vec![Value::Int(1)]]);
    exec(&mut s, "CREATE VIEW vvs AS SELECT n FROM vs");
    let second: Vec<Vec<Value>> = s.query("SELECT n FROM vvs", &[]).unwrap().collect();
    assert_eq!(second, vec![vec![Value::Int(2)]]);

    exec(&mut s, "BEGIN READ ONLY");
    let err = match s.query("SELECT n FROM vvs", &[]) {
        Ok(_) => panic!("a nextval view read in a READ ONLY block must fail"),
        Err(e) => e,
    };
    assert_eq!(err.code(), "25006");
    exec(&mut s, "ROLLBACK");
    // A CTE of the same name shadows the view, so this read is not a write.
    exec(&mut s, "BEGIN READ ONLY");
    assert_eq!(
        rows(&mut s, "WITH vs AS (SELECT 7 AS n) SELECT n FROM vs"),
        vec![vec![Value::Int(7)]]
    );
    exec(&mut s, "ROLLBACK");
}

/// Reading a view needs SELECT on the view only, but the functions its body calls need the
/// reader's EXECUTE, as in a derived table (views.md §9).
#[test]
fn view_body_functions_need_the_readers_execute() {
    let mut s = mem();
    exec(&mut s, "CREATE TABLE t (a i32 PRIMARY KEY)");
    exec(&mut s, "INSERT INTO t VALUES (-5)");
    exec(&mut s, "CREATE VIEW v AS SELECT abs(a) AS x FROM t");
    exec(&mut s, "CREATE VIEW w AS SELECT x FROM v");
    assert_eq!(rows(&mut s, "SELECT x FROM w"), vec![vec![Value::Int(5)]]);
    s.revoke(PrivilegeSet::EMPTY.with(Privilege::Execute), "abs");
    assert_eq!(code(&mut s, "SELECT x FROM w"), "42501");
    assert_eq!(code(&mut s, "SELECT x FROM v"), "42501");
}

/// Views persist through a file commit and reopen, keep their stored text, and still read; a
/// rename rewrite made in one session is what the next open sees (views.md §3/§7).
#[test]
fn views_survive_commit_and_reopen() {
    let dir = tempfile_dir();
    let path = dir.join("views.jed");
    {
        let db = Database::create(CreateOptions {
            path: Some(path.clone()),
            ..Default::default()
        })
        .unwrap();
        let mut s = db.session(SessionOptions::default());
        exec(&mut s, "CREATE TABLE t (a i32 PRIMARY KEY, b text)");
        exec(&mut s, "INSERT INTO t VALUES (1, 'x'), (2, 'y')");
        exec(&mut s, "CREATE VIEW v AS SELECT * FROM t WHERE a > 1");
        exec(&mut s, "CREATE VIEW w (k) AS SELECT a FROM v");
        exec(&mut s, "ALTER TABLE t RENAME TO t2");
    }
    let db = Database::open(&path).unwrap();
    let mut s = db.session(SessionOptions::default());
    assert_eq!(
        rows(
            &mut s,
            "SELECT name, definition FROM jed_views ORDER BY name"
        ),
        vec![
            vec![
                Value::Text("v".into()),
                Value::Text(
                    "select \"t\" . \"a\" , \"t\" . \"b\" from \"t2\" as \"t\" where a > 1".into()
                ),
            ],
            vec![
                Value::Text("w".into()),
                Value::Text("select a from v".into())
            ],
        ]
    );
    assert_eq!(rows(&mut s, "SELECT k FROM w"), vec![vec![Value::Int(2)]]);
    assert_eq!(code(&mut s, "DROP TABLE t2"), "2BP01");
    drop(s);
    // Compaction streams the from-scratch image, view entries included (api.md §2.6).
    db.compact("main").unwrap();
    db.close().unwrap();
    let db = Database::open(&path).unwrap();
    let mut s = db.session(SessionOptions::default());
    assert_eq!(rows(&mut s, "SELECT k FROM w"), vec![vec![Value::Int(2)]]);
    drop(s);
    db.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The CLI dump's tooling accessor lists views in dependency order, ties in name byte order
/// (views.md §11).
#[test]
fn views_in_dependency_order() {
    let mut s = mem();
    exec(&mut s, "CREATE TABLE t (a i32 PRIMARY KEY)");
    exec(&mut s, "CREATE VIEW z AS SELECT a FROM t");
    exec(&mut s, "CREATE VIEW b AS SELECT a FROM z");
    exec(&mut s, "CREATE VIEW a AS SELECT a FROM b");
    exec(&mut s, "CREATE VIEW m AS SELECT a FROM t");
    let order: Vec<String> = s
        .views_in_dependency_order()
        .into_iter()
        .map(|v| v.name)
        .collect();
    assert_eq!(order, vec!["m", "z", "b", "a"]);
}

/// A fresh directory for a file-backed test (removed by the test).
fn tempfile_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "jed-views-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
