//! The RETURNING clause (spec/design/grammar.md §32, cost.md §3) — covers what the corpus
//! suite (`dml/returning.test`) cannot: the Outcome variant split (Statement vs Query),
//! output column names, pinned costs (the projection charge, the touched-set growth, the
//! fold-once/correlated split), the ceiling's all-or-nothing abort, `$N` binding, and
//! transactional behavior. Mirrored in impl/go/returning_test.go and
//! impl/ts/tests/returning.test.ts.

use jed::value::Value;
use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

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

fn rows(db: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match run(db, sql) {
        Outcome::Query { rows, .. } => rows,
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

fn names(db: &mut Session, sql: &str) -> Vec<String> {
    match run(db, sql) {
        Outcome::Query { column_names, .. } => column_names,
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

fn err_code(db: &mut Session, sql: &str) -> String {
    db.query_outcome(sql, &[])
        .expect_err(&format!("expected an error from {sql:?}"))
        .code()
        .to_string()
}

fn setup() -> Session {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for s in [
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32 DEFAULT 7, w i32)",
        "INSERT INTO t VALUES (1, 10, 100), (2, 20, 200), (3, 30, 300)",
    ] {
        db.query_outcome(s, &[])
            .unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn int(n: i64) -> Value {
    Value::Int(n)
}

#[test]
fn insert_values_returning_rows_and_variant() {
    let mut db = setup();
    // Without RETURNING an INSERT stays a bare statement outcome.
    assert!(matches!(
        run(&mut db, "INSERT INTO t VALUES (10, 1, 2)"),
        Outcome::Statement { .. }
    ));
    // With it, the stored rows project back — including multi-row and the `*` glob with
    // the DEFAULT fill-in (v = 7) and the omitted column (w = NULL).
    assert_eq!(
        rows(&mut db, "INSERT INTO t VALUES (11, 5, 6) RETURNING id, v"),
        vec![vec![int(11), int(5)]]
    );
    assert_eq!(
        rows(&mut db, "INSERT INTO t (id) VALUES (12), (13) RETURNING *"),
        vec![
            vec![int(12), int(7), Value::Null],
            vec![int(13), int(7), Value::Null],
        ]
    );
}

#[test]
fn returning_output_names_and_expressions() {
    let mut db = setup();
    // §8 naming: ?column? for an expression, the AS label, the canonical name for a
    // bare/qualified column. Expressions evaluate against the stored row.
    assert_eq!(
        names(
            &mut db,
            "INSERT INTO t VALUES (14, 5, 0) RETURNING v + 1, v * 2 AS dbl, t.w, id"
        ),
        vec!["?column?", "dbl", "w", "id"]
    );
    assert_eq!(
        rows(
            &mut db,
            "DELETE FROM t WHERE id = 14 RETURNING v * 10, abs(w - 1)"
        ),
        vec![vec![int(50), int(1)]]
    );
}

#[test]
fn insert_select_returning() {
    let mut db = setup();
    run(&mut db, "CREATE TABLE src (a i32)");
    run(&mut db, "INSERT INTO src VALUES (40), (41)");
    // RETURNING belongs to the INSERT: it projects the INSERTED rows (defaults filled).
    assert_eq!(
        rows(
            &mut db,
            "INSERT INTO t (id) SELECT a FROM src RETURNING id, v"
        ),
        vec![vec![int(40), int(7)], vec![int(41), int(7)]]
    );
    // The word `returning` is never an IMPLICIT source alias (the §15 stop set) — but an
    // explicit `AS returning` alias still parses, and the clause follows it.
    assert_eq!(
        rows(
            &mut db,
            "INSERT INTO t (id) SELECT a + 100 FROM src AS returning RETURNING id"
        ),
        vec![vec![int(140)], vec![int(141)]]
    );
}

#[test]
fn update_returning_new_values() {
    let mut db = setup();
    assert_eq!(
        rows(
            &mut db,
            "UPDATE t SET v = v + 1 WHERE id <= 2 RETURNING id, v"
        ),
        vec![vec![int(1), int(11)], vec![int(2), int(21)]]
    );
    // Zero matched rows: still a QUERY outcome — empty rows, names intact.
    match run(&mut db, "UPDATE t SET v = 0 WHERE id = 999 RETURNING id") {
        Outcome::Query {
            column_names, rows, ..
        } => {
            assert_eq!(column_names, vec!["id"]);
            assert!(rows.is_empty());
        }
        Outcome::Statement { .. } => panic!("zero-row RETURNING must still be a query result"),
    }
}

#[test]
fn delete_returning_old_values() {
    let mut db = setup();
    assert_eq!(
        rows(&mut db, "DELETE FROM t WHERE w = 200 RETURNING id, v, w"),
        vec![vec![int(2), int(20), int(200)]]
    );
    assert_eq!(
        rows(&mut db, "SELECT id FROM t ORDER BY id"),
        vec![vec![int(1)], vec![int(3)]]
    );
}

#[test]
fn returning_error_codes() {
    let mut db = setup();
    // Resolution precedes execution: the unknown column beats the would-be PK duplicate.
    assert_eq!(
        err_code(&mut db, "INSERT INTO t VALUES (1, 0, 0) RETURNING nosuch"),
        "42703"
    );
    // Aggregates are forbidden in RETURNING (PG 42803).
    assert_eq!(
        err_code(&mut db, "INSERT INTO t VALUES (90, 0, 0) RETURNING sum(v)"),
        "42803"
    );
    assert_eq!(
        err_code(&mut db, "UPDATE t SET v = 1 RETURNING count(*)"),
        "42803"
    );
    // An unknown qualifier is 42P01.
    assert_eq!(
        err_code(&mut db, "INSERT INTO t VALUES (91, 0, 0) RETURNING other.v"),
        "42P01"
    );
    // old/new are RETURNING-only (grammar.md §32): elsewhere they are ordinary unknown
    // qualifiers (42P01, as in PG); an unknown column under them is 42703.
    assert_eq!(
        err_code(&mut db, "UPDATE t SET v = old.v + 1 WHERE id = 1"),
        "42P01"
    );
    assert_eq!(err_code(&mut db, "DELETE FROM t WHERE new.v = 1"), "42P01");
    assert_eq!(
        err_code(&mut db, "UPDATE t SET v = 1 RETURNING old.nosuch"),
        "42703"
    );
    // An empty item list, and any trailing clause after RETURNING, are 42601.
    assert_eq!(err_code(&mut db, "DELETE FROM t RETURNING"), "42601");
    assert_eq!(
        err_code(
            &mut db,
            "DELETE FROM t WHERE id = 1 RETURNING id ORDER BY id"
        ),
        "42601"
    );
    // `returning` is no longer an implicit alias ANYWHERE (the §15 stop set): in a plain
    // SELECT it is now trailing junk, as in PostgreSQL (which reserves the word).
    assert_eq!(err_code(&mut db, "SELECT v FROM t returning"), "42601");
    // Nothing above wrote anything.
    assert_eq!(rows(&mut db, "SELECT count(*) FROM t"), vec![vec![int(3)]]);
}

#[test]
fn returning_subqueries_pre_statement_snapshot() {
    let mut db = setup();
    // Uncorrelated subqueries fold once and read the PRE-statement snapshot (probed
    // against PG 18): the count excludes the two rows being inserted...
    assert_eq!(
        rows(
            &mut db,
            "INSERT INTO t VALUES (50, 0, 0), (51, 0, 0) RETURNING id, (SELECT count(*) FROM t)"
        ),
        vec![vec![int(50), int(3)], vec![int(51), int(3)]]
    );
    // ... an UPDATE's subquery sees pre-update values (sum over old v: 10+20+30) ...
    assert_eq!(
        rows(
            &mut db,
            "UPDATE t SET v = 0 WHERE id = 1 RETURNING (SELECT sum(v) FROM t WHERE w IS NOT NULL)"
        ),
        vec![vec![int(60)]]
    );
    // ... and a DELETE's sees the row still present (5 rows live at this point).
    assert_eq!(
        rows(
            &mut db,
            "DELETE FROM t WHERE id = 1 RETURNING (SELECT count(*) FROM t WHERE w IS NOT NULL)"
        ),
        vec![vec![int(5)]]
    );
    // A correlated subquery's outer reference reads the row being RETURNED (here the
    // deleted row: its neighbor id+1 = 3 has w = 300).
    assert_eq!(
        rows(
            &mut db,
            "DELETE FROM t WHERE id = 2 RETURNING (SELECT s.w FROM t AS s WHERE s.id = t.id + 1)"
        ),
        vec![vec![int(300)]]
    );
}

#[test]
fn returning_costs() {
    let mut db = setup();
    // A plain VALUES insert still costs zero; RETURNING adds row_produced per stored row
    // plus the items' metered evaluation (bare columns are leaves).
    assert_eq!(cost(&mut db, "INSERT INTO t VALUES (60, 1, 1)"), 0);
    assert_eq!(
        cost(&mut db, "INSERT INTO t VALUES (61, 1, 1) RETURNING id, v"),
        1
    );
    assert_eq!(
        cost(
            &mut db,
            "INSERT INTO t VALUES (62, 1, 1), (63, 2, 2) RETURNING v + 1"
        ),
        4 // 2 x (row_produced + one operator_eval)
    );
    // UPDATE/DELETE under a PK point bound: page_read(1) + storage_row_read(1) +
    // the residual filter eval(1), plus the projection (row_produced 1, leaves 0).
    assert_eq!(cost(&mut db, "UPDATE t SET v = 9 WHERE id = 1"), 3);
    assert_eq!(
        cost(&mut db, "UPDATE t SET v = 8 WHERE id = 1 RETURNING v"),
        4
    );
    assert_eq!(cost(&mut db, "DELETE FROM t WHERE id = 60 RETURNING v"), 4);
}

#[test]
fn returning_subquery_costs() {
    // Fresh 3-row table: an uncorrelated RETURNING subquery folds ONCE.
    // Inner `SELECT max(v) FROM t`: page_read 1 + 3 row reads + 3 accumulates +
    // 1 row_produced = 8. Two returned rows add 2 x row_produced (the folded constant is
    // a leaf): total 10.
    let mut db = setup();
    assert_eq!(
        cost(
            &mut db,
            "INSERT INTO t VALUES (64, 1, 1), (65, 1, 2) RETURNING (SELECT max(v) FROM t)"
        ),
        10
    );
    // A correlated one re-runs per RETURNED row: outer DELETE bound = page 1 + row 1 +
    // filter 1 + row_produced 1; the subquery node charges operator_eval 1 + the inner
    // bounded count (page 1 + row 1 + filter 1 + accumulate 1 + row_produced 1 = 5).
    let mut db = setup();
    assert_eq!(
        cost(
            &mut db,
            "DELETE FROM t WHERE id = 1 RETURNING (SELECT count(*) FROM t AS s WHERE s.id = t.id)"
        ),
        10
    );
}

#[test]
fn returning_ceiling_abort_is_all_or_nothing() {
    let mut db = setup();
    // The two-row insert with RETURNING costs 4 (pinned above). A ceiling of 2 aborts
    // during the projection pass — BEFORE phase 2 — so nothing is inserted.
    db.set_max_cost(2);
    assert_eq!(
        err_code(
            &mut db,
            "INSERT INTO t VALUES (70, 1, 1), (71, 2, 2) RETURNING v + 1"
        ),
        "54P01"
    );
    db.set_max_cost(0);
    assert_eq!(rows(&mut db, "SELECT count(*) FROM t"), vec![vec![int(3)]]);
}

#[test]
fn returning_bind_params() {
    let mut db = setup();
    // A $N in the RETURNING list types from context like anywhere else (api.md §5).
    let out = db
        .query_outcome(
            "INSERT INTO t VALUES (80, 3, 0) RETURNING v + $1",
            &[Value::Int(5)],
        )
        .unwrap();
    match out {
        Outcome::Query { rows, .. } => assert_eq!(rows, vec![vec![int(8)]]),
        Outcome::Statement { .. } => panic!("expected a query result"),
    }
    // A parameter no context types is 42P18.
    let err = db
        .query_outcome(
            "INSERT INTO t VALUES (81, 3, 0) RETURNING $1",
            &[Value::Int(5)],
        )
        .expect_err("an untypable parameter must fail");
    assert_eq!(err.code().to_string(), "42P18");
}

#[test]
fn returning_grows_the_touched_set() {
    // A compressed large value charges value_decompress only when RETURNING reads it
    // (the §32 touched-set rule). 100_000 raw bytes at page_size 8192 (C = 8180):
    // ceil(100000/8180) = 13 slabs.
    let big = format!("INSERT INTO big VALUES (1, 0, '{}')", "x".repeat(100_000));
    let fresh = || {
        let mut db = Database::create(CreateOptions::default())
            .unwrap()
            .session(SessionOptions::default());
        run(
            &mut db,
            "CREATE TABLE big (id i32 PRIMARY KEY, w i32, t text)",
        );
        run(&mut db, &big);
        db
    };
    // RETURNING only fixed-width columns: no decompression (page 1 + row 1 + filter 1 +
    // row_produced 1).
    let mut db = fresh();
    assert_eq!(
        cost(&mut db, "DELETE FROM big WHERE id = 1 RETURNING id, w"),
        4
    );
    // RETURNING the compressed column adds its 13 slabs.
    let mut db = fresh();
    assert_eq!(
        cost(&mut db, "DELETE FROM big WHERE id = 1 RETURNING t"),
        17
    );
    // UPDATE: an ASSIGNED column's returned value is the freshly computed one — not a
    // storage read, so no decompression (and the shrunken row re-stores inline-plain:
    // no compression attempt either).
    let mut db = fresh();
    assert_eq!(
        cost(
            &mut db,
            "UPDATE big SET t = 'short' WHERE id = 1 RETURNING t"
        ),
        4
    );
    // RETURNING an UNASSIGNED compressed column is a logical read: the rewrite's own
    // 13 value_compress attempts (the over-RECORD_MAX row re-stores) + the projection's
    // 13 value_decompress + row_produced, over the 3-unit bounded scan.
    let mut db = fresh();
    assert_eq!(cost(&mut db, "UPDATE big SET w = 1 WHERE id = 1"), 16);
    let mut db = fresh();
    assert_eq!(
        cost(&mut db, "UPDATE big SET w = 1 WHERE id = 1 RETURNING t"),
        30
    );
}

#[test]
fn returning_in_transactions() {
    let mut db = setup();
    run(&mut db, "BEGIN");
    assert_eq!(
        rows(&mut db, "INSERT INTO t VALUES (95, 1, 1) RETURNING id"),
        vec![vec![int(95)]]
    );
    run(&mut db, "ROLLBACK");
    assert_eq!(rows(&mut db, "SELECT count(*) FROM t"), vec![vec![int(3)]]);
    // A write statement stays a write statement: 25006 in a READ ONLY block.
    run(&mut db, "BEGIN READ ONLY");
    assert_eq!(
        err_code(&mut db, "DELETE FROM t WHERE id = 1 RETURNING id"),
        "25006"
    );
    run(&mut db, "ROLLBACK");
}

#[test]
fn old_new_qualifiers_per_statement() {
    let mut db = setup();
    // INSERT: old is the all-NULL row (the key included); new = bare = the stored row.
    assert_eq!(
        rows(
            &mut db,
            "INSERT INTO t VALUES (40, 4, 44) RETURNING old.v, new.v, v, old.id"
        ),
        vec![vec![Value::Null, int(4), int(4), Value::Null]]
    );
    // UPDATE: old = pre-assignment, new = bare = post; expressions span both versions;
    // case-insensitive like any identifier.
    assert_eq!(
        rows(
            &mut db,
            "UPDATE t SET v = v + 5 WHERE id = 1 RETURNING OLD.v, New.v, v, new.v - old.v"
        ),
        vec![vec![int(10), int(15), int(15), int(5)]]
    );
    // An unassigned column's two versions agree.
    assert_eq!(
        rows(
            &mut db,
            "UPDATE t SET v = 0 WHERE id = 2 RETURNING old.w, new.w"
        ),
        vec![vec![int(200), int(200)]]
    );
    // DELETE: old = bare = the deleted row; new is the all-NULL row.
    assert_eq!(
        rows(
            &mut db,
            "DELETE FROM t WHERE id = 3 RETURNING old.v, new.v, v"
        ),
        vec![vec![int(30), Value::Null, int(30)]]
    );
    // INSERT ... SELECT takes the same mapping.
    run(&mut db, "CREATE TABLE src2 (a i32)");
    run(&mut db, "INSERT INTO src2 VALUES (60)");
    assert_eq!(
        rows(
            &mut db,
            "INSERT INTO t (id) SELECT a FROM src2 RETURNING old.v, new.v"
        ),
        vec![vec![Value::Null, int(7)]]
    );
}

#[test]
fn old_new_naming_and_star() {
    let mut db = setup();
    // §8: the qualifier never leaks into the output name (old.v is named v, like PG).
    assert_eq!(
        names(
            &mut db,
            "UPDATE t SET v = 1 WHERE id = 1 RETURNING old.v, new.w"
        ),
        vec!["v", "w"]
    );
    // The pseudo-relations are qualifier-only: `*` still expands exactly the table's columns.
    assert_eq!(
        names(&mut db, "INSERT INTO t (id) VALUES (41) RETURNING *"),
        vec!["id", "v", "w"]
    );
}

#[test]
fn old_new_shadowed_by_table_name() {
    // A target table literally named old (or new) keeps the ordinary table-qualified
    // meaning — the row-version pseudo-relation is suppressed (PG-probed).
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    run(&mut db, "CREATE TABLE old (x i32)");
    assert_eq!(
        rows(&mut db, "INSERT INTO old VALUES (1) RETURNING old.x"),
        vec![vec![int(1)]] // the inserted value, NOT the NULL old side
    );
    assert_eq!(
        rows(&mut db, "UPDATE old SET x = x + 1 RETURNING old.x"),
        vec![vec![int(2)]] // bare semantics = the NEW value
    );
    // The other qualifier still works alongside the shadowed one.
    assert_eq!(
        rows(&mut db, "UPDATE old SET x = x + 1 RETURNING new.x"),
        vec![vec![int(3)]]
    );
    assert_eq!(
        rows(&mut db, "DELETE FROM old RETURNING old.x"),
        vec![vec![int(3)]] // bare semantics = the deleted value
    );
    run(&mut db, "CREATE TABLE new (x i32)");
    assert_eq!(
        rows(&mut db, "INSERT INTO new VALUES (9) RETURNING new.x"),
        vec![vec![int(9)]]
    );
    assert_eq!(
        rows(&mut db, "DELETE FROM new RETURNING new.x"),
        vec![vec![int(9)]] // table wins: the deleted value, NOT the NULL new side
    );
}

#[test]
fn old_new_in_subqueries() {
    let mut db = setup();
    run(&mut db, "CREATE TABLE s2 (a i32, b i32)");
    run(&mut db, "INSERT INTO s2 VALUES (1, 500)");
    // old/new resolve inside item subqueries like any outer reference (probed; jed has no
    // FROM-less SELECT, so the single-row s2 anchors the scalar subqueries).
    assert_eq!(
        rows(
            &mut db,
            "UPDATE t SET v = v * 2 WHERE id = 2 RETURNING (SELECT old.v + 0 FROM s2), (SELECT old.v + s2.b FROM s2)"
        ),
        vec![vec![int(20), int(520)]]
    );
    assert_eq!(
        rows(
            &mut db,
            "DELETE FROM t WHERE id = 1 RETURNING (SELECT new.v FROM s2), (SELECT count(*) FROM s2 WHERE s2.a = old.id)"
        ),
        vec![vec![Value::Null, int(1)]]
    );
}

#[test]
fn old_new_touched_set() {
    // The touched-set sides (cost.md §3): `old.col` is ALWAYS a storage read — even when
    // the column is assigned; a DELETE's `new.col` is the constant NULL row and reads
    // nothing. Compressed 100k text at page_size 8192 = 13 slabs.
    let big = format!("INSERT INTO big VALUES (1, 0, '{}')", "x".repeat(100_000));
    let fresh = || {
        let mut db = Database::create(CreateOptions::default())
            .unwrap()
            .session(SessionOptions::default());
        run(
            &mut db,
            "CREATE TABLE big (id i32 PRIMARY KEY, w i32, t text)",
        );
        run(&mut db, &big);
        db
    };
    // RETURNING the ASSIGNED column's old version forces the decompress the new version
    // avoided (4 there — see returning_grows_the_touched_set): 3-unit bounded scan +
    // 13 value_decompress + row_produced (the shrunken rewrite attempts no compression).
    let mut db = fresh();
    assert_eq!(
        cost(
            &mut db,
            "UPDATE big SET t = 'short' WHERE id = 1 RETURNING old.t"
        ),
        17
    );
    // An unassigned column's old side costs the same as its new side (both storage reads):
    // 3 + 13 decompress + 13 rewrite-compress + 1 row_produced.
    let mut db = fresh();
    assert_eq!(
        cost(&mut db, "UPDATE big SET w = 1 WHERE id = 1 RETURNING old.t"),
        30
    );
    // DELETE RETURNING new.t reads nothing (NULL side): the 4-unit shape, value NULL.
    let mut db = fresh();
    match run(&mut db, "DELETE FROM big WHERE id = 1 RETURNING new.t") {
        Outcome::Query { rows, cost, .. } => {
            assert_eq!(rows, vec![vec![Value::Null]]);
            assert_eq!(cost, 4);
        }
        Outcome::Statement { .. } => panic!("expected a query result"),
    }
}
