//! INSERT ... ON CONFLICT (UPSERT) — the pieces the oracle corpus
//! (`spec/conformance/suites/dml/insert_on_conflict.test`) cannot express: the jed-specific
//! divergences from PostgreSQL (spec/design/upsert.md §9) and the affected-row count (the
//! command-tag count a `statement ok` does not assert). The PG-agreeing behavior — DO NOTHING /
//! DO UPDATE, arbiter inference / ON CONSTRAINT, the 21000 second-affect rule, non-arbiter 23505 —
//! is the corpus's job. Mirrored in impl/go/on_conflict_test.go and
//! impl/ts/tests/on_conflict.test.ts.

use jed::{Database, Outcome, execute};

fn db_with(sql: &[&str]) -> Database {
    let mut db = Database::new();
    for s in sql {
        execute(&mut db, s).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn err(db: &mut Database, sql: &str) -> String {
    execute(db, sql)
        .expect_err(&format!("expected an error from {sql:?}"))
        .code()
        .to_string()
}

fn affected(db: &mut Database, sql: &str) -> Option<i64> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Statement { rows_affected, .. } => rows_affected,
        Outcome::Query { .. } => panic!("expected a statement outcome from {sql:?}"),
    }
}

/// DIVERGENCE (upsert.md §9): assigning a PRIMARY KEY column in DO UPDATE is still `0A000` — a
/// deferred follow-on. The standalone UPDATE re-keying has landed (§11 step 6), but extending it
/// to the upsert conflict path is separate. PostgreSQL allows it (probed: `SET id =
/// excluded.id + 100` re-keys the row).
#[test]
fn do_update_pk_column_assignment_is_unsupported() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
        "INSERT INTO t VALUES (1, 10)",
    ]);
    assert_eq!(
        err(
            &mut db,
            "INSERT INTO t VALUES (1, 5) ON CONFLICT (id) DO UPDATE SET id = excluded.id + 100"
        ),
        "0A000"
    );
}

/// DIVERGENCE: `DO UPDATE SET col = DEFAULT` is not supported — the UPDATE `SET = DEFAULT` follow-on
/// is deferred too, so the assignment RHS is a general expression. `DEFAULT` is not reserved (§3),
/// so a bare `DEFAULT` there resolves as a column reference → `42703` (no column named `default`).
/// PostgreSQL supports `SET col = DEFAULT` (probed: it resets the column to its default).
#[test]
fn do_update_set_default_is_unsupported() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32 DEFAULT 7)",
        "INSERT INTO t VALUES (1, 10)",
    ]);
    assert_eq!(
        err(
            &mut db,
            "INSERT INTO t VALUES (1, 5) ON CONFLICT (id) DO UPDATE SET v = DEFAULT"
        ),
        "42703"
    );
}

/// DIVERGENCE: a GENERATED ALWAYS identity column can only be set to DEFAULT (jed has no
/// `SET = DEFAULT`), so any DO UPDATE assignment to one is `428C9` — the standing UPDATE rule.
#[test]
fn do_update_generated_always_is_rejected() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 GENERATED ALWAYS AS IDENTITY, k i32 PRIMARY KEY, v i32)",
        "INSERT INTO t (k, v) VALUES (1, 10)",
    ]);
    assert_eq!(
        err(
            &mut db,
            "INSERT INTO t (k, v) VALUES (1, 5) ON CONFLICT (k) DO UPDATE SET id = 99"
        ),
        "428C9"
    );
}

/// The affected-row count (api.md §4) the corpus's `statement ok` cannot assert: an ON CONFLICT
/// counts the inserted + updated rows; rows skipped by DO NOTHING (or a DO UPDATE WHERE that is
/// false) are not counted.
#[test]
fn affected_row_counts() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
        "INSERT INTO t VALUES (1, 10), (2, 20)",
    ]);
    // DO NOTHING over a batch: id 1 conflicts (skip), id 3 inserts → 1 affected.
    assert_eq!(
        affected(
            &mut db,
            "INSERT INTO t VALUES (1, 99), (3, 30) ON CONFLICT DO NOTHING"
        ),
        Some(1)
    );
    // DO UPDATE: id 2 updates, id 4 inserts → 2 affected.
    assert_eq!(
        affected(
            &mut db,
            "INSERT INTO t VALUES (2, 22), (4, 40) ON CONFLICT (id) DO UPDATE SET v = excluded.v"
        ),
        Some(2)
    );
    // All conflict and are skipped → 0 affected.
    assert_eq!(
        affected(
            &mut db,
            "INSERT INTO t VALUES (1, 0), (2, 0) ON CONFLICT DO NOTHING"
        ),
        Some(0)
    );
    // A DO UPDATE WHERE that is false updates nothing → 0 affected (the row stays unchanged).
    assert_eq!(
        affected(
            &mut db,
            "INSERT INTO t VALUES (1, 7) ON CONFLICT (id) DO UPDATE SET v = excluded.v WHERE false"
        ),
        Some(0)
    );
}
