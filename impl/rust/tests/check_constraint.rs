//! CHECK constraints — `[CONSTRAINT name] CHECK ( expr )` in both positions
//! (spec/design/constraints.md §4, grammar.md §29). Covers what the corpus suite
//! (`ddl/check.test`) cannot: catalog introspection (names, evaluation order, persisted
//! expression text), the on-disk round-trip (v4 catalog check list), a corrupted stored
//! expression (XX001), and the metered evaluation cost. Mirrored in
//! impl/go/check_constraint_test.go and impl/ts/tests/check_constraint.test.ts.

use jed::value::Value;
use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

fn db_with(sql: &[&str]) -> Session {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for s in sql {
        db.query_outcome(s, &[])
            .unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn err(db: &mut Session, sql: &str) -> (String, String) {
    let e = db
        .query_outcome(sql, &[])
        .expect_err(&format!("expected an error from {sql:?}"));
    (e.code().to_string(), e.message)
}

fn check_names(db: &Session, table: &str) -> Vec<String> {
    db.table(table)
        .unwrap()
        .checks
        .iter()
        .map(|c| c.name.clone())
        .collect()
}

/// PG's auto-naming, oracle-probed: exactly one distinct referenced column →
/// `<table>_<col>_check`, else `<table>_check`; the smallest free numeric suffix on a
/// collision; names assigned in textual definition order, then the catalog holds them in
/// evaluation (name) order.
#[test]
fn auto_naming_matches_postgres() {
    let db = db_with(&[
        "CREATE TABLE t (a int CHECK (a > 0), b int, CHECK (b > a), CHECK (1 < 2), \
         CHECK (b < 100))",
        // Two same-column checks on one column, then a table-level one on it.
        "CREATE TABLE t2 (a int CHECK (a > 0) CHECK (a < 10), CHECK (a = 5))",
        // A table-level check FIRST gets the unsuffixed name (textual order, not
        // column-constraints-first).
        "CREATE TABLE t3 (CHECK (a > 0), a int CHECK (a < 5))",
        // An explicit name occupying a would-be auto name: the auto skips to the next free.
        "CREATE TABLE t9 (a int CONSTRAINT t9_a_check CHECK (a > 0) CHECK (a < 5))",
    ]);
    // `CHECK (b > a)` references two columns → t_check; `CHECK (1 < 2)` references none →
    // collides → t_check1; `CHECK (b < 100)` → t_b_check.
    assert_eq!(
        check_names(&db, "t"),
        vec!["t_a_check", "t_b_check", "t_check", "t_check1"]
    );
    assert_eq!(
        check_names(&db, "t2"),
        vec!["t2_a_check", "t2_a_check1", "t2_a_check2"]
    );
    assert_eq!(check_names(&db, "t3"), vec!["t3_a_check", "t3_a_check1"]);
    assert_eq!(check_names(&db, "t9"), vec!["t9_a_check", "t9_a_check1"]);
    // The persisted expression text is the re-rendered token sequence.
    let t = db.table("t").unwrap();
    let texts: Vec<&str> = t.checks.iter().map(|c| c.expr_text.as_str()).collect();
    assert_eq!(texts, vec!["a > 0", "b < 100", "b > a", "1 < 2"]);
}

/// The DDL-time rejections, codes and check order all oracle-probed against PostgreSQL.
#[test]
fn ddl_errors_match_postgres() {
    let mut db = db_with(&[]);
    // Non-boolean expression.
    assert_eq!(
        err(&mut db, "CREATE TABLE x (a int CHECK (a + 1))").0,
        "42804"
    );
    // Subqueries — scalar, EXISTS, IN — are rejected structurally, before any resolution
    // (the inner table need not exist).
    for sql in [
        "CREATE TABLE x (a int CHECK (a > (SELECT v FROM nowhere)))",
        "CREATE TABLE x (a int CHECK (EXISTS (SELECT v FROM nowhere)))",
        "CREATE TABLE x (a int CHECK (a IN (SELECT v FROM nowhere)))",
    ] {
        let (code, msg) = err(&mut db, sql);
        assert_eq!(code, "0A000", "{sql}");
        assert_eq!(msg, "cannot use subquery in check constraint");
    }
    // Aggregates.
    let (code, msg) = err(&mut db, "CREATE TABLE x (a int CHECK (sum(a) > 0))");
    assert_eq!(code, "42803");
    assert_eq!(
        msg,
        "aggregate functions are not allowed in check constraints"
    );
    // Bind parameters.
    let (code, msg) = err(&mut db, "CREATE TABLE x (a int CHECK (a > $1))");
    assert_eq!(code, "42P02");
    assert_eq!(msg, "there is no parameter $1");
    // Unknown column / unknown qualifier resolve through the ordinary resolver.
    assert_eq!(
        err(&mut db, "CREATE TABLE x (a int CHECK (nope > 0))").0,
        "42703"
    );
    assert_eq!(
        err(&mut db, "CREATE TABLE x (a int CHECK (other.a > 0))").0,
        "42P01"
    );
    // A forward reference is fine (checks resolve after all columns are known).
    db.query_outcome("CREATE TABLE fwd (CHECK (b > 0), b int)", &[])
        .unwrap();
    // A qualified reference to this table is fine.
    db.query_outcome("CREATE TABLE q (a int CHECK (q.a > 0))", &[])
        .unwrap();
    // Duplicate explicit name.
    let (code, msg) = err(
        &mut db,
        "CREATE TABLE x (a int CONSTRAINT cc CHECK (a > 0) CONSTRAINT cc CHECK (a < 5))",
    );
    assert_eq!(code, "42710");
    assert_eq!(msg, "constraint cc for relation x already exists");
    // An explicit name colliding with an EARLIER auto name (derived names never yield).
    assert_eq!(
        err(
            &mut db,
            "CREATE TABLE tb (a int CHECK (a > 0), CONSTRAINT tb_a_check CHECK (a < 5))"
        )
        .0,
        "42710"
    );
    // PRIMARY KEY constraints resolve before any check expression (PG's order).
    let (code, msg) = err(
        &mut db,
        "CREATE TABLE tc (a int CHECK (nope > 0), PRIMARY KEY (alsonope))",
    );
    assert_eq!(code, "42703");
    assert!(msg.contains("named in key"), "{msg}");
    // ALL validation precedes ALL naming: a 42703 in a later check beats a 42710 between
    // earlier ones.
    assert_eq!(
        err(
            &mut db,
            "CREATE TABLE td (a int CONSTRAINT cc CHECK (a > 0), CONSTRAINT cc CHECK (nope > 0))"
        )
        .0,
        "42703"
    );
    // The DEFAULT is NOT checked against CHECK at CREATE TABLE.
    db.query_outcome("CREATE TABLE t7 (a int DEFAULT -5 CHECK (a > 0))", &[])
        .unwrap();
    // `CHECK ()` is a syntax error; so is a CHECK with no parenthesized expression.
    assert_eq!(err(&mut db, "CREATE TABLE x (a int, CHECK ())").0, "42601");
    // Columns may be NAMED check / constraint (the keywords stay non-reserved).
    db.query_outcome("CREATE TABLE odd (check int, constraint i16)", &[])
        .unwrap();
    db.query_outcome("INSERT INTO odd VALUES (1, 2)", &[])
        .unwrap();
}

/// Enforcement: FALSE traps 23514 with PG's message; TRUE and NULL pass; checks evaluate
/// in NAME order (not definition order); NOT NULL fires before CHECK; CHECK fires before
/// the duplicate-key check.
#[test]
fn violations_match_postgres_order() {
    let mut db = db_with(&[
        "CREATE TABLE t (a int CHECK (a > 0), b int, CHECK (b > a))",
        // zz is defined first but aa evaluates first (name order, oracle-probed).
        "CREATE TABLE t5 (a int, CONSTRAINT zz CHECK (a > 0), CONSTRAINT aa CHECK (a > 5))",
        "CREATE TABLE tn (a int NOT NULL CHECK (a > 0))",
        "CREATE TABLE tu (k int PRIMARY KEY, v int CHECK (v > 0))",
    ]);
    let (code, msg) = err(&mut db, "INSERT INTO t VALUES (-1, 5)");
    assert_eq!(code, "23514");
    assert_eq!(
        msg,
        "new row for relation t violates check constraint t_a_check"
    );
    // Violating both: the first in name order reports.
    assert_eq!(
        err(&mut db, "INSERT INTO t VALUES (-1, -5)").1,
        "new row for relation t violates check constraint t_a_check"
    );
    assert_eq!(
        err(&mut db, "INSERT INTO t VALUES (5, 1)").1,
        "new row for relation t violates check constraint t_check"
    );
    let (_, msg) = err(&mut db, "INSERT INTO t5 VALUES (-1)");
    assert!(msg.ends_with("violates check constraint aa"), "{msg}");
    // NULL passes a check (UNKNOWN is not FALSE).
    db.query_outcome("INSERT INTO t VALUES (NULL, NULL)", &[])
        .unwrap();
    // NOT NULL fires before CHECK on the same row.
    assert_eq!(err(&mut db, "INSERT INTO tn VALUES (NULL)").0, "23502");
    // CHECK fires before the duplicate-key check.
    db.query_outcome("INSERT INTO tu VALUES (1, 5)", &[])
        .unwrap();
    assert_eq!(err(&mut db, "INSERT INTO tu VALUES (1, -1)").0, "23514");
    // A runtime error inside a check propagates as itself.
    db.query_outcome("CREATE TABLE dz (a int CHECK (10 / a > 0))", &[])
        .unwrap();
    assert_eq!(err(&mut db, "INSERT INTO dz VALUES (0)").0, "22012");
}

/// The two-phase / all-or-nothing pass covers checks: a violating row anywhere in the
/// batch (INSERT multi-row, INSERT ... SELECT, UPDATE) leaves the table untouched, and a
/// defaulted value goes through the same per-row evaluation.
#[test]
fn two_phase_and_defaults() {
    let mut db = db_with(&[
        "CREATE TABLE t (a int CHECK (a > 0))",
        "CREATE TABLE src (v int)",
        "INSERT INTO src VALUES (3), (-3)",
        "CREATE TABLE t7 (a int DEFAULT -5 CHECK (a > 0), b int)",
    ]);
    // Multi-row INSERT: the second row violates → nothing stored.
    assert_eq!(err(&mut db, "INSERT INTO t VALUES (1), (-1)").0, "23514");
    match db.query_outcome("SELECT count(*) FROM t", &[]).unwrap() {
        Outcome::Query { rows, .. } => assert_eq!(rows[0][0], Value::Int(0)),
        other => panic!("expected a query outcome, got {other:?}"),
    }
    // INSERT ... SELECT flows through the same per-row checks.
    assert_eq!(err(&mut db, "INSERT INTO t SELECT v FROM src").0, "23514");
    // UPDATE: a later row violates → no row changes.
    db.query_outcome("INSERT INTO t VALUES (1), (2)", &[])
        .unwrap();
    assert_eq!(err(&mut db, "UPDATE t SET a = a - 1").0, "23514");
    match db.query_outcome("SELECT a FROM t ORDER BY a", &[]).unwrap() {
        Outcome::Query { rows, .. } => {
            assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
        }
        other => panic!("expected a query outcome, got {other:?}"),
    }
    // An UPDATE that passes every check applies.
    db.query_outcome("UPDATE t SET a = a + 10", &[]).unwrap();
    // The stored default is evaluated per row like any value: a DEFAULT keyword slot
    // applying a check-violating default traps 23514 at INSERT, not CREATE.
    assert_eq!(
        err(&mut db, "INSERT INTO t7 VALUES (DEFAULT, 1)").0,
        "23514"
    );
    assert_eq!(err(&mut db, "INSERT INTO t7 (b) VALUES (1)").0, "23514");
    db.query_outcome("INSERT INTO t7 VALUES (2, 1)", &[])
        .unwrap();
}

/// The full expression surface works inside a check: CASE, BETWEEN, IN, LIKE, IS NULL,
/// scalar functions, casts, booleans, decimals, text.
#[test]
fn expression_surface() {
    let mut db = db_with(&[
        "CREATE TABLE e (n int, flag boolean, note text, price numeric(8,2), \
         CHECK (CASE WHEN n IS NULL THEN TRUE ELSE n BETWEEN 0 AND 100 END), \
         CHECK (flag), \
         CHECK (note LIKE 'ok%' OR note IN ('a', 'b')), \
         CHECK (abs(n) <= CAST(100 AS int)), \
         CONSTRAINT price_pos CHECK (price >= 0.50))",
    ]);
    db.query_outcome(
        "INSERT INTO e VALUES (50, TRUE, 'ok then', 1.00), (NULL, TRUE, 'a', 0.50)",
        &[],
    )
    .unwrap();
    assert_eq!(
        err(&mut db, "INSERT INTO e VALUES (101, TRUE, 'a', 1.00)").0,
        "23514"
    );
    assert_eq!(
        err(&mut db, "INSERT INTO e VALUES (1, FALSE, 'a', 1.00)").0,
        "23514"
    );
    assert_eq!(
        err(&mut db, "INSERT INTO e VALUES (1, TRUE, 'c', 1.00)").0,
        "23514"
    );
    let (_, msg) = err(&mut db, "INSERT INTO e VALUES (1, TRUE, 'a', 0.49)");
    assert!(
        msg.ends_with("violates check constraint price_pos"),
        "{msg}"
    );
}

/// Check evaluation is metered expression work: each interior node charges operator_eval
/// per candidate row (constraints.md §4.4) — the documented exception to "VALUES inserts
/// cost zero".
#[test]
fn check_evaluation_is_metered() {
    let mut db = db_with(&["CREATE TABLE c (a int CHECK (a > 0))"]);
    // One interior node (>) × one row.
    match db.query_outcome("INSERT INTO c VALUES (1)", &[]).unwrap() {
        Outcome::Statement { cost, .. } => assert_eq!(cost, 1),
        other => panic!("expected a statement outcome, got {other:?}"),
    }
    // Two rows × one node.
    match db
        .query_outcome("INSERT INTO c VALUES (2), (3)", &[])
        .unwrap()
    {
        Outcome::Statement { cost, .. } => assert_eq!(cost, 2),
        other => panic!("expected a statement outcome, got {other:?}"),
    }
    // UPDATE: full scan of 3 rows. Baseline without the check would be page_read(1) +
    // 3×storage_row_read + 3×(a + 1 eval) = 7; the check adds one more eval per updated
    // row → 10.
    match db.query_outcome("UPDATE c SET a = a + 1", &[]).unwrap() {
        Outcome::Statement { cost, .. } => assert_eq!(cost, 10),
        other => panic!("expected a statement outcome, got {other:?}"),
    }
    // The ceiling aborts mid-validation deterministically.
    db.set_max_cost(2);
    let e = db
        .query_outcome("INSERT INTO c VALUES (4), (5), (6)", &[])
        .unwrap_err();
    assert_eq!(e.code(), "54P01");
    db.set_max_cost(0);
    match db.query_outcome("SELECT count(*) FROM c", &[]).unwrap() {
        Outcome::Query { rows, .. } => assert_eq!(rows[0][0], Value::Int(3)),
        other => panic!("expected a query outcome, got {other:?}"),
    }
}

/// Round-trip: the v4 catalog persists (name, expression text) in evaluation order; a
/// reloaded table enforces its checks identically, and a corrupted stored expression is
/// XX001 at open.
#[test]
fn round_trips_through_the_on_disk_image() {
    let db = db_with(&[
        "CREATE TABLE t (a int PRIMARY KEY, b int CHECK (b > 0), price numeric(8,2), \
         CONSTRAINT price_range CHECK (price >= 0.50 AND price <= 9999.99), note text, \
         CHECK (note = 'ok' OR note = 'a''b'))",
        "INSERT INTO t VALUES (1, 5, 1.00, 'ok'), (2, NULL, 9999.99, 'a''b'), \
         (3, 100, 0.50, 'ok')",
    ]);
    let image = db.to_image(256, 1).unwrap();
    let mut loaded = Database::from_image(&image)
        .unwrap()
        .session(SessionOptions::default());

    let t = loaded.table("t").unwrap();
    let stored: Vec<(&str, &str)> = t
        .checks
        .iter()
        .map(|c| (c.name.as_str(), c.expr_text.as_str()))
        .collect();
    assert_eq!(
        stored,
        vec![
            ("price_range", "price >= 0.50 AND price <= 9999.99"),
            ("t_b_check", "b > 0"),
            ("t_note_check", "note = 'ok' OR note = 'a''b'"),
        ]
    );
    // Still enforced, with the same message.
    let (code, msg) = err(&mut loaded, "INSERT INTO t VALUES (4, -1, 1.00, 'ok')");
    assert_eq!(code, "23514");
    assert_eq!(
        msg,
        "new row for relation t violates check constraint t_b_check"
    );
    assert_eq!(
        err(&mut loaded, "INSERT INTO t VALUES (4, 1, 0.10, 'ok')").0,
        "23514"
    );
    assert_eq!(
        err(&mut loaded, "INSERT INTO t VALUES (4, 1, 1.00, 'nope')").0,
        "23514"
    );
    loaded
        .query_outcome("INSERT INTO t VALUES (4, 1, 1.00, 'a''b')", &[])
        .unwrap();
    // A second generation (load → image → load) is byte-stable: the text is written back
    // verbatim.
    let image2 = loaded.to_image(256, 1).unwrap();
    let reloaded = Database::from_image(&image2)
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        check_names(&reloaded, "t"),
        vec!["price_range", "t_b_check", "t_note_check"]
    );

    // A stored expression that no longer parses is XX001 (the file lied): patch the text
    // `b > 0` to the same-length garbage `b > (`.
    let needle = b"b > 0";
    let at = image
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("stored check text in image");
    let mut corrupt = image.clone();
    corrupt[at + 4] = b'(';
    let e = match Database::from_image(&corrupt) {
        Err(e) => e,
        Ok(_) => panic!("corrupt check text must fail to load"),
    };
    assert_eq!(e.code(), "XX001");
}
