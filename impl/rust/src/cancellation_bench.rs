// Ignored measurement, deliberately in shared's scope so prepared execution can use `armed`
// without inventing a public API just for a benchmark. No new dependency.
use super::*;
use std::hint::black_box;
use std::time::{Duration, Instant};

#[test]
#[ignore]
fn cancellation_benchmark() {
    let mut db = Database::create(CreateOptions::default()).unwrap();
    for sql in [
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
        "INSERT INTO t SELECT i, i % 100 FROM generate_series(1, 10000) AS g(i)",
    ] {
        db.execute(sql, &[]).unwrap();
    }
    let token = CancellationToken::new();
    let mut s = db.session(SessionOptions::default());
    for (name, sql) in [
        ("cheap", "SELECT v FROM t WHERE id = 42"),
        ("stream", "SELECT v FROM t ORDER BY id LIMIT 10000"),
        ("aggregate", "SELECT sum(v), count(*) FROM t"),
        ("projection", "SELECT v FROM t"),
    ] {
        let stmt = s.prepare(sql).unwrap();
        for (mode, enabled, limited) in [
            ("disabled", false, false),
            ("enabled", true, false),
            ("scalar-disabled", false, true),
            ("scalar-enabled", true, true),
        ] {
            s.set_max_cost(if limited { i64::MAX } else { 0 });
            for _ in 0..3 {
                let start = Instant::now();
                let mut n = 0;
                while start.elapsed() < Duration::from_millis(500) {
                    let mut rows = if enabled {
                        s.armed(&token, |s| s.query_prepared(&stmt, &[]))
                    } else {
                        s.query_prepared(&stmt, &[])
                    }
                    .unwrap();
                    for row in rows.by_ref() {
                        black_box(row);
                    }
                    rows.error().unwrap();
                    black_box(rows.cost());
                    n += 1;
                }
                println!(
                    "{name}/{mode} {:.1} ns/op {:.1} ops/s",
                    start.elapsed().as_nanos() as f64 / n as f64,
                    n as f64 / start.elapsed().as_secs_f64()
                );
            }
        }
    }
    let start = Instant::now();
    for _ in 0..1000000 {
        black_box(CancellationToken::new());
    }
    println!(
        "token creation/drop {:.1} ns/op",
        start.elapsed().as_nanos() as f64 / 1000000.0
    );
}

#[test]
#[ignore]
fn cancellation_latency_measurement() {
    let mut db = Database::create(CreateOptions::default()).unwrap();
    for sql in [
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
        "INSERT INTO t SELECT i, i % 100 FROM generate_series(1, 1000000) AS g(i)",
    ] {
        db.execute(sql, &[]).unwrap();
    }
    for sql in [
        "SELECT sum(v) FROM t",
        "SELECT v FROM t",
        "SELECT v FROM t ORDER BY id LIMIT 1000000",
    ] {
        let mut samples = Vec::new();
        for _ in 0..20 {
            let token = CancellationToken::new();
            let mut rows = db.query_cancelable(sql, &[], &token).unwrap();
            let worker = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(1));
                let start = Instant::now();
                token.cancel();
                start
            });
            for row in rows.by_ref() {
                black_box(row);
            }
            let end = Instant::now();
            assert_eq!(rows.error().unwrap_err().code(), "57014");
            samples.push(end.duration_since(worker.join().unwrap()).as_nanos());
        }
        samples.sort();
        println!(
            "{sql}: cancellation response median={}ns p95={}ns max={}ns",
            samples[10], samples[18], samples[19]
        );
    }
}
