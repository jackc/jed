//! Secondary indexes (spec/design/indexes.md) — covers what the corpus suite
//! (`ddl/create_index.test`, `query/index_scan.test`) cannot: catalog introspection
//! (index definitions, name order), the v5 on-disk round-trip with index trees, the
//! file-backed paged-open + incremental-commit path, and transactional DDL. Mirrored in
//! impl/go/secondary_index_test.go and impl/ts/tests/secondary_index.test.ts.

use std::path::PathBuf;

use jed::value::Value;
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

fn ids(db: &mut Session, sql: &str) -> Vec<i64> {
    match run(db, sql) {
        Outcome::Query { rows, .. } => rows
            .iter()
            .map(|r| match r[0] {
                Value::Int(n) => n,
                ref v => panic!("expected an int id, got {v:?}"),
            })
            .collect(),
        _ => panic!("expected a query outcome"),
    }
}

fn err_code(db: &mut Session, sql: &str) -> String {
    db.query_outcome(sql, &[])
        .expect_err(&format!("expected an error from {sql:?}"))
        .code()
        .to_string()
}

/// The 20-row fixture the planner/cost tests run against: `v = i % 5` gives 4 rows per
/// value, so an equality admits 4 of 20.
fn db20() -> Session {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32, w i32)");
    for i in 1..=20 {
        run(
            &mut db,
            &format!("INSERT INTO t VALUES ({i}, {}, {i})", i % 5),
        );
    }
    db
}

/// Auto-naming matches PostgreSQL (oracle-probed, indexes.md §2): lowercased
/// `<table>_<cols>_idx` + the smallest free suffix; duplicates in the column list are
/// allowed and named through; an explicit name round-trips as written. The catalog holds
/// indexes in ascending lowercased-name order.
#[test]
fn auto_naming_matches_postgres() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE T (A i32 PRIMARY KEY, B i32)");
    run(&mut db, "CREATE INDEX ON T (B)"); // t_b_idx
    run(&mut db, "CREATE INDEX ON T (B)"); // t_b_idx1
    run(&mut db, "CREATE INDEX ON T (B)"); // t_b_idx2
    run(&mut db, "CREATE INDEX ON T (A, B)"); // t_a_b_idx
    run(&mut db, "CREATE INDEX ON T (B, B)"); // t_b_b_idx (duplicate column allowed — PG)
    run(&mut db, "CREATE INDEX Mine ON T (B)"); // explicit, original case kept
    let t = db.table("t").unwrap();
    let names: Vec<&str> = t.indexes.iter().map(|i| i.name.as_str()).collect();
    // Ascending lowercased-name order (the catalog/planner order).
    assert_eq!(
        names,
        vec![
            "Mine",
            "t_a_b_idx",
            "t_b_b_idx",
            "t_b_idx",
            "t_b_idx1",
            "t_b_idx2"
        ]
    );
    assert_eq!(t.indexes[1].columns, vec![0, 1]);
    assert_eq!(t.indexes[2].columns, vec![1, 1]);
    // The PK list is independent of the indexes.
    assert_eq!(t.pk_indices(), vec![0]);
}

/// DDL errors mirror PostgreSQL (oracle-probed, indexes.md §2): validation order is
/// table → columns (list order) → name collision; the relation namespace is shared with
/// tables; DROP mismatches are 42704/42809.
#[test]
fn ddl_errors_match_postgres() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (a i32 PRIMARY KEY, s f64)");
    // Table existence first (even with a bad column).
    assert_eq!(
        err_code(&mut db, "CREATE INDEX i ON nosuch (nope)"),
        "42P01"
    );
    // Column existence next — before the name-collision check (PG's order).
    run(&mut db, "CREATE INDEX taken ON t (a)");
    assert_eq!(err_code(&mut db, "CREATE INDEX taken ON t (nope)"), "42703");
    // f64 IS now a valid index column (the float-order-preserving key, encoding.md §2.8 — every
    // scalar is keyable; text/bytea covered in ddl/create_index.test).
    run(&mut db, "CREATE INDEX i ON t (s)");
    // Name collisions across the shared relation namespace: vs an index, vs a table,
    // and CREATE TABLE vs an index name.
    assert_eq!(err_code(&mut db, "CREATE INDEX taken ON t (a)"), "42P07");
    assert_eq!(err_code(&mut db, "CREATE INDEX t ON t (a)"), "42P07");
    assert_eq!(err_code(&mut db, "CREATE TABLE taken (x i32)"), "42P07");
    // DROP mismatches.
    assert_eq!(err_code(&mut db, "DROP INDEX nosuch"), "42704");
    assert_eq!(err_code(&mut db, "DROP INDEX t"), "42809");
    assert_eq!(err_code(&mut db, "DROP TABLE taken"), "42809");
    // DROP INDEX removes it; DROP TABLE drops its indexes and frees the names.
    run(&mut db, "DROP INDEX taken");
    assert_eq!(err_code(&mut db, "DROP INDEX taken"), "42704");
    run(&mut db, "CREATE INDEX taken ON t (a)");
    run(&mut db, "DROP TABLE t");
    run(&mut db, "CREATE TABLE taken (x i32)");
    // The lookahead keeps every word non-reserved (grammar.md §30): the unnamed form
    // over a table named `on`, and an index explicitly named `on`.
    run(&mut db, "CREATE TABLE on (x i32)");
    run(&mut db, "CREATE INDEX ON on (x)"); // unnamed form over the table named on
    assert_eq!(db.table("on").unwrap().indexes[0].name, "on_x_idx");
    run(&mut db, "DROP TABLE on"); // free the name `on` in the relation namespace
    run(&mut db, "CREATE TABLE q (x i32)");
    run(&mut db, "CREATE INDEX on ON q (x)"); // an index NAMED on
    assert_eq!(db.table("q").unwrap().indexes[0].name, "on");
    run(&mut db, "DROP INDEX on");
}

/// The planner picks the index for a first-column equality and the cost drops to the
/// index-bounded form (cost.md §3 "index-bounded scan"); a provably-empty bound reads
/// nothing; the PK bound wins over an index; the lowest-named index breaks ties.
#[test]
fn planner_costs_are_pinned() {
    let mut db = db20();
    // Full scan: page_read(1 node) + 20 storage_row_read + 20 filter evals (v = 3 is one
    // operator_eval per row) + 4 row_produced = 45.
    assert_eq!(cost(&mut db, "SELECT id FROM t WHERE v = 3"), 45);
    // The build scan: page_read(1) + 20 storage_row_read = 21.
    assert_eq!(cost(&mut db, "CREATE INDEX t_v_idx ON t (v)"), 21);
    // Index-bounded: page_read(1 index node) + 4 × page_read(1 table node point lookup)
    // + 4 storage_row_read + 4 filter evals + 4 row_produced = 17.
    assert_eq!(cost(&mut db, "SELECT id FROM t WHERE v = 3"), 17);
    assert_eq!(
        ids(&mut db, "SELECT id FROM t WHERE v = 3 ORDER BY id"),
        vec![3, 8, 13, 18]
    );
    // A NULL equality (3VL) and contradictory equalities are provably empty: no page, no
    // row — only the residual projection work is gone too, so the cost is 0.
    assert_eq!(cost(&mut db, "SELECT id FROM t WHERE v = NULL"), 0);
    assert_eq!(cost(&mut db, "SELECT id FROM t WHERE v = 1 AND v = 2"), 0);
    // The PK bound wins over the index (a point lookup, no second tree).
    // page_read(1) + 1 row + 3 operator_evals (two compares + AND) + 1 produced = 6.
    assert_eq!(cost(&mut db, "SELECT id FROM t WHERE id = 7 AND v = 2"), 6);
    // A non-first-column equality cannot use the index: full scan.
    run(&mut db, "CREATE INDEX two ON t (w, v)");
    assert_eq!(cost(&mut db, "SELECT id FROM t WHERE v = 3"), 17); // t_v_idx still
    run(&mut db, "DROP INDEX t_v_idx");
    assert_eq!(cost(&mut db, "SELECT id FROM t WHERE v = 3"), 45); // `two` can't serve v
    // First-column equality on the composite index works (the entry's tail component is
    // skipped to reach the row key); the lowest lowercased name wins a tie. One admitted
    // row: index node + point lookup + row read + filter eval + produced = 5.
    assert_eq!(cost(&mut db, "SELECT id FROM t WHERE w = 7"), 5);
    run(&mut db, "CREATE INDEX a_first ON t (w)");
    assert_eq!(cost(&mut db, "SELECT id FROM t WHERE w = 7"), 5); // a_first (same shape)
    // DROP INDEX returns the scan to full cost (1 + 20 + 20 + 1 = 42).
    run(&mut db, "DROP INDEX a_first");
    run(&mut db, "DROP INDEX two");
    assert_eq!(cost(&mut db, "SELECT id FROM t WHERE w = 7"), 42);
}

/// The v5 image round-trips: index trees (including a NULL entry), the out-of-order PK
/// list, and a second-generation serialize are byte-stable; a reloaded database still
/// uses (and maintains) its indexes.
#[test]
fn round_trips_through_the_on_disk_image() {
    let mut db = db20();
    run(&mut db, "CREATE INDEX t_v_idx ON t (v)");
    run(&mut db, "INSERT INTO t VALUES (100, NULL, 0)");
    let img = db.to_image(8192, 1).unwrap();
    let mut loaded = Database::from_image(&img)
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(loaded.to_image(8192, 1).unwrap(), img, "byte-stable reload");
    let t = loaded.table("t").unwrap();
    assert_eq!(t.indexes.len(), 1);
    assert_eq!(t.indexes[0].name, "t_v_idx");
    assert_eq!(t.indexes[0].columns, vec![1]);
    // The reloaded index serves scans at the same pinned cost and observes mutations.
    assert_eq!(cost(&mut loaded, "SELECT id FROM t WHERE v = 3"), 17);
    run(&mut loaded, "UPDATE t SET v = 3 WHERE id = 100");
    assert_eq!(
        ids(&mut loaded, "SELECT id FROM t WHERE v = 3 ORDER BY id"),
        vec![3, 8, 13, 18, 100]
    );
}

/// Index DDL is transactional (transactions.md §4.5): a CREATE INDEX inside a rolled-back
/// block vanishes (definition and store), and one inside a committed block persists.
#[test]
fn index_ddl_is_transactional() {
    let mut db = db20();
    run(&mut db, "BEGIN");
    run(&mut db, "CREATE INDEX t_v_idx ON t (v)");
    assert_eq!(cost(&mut db, "SELECT id FROM t WHERE v = 3"), 17);
    run(&mut db, "ROLLBACK");
    assert!(db.table("t").unwrap().indexes.is_empty());
    assert_eq!(cost(&mut db, "SELECT id FROM t WHERE v = 3"), 45);
    run(&mut db, "BEGIN");
    run(&mut db, "CREATE INDEX t_v_idx ON t (v)");
    run(&mut db, "COMMIT");
    assert_eq!(cost(&mut db, "SELECT id FROM t WHERE v = 3"), 17);
}

/// File-backed: an index survives the incremental commit + demand-paged reopen
/// (format.md "Allocation & incremental commit"; pager.md), keeps the same pinned scan
/// cost (page_read is logical — buffer-pool-invisible), and stays maintainable across
/// commits.
#[test]
fn file_backed_paged_reopen_uses_the_index() {
    let path = tmp("secondary_index_paged.jed");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        page_size: 256,
    })
    .unwrap()
    .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32, w i32)");
    for i in 1..=20 {
        run(
            &mut db,
            &format!("INSERT INTO t VALUES ({i}, {}, {i})", i % 5),
        );
    }
    run(&mut db, "CREATE INDEX t_v_idx ON t (v)");
    let in_memory_cost = cost(&mut db, "SELECT id FROM t WHERE v = 3");
    drop(db);

    let mut reopened = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());
    // The paged open reads the index tree as a skeleton; the logical cost is unchanged.
    assert_eq!(
        cost(&mut reopened, "SELECT id FROM t WHERE v = 3"),
        in_memory_cost
    );
    assert_eq!(
        ids(&mut reopened, "SELECT id FROM t WHERE v = 3 ORDER BY id"),
        vec![3, 8, 13, 18]
    );
    // Mutate + commit incrementally (only dirty index pages are written), then reopen.
    run(&mut reopened, "UPDATE t SET v = 3 WHERE id = 4");
    run(&mut reopened, "DELETE FROM t WHERE id = 13");
    drop(reopened);
    let mut again = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        ids(&mut again, "SELECT id FROM t WHERE v = 3 ORDER BY id"),
        vec![3, 4, 8, 18]
    );
    drop(again);
    let _ = std::fs::remove_file(&path);
}

/// The CREATE INDEX build scan honors the cost ceiling (CLAUDE.md §13): a ceiling below
/// the build cost aborts deterministically with 54P01 and registers nothing.
#[test]
fn create_index_honors_the_cost_ceiling() {
    let mut db = db20();
    db.set_max_cost(10); // the build scan costs 21
    assert_eq!(err_code(&mut db, "CREATE INDEX t_v_idx ON t (v)"), "54P01");
    db.set_max_cost(0);
    assert!(db.table("t").unwrap().indexes.is_empty());
    run(&mut db, "CREATE INDEX t_v_idx ON t (v)");
}
