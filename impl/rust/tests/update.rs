//! Step 6: UPDATE — value replacement, old-row assignment semantics, the two-phase
//! all-or-nothing guarantee, the rejected cases (duplicate target, overflow, not-null),
//! and PRIMARY KEY re-keying (§11 step 6). The PG-divergent re-keying cases (an
//! end-state-valid key swap / cascade that PG rejects on the per-row transient) live here
//! rather than the oracle corpus, the same divergence UNIQUE carries (indexes.md §8).

use jed::{Engine, Value, execute};

fn db_with(stmts: &[&str]) -> Engine {
    let mut db = Engine::new();
    for s in stmts {
        execute(&mut db, s).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

/// The (id, a, b) i32/i16 rows in storage-key order, as i64s, for end-state assertions.
fn ids_abs(db: &Engine) -> Vec<(i64, i64, i64)> {
    db.rows_in_key_order("t")
        .unwrap()
        .iter()
        .map(|r| match (&r[0], &r[1], &r[2]) {
            (Value::Int(id), Value::Int(a), Value::Int(b)) => (*id, *a, *b),
            other => panic!("expected ints, got {other:?}"),
        })
        .collect()
}

fn setup() -> Engine {
    db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, a i16, b i16)",
        "INSERT INTO t VALUES (1, 10, 11)",
        "INSERT INTO t VALUES (2, 20, 22)",
        "INSERT INTO t VALUES (3, 30, 33)",
    ])
}

#[test]
fn update_missing_table_traps() {
    let mut db = Engine::new();
    assert_eq!(
        execute(&mut db, "UPDATE nope SET a = 1")
            .unwrap_err()
            .code(),
        "42P01"
    );
}

#[test]
fn update_unknown_column_traps() {
    let mut db = setup();
    assert_eq!(
        execute(&mut db, "UPDATE t SET nope = 1")
            .unwrap_err()
            .code(),
        "42703"
    );
}

/// Re-keying validates against the statement's END STATE (like UNIQUE, indexes.md §8): a
/// swap of two primary keys keeps both keys present, so jed accepts it — where PostgreSQL's
/// per-row check fails on the transient collision. Each row's non-key columns move with it.
#[test]
fn update_pk_swap_is_end_state_valid() {
    let mut db = setup();
    execute(&mut db, "UPDATE t SET id = 3 - id WHERE id <= 2").unwrap();
    // (1,10,11)→(2,10,11) and (2,20,22)→(1,20,22); id 3 untouched.
    assert_eq!(ids_abs(&db), vec![(1, 20, 22), (2, 10, 11), (3, 30, 33)]);
}

/// A cascade that shifts every key up by one is likewise end-state-valid (the new keys
/// {2,3,4} are all distinct and only id 4 is new), so jed re-keys all three rows — where
/// PostgreSQL rejects the per-row transient (id 1 → 2 while 2 still exists).
#[test]
fn update_pk_increment_cascade_succeeds() {
    let mut db = setup();
    execute(&mut db, "UPDATE t SET id = id + 1").unwrap();
    assert_eq!(ids_abs(&db), vec![(2, 10, 11), (3, 20, 22), (4, 30, 33)]);
}

/// Re-keying onto a DISTINCT existing (non-updated) row's key collides — 23505, all-or-nothing.
#[test]
fn update_pk_collision_with_existing_traps() {
    let mut db = setup();
    assert_eq!(
        execute(&mut db, "UPDATE t SET id = 3 WHERE id = 1")
            .unwrap_err()
            .code(),
        "23505"
    );
    assert_eq!(ids_abs(&db), vec![(1, 10, 11), (2, 20, 22), (3, 30, 33)]);
}
