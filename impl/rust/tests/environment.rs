//! The environment (spec/design/environment.md): the host-API surface that gives each database its
//! outside inputs — loaded Unicode and time-zone data, host functions, and the random and clock
//! sources. Out of the corpus's reach (it is how a host supplies inputs, not SQL), so it is tested per
//! core (CLAUDE.md §10): isolation between environments in one process, derivation with
//! `to_builder`, and a database outliving the caller's reference to its environment. Mirrors the
//! Go/TS environment tests.

use std::path::Path;

use jed::value::Value;
use jed::{CreateOptions, Database, Environment, HostFunction, ScalarType, SessionOptions};

fn unicode_bytes() -> Vec<u8> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spec/collation/fixtures/unicode.jucd");
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn tz_bytes() -> &'static [u8] {
    include_bytes!("../../../spec/tz/fixtures/tzdata.jtz")
}

/// `answer() -> i64`, a zero-argument host function returning `n`.
fn answer(n: i64) -> HostFunction {
    HostFunction::new(
        "answer",
        vec![],
        ScalarType::Int64,
        Box::new(move |_: &[Value]| -> jed::Result<Value> { Ok(Value::Int(n)) }),
    )
}

fn db_in(env: &Environment) -> Database {
    Database::create(CreateOptions {
        environment: env.clone(),
        ..Default::default()
    })
    .unwrap()
}

/// The single value `sql` returns, rendered.
fn scalar(db: &Database, sql: &str) -> jed::Result<String> {
    let mut s = db.session(SessionOptions::default());
    let mut rows = s.query(sql, &[])?;
    let first = rows.next();
    rows.error()?;
    Ok(first.expect("one row")[0].render())
}

#[test]
fn environments_in_one_process_are_isolated() {
    // Two environments side by side: one with the Unicode + tz bundles, a host function, and a clock
    // at the epoch; one empty but for a different clock. Each database sees only its own inputs.
    let loaded = Environment::builder()
        .load_unicode_data(&unicode_bytes())
        .unwrap()
        .load_time_zone_data(tz_bytes())
        .unwrap()
        .register_function(answer(42))
        .unwrap()
        .clock_source(jed::manual_clock(0))
        .build();
    let bare = Environment::builder()
        .clock_source(jed::manual_clock(86_400_000_000))
        .build();
    let a = db_in(&loaded);
    let b = db_in(&bare);

    assert_eq!(
        scalar(&a, "SELECT 'b' < 'B' COLLATE \"unicode\"").unwrap(),
        "true"
    );
    assert_eq!(
        scalar(&b, "SELECT 'b' < 'B' COLLATE \"unicode\"")
            .unwrap_err()
            .code(),
        "42704"
    );

    let at = "SELECT timestamptz '2024-01-15 12:00:00+00' AT TIME ZONE 'America/New_York'";
    assert_eq!(scalar(&a, at).unwrap(), "2024-01-15 07:00:00");
    assert_eq!(scalar(&b, at).unwrap_err().code(), "22023");

    assert_eq!(scalar(&a, "SELECT answer()").unwrap(), "42");
    assert_eq!(scalar(&b, "SELECT answer()").unwrap_err().code(), "42883");

    assert_eq!(
        scalar(&a, "SELECT now()").unwrap(),
        "1970-01-01 00:00:00+00"
    );
    assert_eq!(
        scalar(&b, "SELECT now()").unwrap(),
        "1970-01-02 00:00:00+00"
    );

    // Introspection reads each handle's own environment.
    assert_eq!(a.environment().loaded_collations().len(), 2);
    assert!(b.environment().loaded_collations().is_empty());
    assert!(!a.environment().loaded_time_zones().is_empty());
    assert!(b.environment().loaded_time_zones().is_empty());
}

#[test]
fn a_file_resolves_its_collations_against_the_open_environment() {
    // A file referencing a collation opens only in an environment that provides it; the same file
    // opens fine in another environment that does — nothing is process-wide.
    let unicode = Environment::builder()
        .load_unicode_data(&unicode_bytes())
        .unwrap()
        .build();
    let mut db = db_in(&unicode);
    db.execute(
        "CREATE TABLE t (id i32 PRIMARY KEY, name text COLLATE \"unicode\")",
        &[],
    )
    .unwrap();
    let image = db.to_image(4096, 1).unwrap();
    match Database::from_image(&image, Environment::default()) {
        Err(e) => assert_eq!(e.code(), "XX002"),
        Ok(_) => panic!("opened a file referencing a collation the environment lacks"),
    }
    let reopened = Database::from_image(
        &image,
        Environment::builder()
            .load_unicode_data(&unicode_bytes())
            .unwrap()
            .build(),
    )
    .unwrap();
    assert_eq!(reopened.environment().loaded_collations().len(), 2);
}

#[test]
fn to_builder_shares_contents_and_changes_only_what_it_is_told() {
    let clock = jed::manual_clock(0);
    let base = Environment::builder()
        .load_unicode_data(&unicode_bytes())
        .unwrap()
        .register_function(answer(1))
        .unwrap()
        .clock_source(clock.clone())
        .build();

    // Changing only the clock: everything else is the same loaded data, shared by reference.
    let later = base
        .to_builder()
        .clock_source(jed::manual_clock(86_400_000_000))
        .build();
    assert!(!Environment::ptr_eq(&base, &later));
    assert!(std::sync::Arc::ptr_eq(
        &base.collation("unicode").unwrap(),
        &later.collation("unicode").unwrap()
    ));
    let (a, b) = (db_in(&base), db_in(&later));
    assert_eq!(
        scalar(&a, "SELECT now()").unwrap(),
        "1970-01-01 00:00:00+00"
    );
    assert_eq!(
        scalar(&b, "SELECT now()").unwrap(),
        "1970-01-02 00:00:00+00"
    );
    assert_eq!(scalar(&b, "SELECT answer()").unwrap(), "1");
    assert_eq!(
        scalar(&b, "SELECT 'b' < 'B' COLLATE \"unicode\"").unwrap(),
        "true"
    );

    // Adding a bundle and a function to a derived builder leaves the original unaffected.
    let more = base
        .to_builder()
        .load_time_zone_data(tz_bytes())
        .unwrap()
        .register_function(HostFunction::new(
            "answer2",
            vec![],
            ScalarType::Int64,
            Box::new(|_: &[Value]| -> jed::Result<Value> { Ok(Value::Int(2)) }),
        ))
        .unwrap()
        .build();
    assert!(!more.loaded_time_zones().is_empty());
    assert!(base.loaded_time_zones().is_empty());
    let c = db_in(&more);
    assert_eq!(scalar(&c, "SELECT answer2()").unwrap(), "2");
    assert_eq!(scalar(&a, "SELECT answer2()").unwrap_err().code(), "42883");
    // A derived builder still rejects a conflicting registration (42723).
    assert_eq!(
        base.to_builder()
            .register_function(answer(3))
            .unwrap_err()
            .code(),
        "42723"
    );

    // The environment fixes which source is used, not its state: moving the shared clock moves
    // every database whose environment installed it.
    clock.set(2 * 86_400_000_000);
    assert_eq!(
        scalar(&a, "SELECT now()").unwrap(),
        "1970-01-03 00:00:00+00"
    );
    assert_eq!(
        scalar(&c, "SELECT now()").unwrap(),
        "1970-01-03 00:00:00+00"
    );
}

#[test]
fn a_database_outlives_the_callers_environment() {
    // The handle keeps its environment alive: dropping every caller reference changes nothing.
    let mut db = {
        let env = Environment::builder()
            .load_unicode_data(&unicode_bytes())
            .unwrap()
            .register_function(answer(7))
            .unwrap()
            .random_source(jed::seeded_random_source(1))
            .build();
        db_in(&env)
    };
    assert_eq!(scalar(&db, "SELECT answer()").unwrap(), "7");
    assert_eq!(
        scalar(&db, "SELECT 'b' < 'B' COLLATE \"unicode\"").unwrap(),
        "true"
    );
    // The seeded stream (spec/encoding/prng.toml seed 1) still serves the handle's sessions.
    assert_eq!(
        scalar(&db, "SELECT uuidv4()").unwrap(),
        "910a2dec-8902-4cc1-beeb-8da1658eec67"
    );
    // Attachments and every session share the handle's one environment.
    db.attach("side", jed::AttachSource::memory(), false)
        .unwrap();
    db.execute("CREATE TABLE side.t (a i64)", &[]).unwrap();
    db.execute("INSERT INTO side.t VALUES (answer())", &[])
        .unwrap();
    assert_eq!(scalar(&db, "SELECT a FROM side.t").unwrap(), "7");
    let s = db.session(SessionOptions::default());
    assert!(Environment::ptr_eq(s.environment(), db.environment()));
}
