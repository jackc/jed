//! UNIQUE constraints + unique indexes (spec/design/constraints.md §5, indexes.md §8) —
//! covers what the corpus suite (`ddl/unique.test`) cannot: catalog introspection (the
//! unique flag, fold results, name order), the v6 on-disk round-trip, transactional DDL,
//! and the documented PG divergences (end-state UPDATE validation, droppable
//! constraint-backed indexes). Mirrored in impl/go/unique_test.go and
//! impl/ts/tests/unique.test.ts.

use jed::value::Value;
use jed::{Database, Outcome, execute};

fn run(db: &mut Database, sql: &str) -> Outcome {
    execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message))
}

fn cost(db: &mut Database, sql: &str) -> i64 {
    match run(db, sql) {
        Outcome::Statement { cost, .. } => cost,
        Outcome::Query { cost, .. } => cost,
    }
}

fn ids(db: &mut Database, sql: &str) -> Vec<i64> {
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

fn err_code(db: &mut Database, sql: &str) -> String {
    execute(db, sql)
        .expect_err(&format!("expected an error from {sql:?}"))
        .code()
        .to_string()
}

fn err_msg(db: &mut Database, sql: &str) -> String {
    execute(db, sql)
        .expect_err(&format!("expected an error from {sql:?}"))
        .message
}

fn index_names(db: &Database, table: &str) -> Vec<(String, bool)> {
    db.table(table)
        .unwrap()
        .indexes
        .iter()
        .map(|i| (i.name.clone(), i.unique))
        .collect()
}

/// Constraint naming matches PostgreSQL (oracle-probed, constraints.md §5.3): the
/// lowercased `<table>_<cols>_key` base with the smallest free suffix, walked past BOTH
/// the relation namespace and the table's check names; an explicit CONSTRAINT name is the
/// index name as written.
#[test]
fn constraint_naming_matches_postgres() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE other (x i32)");
    run(&mut db, "CREATE INDEX walk_a_key ON other (x)"); // occupies the derived base
    run(
        &mut db,
        "CREATE TABLE Walk (a i32 UNIQUE, b i32, CONSTRAINT Named UNIQUE (b, a), CONSTRAINT walk_b_check CHECK (b > 0), UNIQUE (b))",
    );
    // walk_a_key taken by a relation -> walk_a_key1; walk_b_key derived past the CHECK
    // name walk_b_check (different name, no walk needed); Named kept as written.
    assert_eq!(
        index_names(&db, "walk"),
        vec![
            ("Named".to_string(), true),
            ("walk_a_key1".to_string(), true),
            ("walk_b_key".to_string(), true),
        ]
    );
    // A derived name walks past a CHECK name too (PG-probed: w1_a_key -> w1_a_key1).
    run(
        &mut db,
        "CREATE TABLE w1 (a i32, CONSTRAINT w1_a_key CHECK (a > 0), UNIQUE (a))",
    );
    assert_eq!(
        index_names(&db, "w1"),
        vec![("w1_a_key1".to_string(), true)]
    );
}

/// The dedup/fold rules match PostgreSQL (oracle-probed, constraints.md §5.2): identical
/// member lists fold into one (the first explicitly-named one's name wins); a list
/// identical to the primary key's folds away entirely; a differing ORDER is distinct.
#[test]
fn dedup_and_pk_fold_match_postgres() {
    let mut db = Database::new();
    // Repeated bare UNIQUE + the table-level twin all fold into one auto-named index.
    run(&mut db, "CREATE TABLE e3 (a i32 UNIQUE UNIQUE, UNIQUE (a))");
    assert_eq!(index_names(&db, "e3"), vec![("e3_a_key".to_string(), true)]);
    // An unnamed-then-named pair keeps the NAME (PG: p1 kept "named").
    run(
        &mut db,
        "CREATE TABLE p1 (a i32 UNIQUE, CONSTRAINT named UNIQUE (a))",
    );
    assert_eq!(index_names(&db, "p1"), vec![("named".to_string(), true)]);
    // Two named duplicates keep the FIRST (PG: e7 kept "x").
    run(
        &mut db,
        "CREATE TABLE e7 (a i32, CONSTRAINT x UNIQUE (a), CONSTRAINT y UNIQUE (a))",
    );
    assert_eq!(index_names(&db, "e7"), vec![("x".to_string(), true)]);
    // The PK absorbs an identical list — regardless of declaration order or form.
    run(&mut db, "CREATE TABLE e5 (a i32 PRIMARY KEY UNIQUE)");
    assert_eq!(index_names(&db, "e5"), vec![]);
    run(&mut db, "CREATE TABLE p2 (a i32 UNIQUE, PRIMARY KEY (a))");
    assert_eq!(index_names(&db, "p2"), vec![]);
    run(
        &mut db,
        "CREATE TABLE e9 (a i32, b i32, PRIMARY KEY (a, b), UNIQUE (a, b))",
    );
    assert_eq!(index_names(&db, "e9"), vec![]);
    // A differing member ORDER is a distinct constraint (PG: p3 kept both).
    run(
        &mut db,
        "CREATE TABLE p3 (a i32, b i32, PRIMARY KEY (a, b), UNIQUE (b, a))",
    );
    assert_eq!(
        index_names(&db, "p3"),
        vec![("p3_b_a_key".to_string(), true)]
    );
}

/// DDL errors match PostgreSQL (oracle-probed, constraints.md §5.1/§5.3): member
/// resolution 42703/42701/0A000 (before any CHECK validates), explicit-name collisions
/// 42P07 (relation namespace, including the table being created) before 42710 (the
/// table's constraint names).
#[test]
fn ddl_errors_match_postgres() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE other (x i32)");
    // Member resolution, PG's wording-bearing codes.
    assert_eq!(
        err_code(&mut db, "CREATE TABLE e2 (a i32, UNIQUE (nosuch))"),
        "42703"
    );
    assert_eq!(
        err_code(&mut db, "CREATE TABLE e1 (a i32, UNIQUE (a, a))"),
        "42701"
    );
    // f64 IS now a valid UNIQUE member (the float-order-preserving key, encoding.md §2.8 — every
    // scalar is keyable; text/bytea covered in ddl/unique.test).
    run(&mut db, "CREATE TABLE e6 (a i32, s f64 UNIQUE)");
    // UNIQUE members resolve BEFORE any CHECK validates (PG: z1/z2), in either order.
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE z1 (a i32, CHECK (nosuch1 > 0), UNIQUE (nosuch2))"
        ),
        "42703"
    );
    let msg = err_msg(
        &mut db,
        "CREATE TABLE z2 (a i32, UNIQUE (nosuch2), CHECK (nosuch1 > 0))",
    );
    assert!(msg.contains("nosuch2"), "unique member first: {msg}");
    // An explicit constraint name collides in the RELATION namespace: an existing table,
    // the table being created (PG: p4), and a same-statement sibling (PG: e8).
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE c2 (a i32, CONSTRAINT other UNIQUE (a))"
        ),
        "42P07"
    );
    assert_eq!(
        err_code(&mut db, "CREATE TABLE p4 (a i32, CONSTRAINT p4 UNIQUE (a))"),
        "42P07"
    );
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE e8 (a i32, CONSTRAINT x UNIQUE (a), b i32, CONSTRAINT x UNIQUE (b))"
        ),
        "42P07"
    );
    // ... and with a CHECK constraint's name it is 42710, in either declaration order
    // (PG: z4/z5 — both report when the unique constraint is created).
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE z4 (a i32, CONSTRAINT zc CHECK (a > 0), CONSTRAINT zc UNIQUE (a))"
        ),
        "42710"
    );
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE z5 (a i32, CONSTRAINT zc UNIQUE (a), CONSTRAINT zc CHECK (a > 0))"
        ),
        "42710"
    );
    // CREATE UNIQUE <not-index> is a syntax error.
    assert_eq!(err_code(&mut db, "CREATE UNIQUE TABLE t (a i32)"), "42601");
}

/// INSERT enforcement (indexes.md §8): a duplicate against the store or within the batch
/// traps 23505 naming the index; NULLS DISTINCT exempts any tuple with a NULL component;
/// the violation precedence is CHECK before PK before UNIQUE, and among unique indexes
/// the catalog (name) order.
#[test]
fn insert_enforcement() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32 UNIQUE, w i32, CONSTRAINT wv UNIQUE (w, v), CHECK (id < 100))",
    );
    run(&mut db, "INSERT INTO t VALUES (1, 10, 100), (2, NULL, 100)");
    // A stored duplicate; the message names the violated index.
    let msg = err_msg(&mut db, "INSERT INTO t VALUES (3, 10, 200)");
    assert!(msg.contains("t_v_key"), "names the index: {msg}");
    assert_eq!(
        err_code(&mut db, "INSERT INTO t VALUES (3, 10, 200)"),
        "23505"
    );
    // An in-batch duplicate (two-phase: nothing stored).
    assert_eq!(
        err_code(&mut db, "INSERT INTO t VALUES (3, 30, 1), (4, 30, 2)"),
        "23505"
    );
    assert_eq!(ids(&mut db, "SELECT id FROM t ORDER BY id"), vec![1, 2]);
    // NULLS DISTINCT: any number of NULLs coexist, and a NULL component exempts the
    // multi-column tuple — (100, NULL) twice is fine even though w matches.
    run(&mut db, "INSERT INTO t VALUES (5, NULL, 100)");
    run(
        &mut db,
        "INSERT INTO t VALUES (6, NULL, 300), (7, NULL, 300)",
    );
    // A fully non-NULL composite duplicate traps, naming the composite index (its own
    // table — beside t_v_key the component dup would always be reported first).
    run(
        &mut db,
        "CREATE TABLE c (id i32 PRIMARY KEY, w i32, v i32, CONSTRAINT wv2 UNIQUE (w, v))",
    );
    run(&mut db, "INSERT INTO c VALUES (1, 40, 400)");
    let msg = err_msg(&mut db, "INSERT INTO c VALUES (2, 40, 400)");
    assert!(msg.contains("wv2"), "names the composite index: {msg}");
    // A distinct pair sharing one component is a different tuple — allowed.
    run(&mut db, "INSERT INTO c VALUES (2, 40, 401)");
    run(&mut db, "INSERT INTO t VALUES (8, 40, 400)");
    // INSERT ... SELECT takes the same path.
    assert_eq!(
        err_code(
            &mut db,
            "INSERT INTO t SELECT id + 20, v, w FROM t WHERE id = 8"
        ),
        "23505"
    );
    // Precedence: the PK's 23505 wins over UNIQUE (PG-probed), naming <table>_pkey.
    let msg = err_msg(&mut db, "INSERT INTO t VALUES (1, 10, 999)");
    assert!(msg.contains("t_pkey"), "PK reported first: {msg}");
    // ... and CHECK (23514) fires before either (PG-probed).
    assert_eq!(
        err_code(&mut db, "INSERT INTO t VALUES (101, 10, 999)"),
        "23514"
    );
    // Two violated unique indexes report in catalog (name) order: t_v_key < wv.
    let msg = err_msg(&mut db, "INSERT INTO t VALUES (10, 40, 400)");
    assert!(msg.contains("t_v_key"), "name order: {msg}");
}

/// UPDATE validates uniqueness against the statement's END STATE (indexes.md §8 — the
/// documented PG divergence): self-resolving rewrites succeed; genuine conflicts with
/// untouched rows and in-batch collisions trap 23505; nothing is written on failure.
#[test]
fn update_enforcement_end_state() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE m (id i32 PRIMARY KEY, v i32 UNIQUE)");
    run(&mut db, "INSERT INTO m VALUES (1, 1), (2, 2), (3, 30)");
    // PG fails both of these on the transient per-row collision; jed's end state is unique.
    run(&mut db, "UPDATE m SET v = v + 1 WHERE id < 3"); // 1,2 -> 2,3
    assert_eq!(ids(&mut db, "SELECT v FROM m ORDER BY id"), vec![2, 3, 30]);
    run(&mut db, "UPDATE m SET v = 5 - v WHERE id < 3"); // swap: 2,3 -> 3,2
    assert_eq!(ids(&mut db, "SELECT v FROM m ORDER BY id"), vec![3, 2, 30]);
    // A no-op rewrite of the same value is fine (its own old entry never conflicts).
    run(&mut db, "UPDATE m SET v = v WHERE id = 1");
    // A genuine conflict with an untouched row.
    assert_eq!(
        err_code(&mut db, "UPDATE m SET v = 30 WHERE id = 1"),
        "23505"
    );
    // An in-batch collision: two rewritten rows landing on one value.
    assert_eq!(
        err_code(&mut db, "UPDATE m SET v = 7 WHERE id < 3"),
        "23505"
    );
    // All-or-nothing: the failed statements wrote nothing.
    assert_eq!(ids(&mut db, "SELECT v FROM m ORDER BY id"), vec![3, 2, 30]);
    // NULL is exempt on UPDATE too: several rows may go NULL at once.
    run(&mut db, "UPDATE m SET v = NULL WHERE id < 3");
    assert_eq!(
        ids(&mut db, "SELECT id FROM m WHERE v IS NULL ORDER BY id"),
        vec![1, 2]
    );
}

/// CREATE UNIQUE INDEX verifies existing rows before registering (indexes.md §2/§8): a
/// duplicate traps 23505 and creates nothing (the name stays free); NULLs are exempt;
/// thereafter it enforces like a constraint-backed index. The auto-name keeps `_idx`.
#[test]
fn create_unique_index_build() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE d (id i32 PRIMARY KEY, a i32, n i32)");
    run(
        &mut db,
        "INSERT INTO d VALUES (1, 7, NULL), (2, 7, NULL), (3, 8, 5)",
    );
    // Build over duplicates fails and registers nothing.
    let msg = err_msg(&mut db, "CREATE UNIQUE INDEX dup ON d (a)");
    assert!(msg.contains("dup"), "names the failed index: {msg}");
    assert_eq!(index_names(&db, "d"), vec![]);
    // The name is free again (nothing was created).
    run(&mut db, "CREATE TABLE dup (x i32)");
    run(&mut db, "DROP TABLE dup");
    // NULLs are exempt at build time (two NULLs in n).
    run(&mut db, "CREATE UNIQUE INDEX ON d (n)"); // d_n_idx — the _idx auto-name
    assert_eq!(index_names(&db, "d"), vec![("d_n_idx".to_string(), true)]);
    // ... and it enforces thereafter.
    assert_eq!(err_code(&mut db, "INSERT INTO d VALUES (4, 9, 5)"), "23505");
    run(&mut db, "INSERT INTO d VALUES (4, 9, NULL)");
}

/// DROP INDEX of a constraint-backed unique index is allowed and drops the constraint
/// (the documented PG divergence — indexes.md §7: jed has no ALTER TABLE, so the index
/// name is the constraint's only handle).
#[test]
fn drop_index_drops_the_constraint() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32 UNIQUE)");
    run(&mut db, "INSERT INTO t VALUES (1, 10)");
    assert_eq!(err_code(&mut db, "INSERT INTO t VALUES (2, 10)"), "23505");
    run(&mut db, "DROP INDEX t_v_key");
    run(&mut db, "INSERT INTO t VALUES (2, 10)"); // no longer enforced
    assert_eq!(index_names(&db, "t"), vec![]);
}

/// Uniqueness validation is unmetered (cost.md §3): an INSERT into a uniquely-indexed
/// table still costs 0, and a CREATE UNIQUE INDEX build charges exactly the plain build's
/// scan. The planner treats a unique index like any other (the bounded-scan cost).
#[test]
fn costs_are_unchanged_by_unique() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32, w i32)");
    for i in 1..=20 {
        run(
            &mut db,
            &format!("INSERT INTO t VALUES ({i}, {}, {i})", i % 5),
        );
    }
    // The unique build charges the same page_read(1) + 20 rows = 21 as a plain build.
    assert_eq!(cost(&mut db, "CREATE UNIQUE INDEX t_w_idx ON t (w)"), 21);
    // INSERT ... VALUES stays zero-cost — the probe is unmetered.
    assert_eq!(cost(&mut db, "INSERT INTO t VALUES (21, 9, 21)"), 0);
    // The unique index bounds a scan exactly like a plain one: 1 index node + 1 point
    // lookup + 1 row + 1 filter eval + 1 produced = 5.
    assert_eq!(cost(&mut db, "SELECT id FROM t WHERE w = 7"), 5);
}

/// The v6 round-trip: the unique flag survives serialize -> load (and the reloaded
/// database still enforces), and the image is byte-stable across a second serialize.
#[test]
fn round_trip_preserves_unique() {
    let mut db = Database::new();
    run(
        &mut db,
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32 UNIQUE, w i32)",
    );
    run(&mut db, "CREATE INDEX plain ON t (w)");
    run(&mut db, "INSERT INTO t VALUES (1, 10, 100), (2, NULL, 100)");
    let image = db.to_image(8192, 1).unwrap();
    let mut loaded = Database::from_image(&image).unwrap();
    assert_eq!(
        index_names(&loaded, "t"),
        vec![("plain".to_string(), false), ("t_v_key".to_string(), true)]
    );
    assert_eq!(
        err_code(&mut loaded, "INSERT INTO t VALUES (3, 10, 1)"),
        "23505"
    );
    run(&mut loaded, "INSERT INTO t VALUES (3, NULL, 1)");
    assert_eq!(image, db.to_image(8192, 1).unwrap(), "byte-stable");
}

/// Transactional DDL: a UNIQUE created inside a rolled-back block leaves no trace — no
/// definition, no store, no enforcement (the §3 snapshot model).
#[test]
fn transactional_ddl_rolls_back() {
    let mut db = Database::new();
    run(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
    run(&mut db, "INSERT INTO t VALUES (1, 10)");
    run(&mut db, "BEGIN");
    run(&mut db, "CREATE UNIQUE INDEX u ON t (v)");
    assert_eq!(err_code(&mut db, "INSERT INTO t VALUES (2, 10)"), "23505");
    run(&mut db, "ROLLBACK");
    assert_eq!(index_names(&db, "t"), vec![]);
    run(&mut db, "INSERT INTO t VALUES (2, 10)"); // not enforced after rollback
}
