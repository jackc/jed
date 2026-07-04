//! Integration tests for the jed-migrate crate, driven through the public API against the
//! shared `../testdata/` corpus (design.md §10).

use std::path::PathBuf;

use jed::{CreateOptions, Database, SessionOptions};
use jed_migrate::{
    MigrateError, Migrator, Options, load_migrations, load_migrations_from_entries, new_migration,
    resolve_targets,
};

fn testdata(sub: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../testdata")
        .join(sub)
}

fn mem_db() -> Database {
    Database::create(CreateOptions::default()).expect("in-memory create is infallible")
}

/// Mint a throwaway session to read a scalar count, so tests need no `&mut Database`.
fn count(db: &Database, sql: &str) -> i64 {
    let mut s = db.session(SessionOptions::default());
    s.query_row(sql, (), |r| r.get::<i64>(0))
        .expect("query")
        .expect("one row")
}

// ───────────────────────────── loading ─────────────────────────────

#[test]
fn loads_blog_set() {
    let migrations = load_migrations(&testdata("blog")).unwrap();
    assert_eq!(migrations.len(), 3);
    let names: Vec<&str> = migrations.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        names,
        ["001_create_users", "002_add_posts", "003_add_email_index"]
    );
    for (i, m) in migrations.iter().enumerate() {
        assert_eq!(m.sequence, (i + 1) as u32);
        assert!(!m.is_irreversible(), "{} should be reversible", m.name);
        assert!(m.down.is_some());
    }
    assert!(migrations[0].up.contains("insert into users"));
    assert!(!migrations[0].down.as_ref().unwrap().contains("insert into"));
}

#[test]
fn loads_irreversible_set() {
    let migrations = load_migrations(&testdata("irreversible")).unwrap();
    assert_eq!(migrations.len(), 2);
    assert!(!migrations[0].is_irreversible());
    assert!(migrations[1].is_irreversible());
    assert!(migrations[1].down.is_none());
}

#[test]
fn ignores_non_migration_files() {
    let migrations = load_migrations(&testdata("ignored")).unwrap();
    assert_eq!(migrations.len(), 1, "non-matching files must be ignored");
    assert_eq!(migrations[0].name, "001_only");
}

#[test]
fn refuses_malformed_sets() {
    for sub in ["gap", "duplicate", "missing_one", "empty_up"] {
        let err = load_migrations(&testdata(&format!("malformed/{sub}")))
            .expect_err(&format!("{sub} should fail to load"));
        assert!(matches!(err, MigrateError::Load(_)), "{sub}: {err:?}");
    }
}

#[test]
fn embedded_source_loads_identically() {
    // The embedded loader produces the same result as the directory loader (design.md §7).
    let entries = &[
        (
            "001_create_users.sql",
            include_str!("../../testdata/blog/001_create_users.sql"),
        ),
        (
            "002_add_posts.sql",
            include_str!("../../testdata/blog/002_add_posts.sql"),
        ),
        (
            "003_add_email_index.sql",
            include_str!("../../testdata/blog/003_add_email_index.sql"),
        ),
        ("not_a_migration.txt", "ignored"),
    ];
    let embedded = load_migrations_from_entries(entries).unwrap();
    let from_dir = load_migrations(&testdata("blog")).unwrap();
    assert_eq!(embedded, from_dir);
}

// ───────────────────────────── the migrate walk ─────────────────────────────

#[test]
fn migrate_up_then_down_round_trips() {
    let db = mem_db();
    let migrations = load_migrations(&testdata("blog")).unwrap();
    let mut m = Migrator::new(&db, migrations, Options::default()).unwrap();

    assert_eq!(m.current_version().unwrap(), 0);
    assert_eq!(db.table_names(), vec!["schema_version"]);

    m.migrate().unwrap();
    assert_eq!(m.current_version().unwrap(), 3);
    assert_eq!(db.table_names(), vec!["posts", "schema_version", "users"]);

    m.migrate_to(0).unwrap();
    assert_eq!(m.current_version().unwrap(), 0);
    assert_eq!(db.table_names(), vec!["schema_version"]);

    // Back up again — proves the down halves truly reversed the schema.
    m.migrate().unwrap();
    assert_eq!(db.table_names(), vec!["posts", "schema_version", "users"]);
}

#[test]
fn migrate_stepwise_runs_multi_statement_up() {
    let db = mem_db();
    let migrations = load_migrations(&testdata("blog")).unwrap();
    let mut m = Migrator::new(&db, migrations, Options::default()).unwrap();
    for target in 1..=3 {
        m.migrate_to(target).unwrap();
        assert_eq!(m.current_version().unwrap(), target);
    }
    // 001's multi-statement up half seeded two users.
    assert_eq!(count(&db, "select count(*) from users"), 2);
}

#[test]
fn fast_path_is_a_noop() {
    let db = mem_db();
    let migrations = load_migrations(&testdata("blog")).unwrap();
    let mut m = Migrator::new(&db, migrations, Options::default()).unwrap();
    m.migrate().unwrap();
    m.migrate_to(3).unwrap();
    m.migrate().unwrap();
    assert_eq!(m.current_version().unwrap(), 3);
}

#[test]
fn bad_target_is_rejected() {
    let db = mem_db();
    let migrations = load_migrations(&testdata("blog")).unwrap();
    let mut m = Migrator::new(&db, migrations, Options::default()).unwrap();
    assert!(matches!(
        m.migrate_to(4),
        Err(MigrateError::BadVersion { .. })
    ));
}

#[test]
fn irreversible_down_fails_and_leaves_version_unmoved() {
    let db = mem_db();
    let migrations = load_migrations(&testdata("irreversible")).unwrap();
    let mut m = Migrator::new(&db, migrations, Options::default()).unwrap();
    m.migrate().unwrap();
    assert_eq!(m.current_version().unwrap(), 2);
    assert!(matches!(
        m.migrate_to(0),
        Err(MigrateError::Irreversible { .. })
    ));
    assert_eq!(m.current_version().unwrap(), 2, "version must be unmoved");
}

#[test]
fn migration_error_carries_context() {
    let db = mem_db();
    let migrations = vec![jed_migrate::Migration {
        sequence: 1,
        name: "001_bad".to_string(),
        up: "create table ok (id bigint primary key);\ninsert into nope (id) values (1);"
            .to_string(),
        down: None,
    }];
    let mut m = Migrator::new(&db, migrations, Options::default()).unwrap();
    let err = m.migrate().expect_err("should fail");
    match &err {
        MigrateError::Migration {
            name,
            direction,
            statement,
            ..
        } => {
            assert_eq!(name, "001_bad");
            assert_eq!(*direction, "up");
            assert!(!statement.is_empty());
        }
        other => panic!("expected MigrateError::Migration, got {other:?}"),
    }
    assert_eq!(err.sql_state(), Some("42P01")); // undefined table
    assert_eq!(m.current_version().unwrap(), 0, "rolled back");
    assert!(!db.table_names().iter().any(|n| n == "ok"));
}

#[test]
fn in_script_transaction_control_is_rejected() {
    let db = mem_db();
    let migrations = load_migrations(&testdata("tx_control")).unwrap();
    let mut m = Migrator::new(&db, migrations, Options::default()).unwrap();
    let err = m.migrate().expect_err("should fail");
    assert_eq!(err.sql_state(), Some("0A000"));
    assert_eq!(m.current_version().unwrap(), 0, "rolled back");
}

#[test]
fn status_reports_progress() {
    let db = mem_db();
    let migrations = load_migrations(&testdata("blog")).unwrap();
    let mut m = Migrator::new(&db, migrations, Options::default()).unwrap();
    let s = m.status().unwrap();
    assert_eq!((s.current, s.target, s.pending), (0, 3, 3));
    m.migrate_to(2).unwrap();
    let s = m.status().unwrap();
    assert_eq!((s.current, s.target, s.pending), (2, 3, 1));
}

#[test]
fn custom_version_table() {
    let db = mem_db();
    let migrations = load_migrations(&testdata("blog")).unwrap();
    let mut m = Migrator::new(
        &db,
        migrations,
        Options {
            version_table: Some("migration_state".to_string()),
        },
    )
    .unwrap();
    m.migrate_to(1).unwrap();
    let names = db.table_names();
    assert!(names.iter().any(|n| n == "migration_state"));
    assert!(!names.iter().any(|n| n == "schema_version"));
}

#[test]
fn invalid_version_table_name_is_rejected() {
    let db = mem_db();
    let migrations = load_migrations(&testdata("blog")).unwrap();
    assert!(
        Migrator::new(
            &db,
            migrations,
            Options {
                version_table: Some("bad name; drop table x".to_string()),
            },
        )
        .is_err()
    );
}

// ───────────────────────────── target grammar ─────────────────────────────

#[test]
fn resolves_target_grammar() {
    const N: u32 = 5;
    assert_eq!(resolve_targets("", 0, N).unwrap(), vec![5]);
    assert_eq!(resolve_targets("last", 2, N).unwrap(), vec![5]);
    assert_eq!(resolve_targets("3", 0, N).unwrap(), vec![3]);
    assert_eq!(resolve_targets("0", 5, N).unwrap(), vec![0]);
    assert_eq!(resolve_targets("+2", 1, N).unwrap(), vec![3]);
    assert_eq!(resolve_targets("-2", 5, N).unwrap(), vec![3]);
    assert_eq!(resolve_targets("-+1", 5, N).unwrap(), vec![4, 5]);
    assert_eq!(resolve_targets("-+3", 5, N).unwrap(), vec![2, 5]);
    for bad in ["6", "-1", "+9", "-+9", "banana", "+"] {
        assert!(
            resolve_targets(bad, if bad == "-1" { 0 } else { 5 }, N).is_err(),
            "{bad}"
        );
    }
}

// ───────────────────────────── scaffolding ─────────────────────────────

#[test]
fn new_migration_scaffolds_next_sequence() {
    let dir = std::env::temp_dir().join(format!("jed_migrate_new_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let p1 = new_migration(&dir, "create_users").unwrap();
    assert_eq!(p1.file_name().unwrap(), "001_create_users.sql");
    let body = std::fs::read_to_string(&p1).unwrap();
    assert!(body.contains(jed_migrate::SEPARATOR));

    let p2 = new_migration(&dir, "add_posts").unwrap();
    assert_eq!(p2.file_name().unwrap(), "002_add_posts.sql");

    // The comment-only stubs have empty up halves, so loading them refuses the set.
    assert!(load_migrations(&dir).is_err());

    let _ = std::fs::remove_dir_all(&dir);
}
