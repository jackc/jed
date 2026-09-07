//! Host state, persisted dependency lifetime, and argument-sensitive PG admission extensions.
use jed::timezone::{load_time_zone_data, open_bundle, save_bundle};
use jed::{CreateOptions, Database, Engine, Outcome, SessionOptions, Value};
const BUNDLE: &[u8] = include_bytes!("../../../spec/tz/fixtures/tzdata.jtz");
const INDEX: &str = "CREATE UNIQUE INDEX zone_idx ON t (account) WHERE (ts AT TIME ZONE 'America/New_York')::date >= DATE '2024-01-01'";
fn run(db: &mut Engine, sql: &str) -> Outcome {
    jed::execute(db, sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}
fn code(db: &mut Engine, sql: &str, expected: &str) {
    assert_eq!(jed::execute(db, sql).unwrap_err().code(), expected, "{sql}");
}
fn build() -> Engine {
    load_time_zone_data(BUNDLE).unwrap();
    let mut db = Engine::new();
    run(
        &mut db,
        "CREATE TABLE t(id int PRIMARY KEY, account int, ts timestamptz)",
    );
    run(
        &mut db,
        "INSERT INTO t VALUES (1,7,'2024-06-01 00:00:00+00'),(2,7,'2020-01-01 00:00:00+00')",
    );
    run(&mut db, INDEX);
    db
}
fn skew_pin(original: &[u8], mode: &str) -> Vec<u8> {
    let mut img = original.to_vec();
    let tag = if mode == "missing" {
        &b"America/New_York"[..]
    } else {
        &b"2026a"[..]
    };
    let start = img.windows(tag.len()).rposition(|w| w == tag).unwrap();
    let off = start + tag.len() - usize::from(mode != "checksum");
    let name = img
        .windows(b"America/New_York".len())
        .rposition(|w| w == b"America/New_York")
        .unwrap();
    match mode {
        "mode" => img[name - 5] = 2,
        "empty-static" => {
            img[name - 4] = 0;
            img[name - 3] = 0;
        }
        "empty-name" => {
            img[name - 2] = 0;
            img[name - 1] = 0;
        }
        "empty-version" => {
            img[start - 2] = 0;
            img[start - 1] = 0;
        }
        _ => img[off] ^= 1,
    }

    let page = start / 4096 * 4096;
    let mut covered = img[page..page + 12].to_vec();
    covered.extend_from_slice(&img[page + 16..page + 4096]);
    img[page + 12..page + 16].copy_from_slice(&jed::format::crc32_ieee(&covered).to_be_bytes());
    img
}
#[test]
fn timezone_lifetime() {
    let image = build().to_image(4096, 1).unwrap();
    for mode in ["mode", "empty-static", "empty-name", "empty-version"] {
        match Engine::from_image(&skew_pin(&image, mode)) {
            Err(e) => assert_eq!(e.code(), "XX001", "{mode}"),
            Ok(_) => panic!("accepted {mode}"),
        }
    }
    let mut matching = Engine::from_image(&image).unwrap();
    code(
        &mut matching,
        "INSERT INTO t VALUES (3,7,'2024-06-01 00:00:00+00')",
        "23505",
    );
    for mode in ["version", "checksum", "missing"] {
        let mut db = Engine::from_image(&skew_pin(&image, mode)).unwrap();
        let Outcome::Query { rows: plan, .. } = run(
            &mut db,
            "EXPLAIN SELECT id FROM t WHERE (ts AT TIME ZONE 'America/New_York')::date >= DATE '2024-01-01' AND account = 7",
        ) else {
            panic!()
        };
        assert!(
            !plan
                .iter()
                .flatten()
                .any(|v| matches!(v,Value::Text(s) if s.contains("zone_idx")))
        );
        let Outcome::Query { rows, .. } = run(
            &mut db,
            "SELECT id FROM t WHERE (ts AT TIME ZONE 'America/New_York')::date >= DATE '2024-01-01' AND account = 7",
        ) else {
            panic!()
        };
        assert_eq!(rows, vec![vec![Value::Int(1)]]);
        code(
            &mut db,
            "INSERT INTO t VALUES (3,8,'2024-06-01 00:00:00+00')",
            "XX002",
        );
        code(&mut db, "UPDATE t SET account = 8 WHERE id = 999", "XX002");
        run(&mut db, "BEGIN");
        run(&mut db, "DROP INDEX zone_idx");
        code(
            &mut db,
            "CREATE UNIQUE INDEX zone_idx ON t(account)",
            "23505",
        );
        run(&mut db, "ROLLBACK");
        code(&mut db, "DELETE FROM t WHERE id = 2", "XX002");
        run(&mut db, "BEGIN");
        run(&mut db, "DROP INDEX zone_idx");
        run(&mut db, INDEX);
        run(&mut db, "COMMIT");
        run(
            &mut db,
            "INSERT INTO t VALUES (3,8,'2024-06-01 00:00:00+00')",
        );
        code(
            &mut db,
            "INSERT INTO t VALUES (4,7,'2024-06-01 00:00:00+00')",
            "23505",
        );
    }
}
#[test]
fn timezone_session_independence() {
    let mut db = build();
    for pred in [
        "ts IS NULL",
        "EXTRACT(epoch FROM ts)>0",
        "date_part('epoch',ts)>0",
        "make_timestamptz(2024,1,1,0,0,0,'UTC') < ts",
        "ts + INTERVAL '1 hour' > ts",
        "uuid_extract_timestamp(uuid '01941f29-7c00-7000-8000-000000000000') IS NOT NULL",
    ] {
        run(
            &mut db,
            &format!("CREATE INDEX ON t (account) WHERE {pred}"),
        );
    }
    for zone in ["UTC", "+01", "America/New_York"] {
        db.session.set_time_zone(zone).unwrap();
        code(
            &mut db,
            "INSERT INTO t VALUES (3,7,'2024-06-01 00:00:00+00')",
            "23505",
        );
    }
    code(
        &mut db,
        "CREATE INDEX unknown_zone ON t ((ts AT TIME ZONE 'Missing/Zone'))",
        "22023",
    );
}
#[test]
fn dynamic_zone_cache() {
    load_time_zone_data(BUNDLE).unwrap();
    let mut s = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    let ddl = "CREATE INDEX dynamic_idx ON d (account) WHERE (ts AT TIME ZONE zone)::date >= DATE '2024-01-01'";
    for sql in [
        "CREATE TABLE d (id int PRIMARY KEY, account int, ts timestamptz, zone text)",
        "INSERT INTO d VALUES (1,7,'2024-06-01 00:00:00+00','UTC')",
        ddl,
    ] {
        s.query_outcome(sql, &[]).unwrap();
    }
    let stmt=s.prepare("SELECT id FROM d WHERE (ts AT TIME ZONE zone)::date >= DATE '2024-01-01' AND account = 7").unwrap();
    for _ in 0..2 {
        let mut q = s.query_prepared(&stmt, &[]).unwrap();
        for row in &mut q {
            assert!(!row.is_empty());
        }
        q.error().unwrap();
    }
    assert!(stmt.cache().borrow().is_none());
    let ins = s
        .prepare("INSERT INTO d VALUES ($1,7,'2024-06-01 00:00:00+00','UTC') RETURNING id")
        .unwrap();
    for id in [90, 91] {
        let mut q = s.query_prepared(&ins, &[Value::Int(id)]).unwrap();
        for row in &mut q {
            assert!(!row.is_empty());
        }
        q.error().unwrap();
    }
    assert!(ins.insert_cache().borrow().is_none());
    let mut b = open_bundle(BUNDLE).unwrap();
    b.zones = vec![("Test/IndexAdded".into(), b.zones[0].1.clone())];
    b.links.clear();
    load_time_zone_data(&save_bundle(&b)).unwrap();
    {
        let mut q = s.query_prepared(&stmt, &[]).unwrap();
        for row in &mut q {
            assert!(!row.is_empty());
        }
        q.error().unwrap();
    }
    match s.query_prepared(&ins, &[Value::Int(92)]) {
        Err(e) => assert_eq!(e.code(), "XX002"),
        Ok(mut q) => {
            for _ in &mut q {}
            assert_eq!(q.error().unwrap_err().code(), "XX002");
        }
    }
    assert_eq!(
        s.query_outcome(
            "INSERT INTO d VALUES (2,7,'2024-06-01 00:00:00+00','UTC')",
            &[]
        )
        .unwrap_err()
        .code(),
        "XX002"
    );
    for sql in [
        "BEGIN",
        "DROP INDEX dynamic_idx",
        ddl,
        "COMMIT",
        "INSERT INTO d VALUES (2,7,'2024-06-01 00:00:00+00','UTC')",
    ] {
        s.query_outcome(sql, &[]).unwrap();
    }
}
