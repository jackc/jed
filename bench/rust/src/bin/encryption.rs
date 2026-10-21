//! Standalone encryption-at-rest benchmark worker (spec/design/encryption.md §9, *Integrated
//! performance*); mise-tasks/bench/encryption alternates the variants and reports.
//!
//! `encryption plain|encrypted DIR ROWS` builds a table of ROWS rows (`id i64 PRIMARY KEY`, a 100-byte
//! text) in batches of 1000 committed rows, then reopens it with a 1 MiB page cache and runs a full
//! scan and random point lookups whose leaves mostly miss the cache, then the same lookups with a
//! cache that holds the whole table. Each phase prints one `phase\tmillis\tchecksum` line. Commits
//! skip fsync, so the commit phase measures sealing rather than the disk.
use std::time::Instant;

use jed::{CreateOptions, Credential, Database, Locking, OpenOptions, SessionOptions, Value};

const KEY: [u8; 32] = [0x4A; 32];
const LOOKUPS: i64 = 20_000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 || !matches!(args[1].as_str(), "plain" | "encrypted") {
        return Err("usage: encryption plain|encrypted DIR ROWS".into());
    }
    let credential = (args[1] == "encrypted").then_some(Credential::Key(KEY));
    let path = std::path::PathBuf::from(&args[2]).join(format!("{}.jed", args[1]));
    let _ = std::fs::remove_file(&path);
    let rows: i64 = args[3].parse()?;

    let db = Database::create(CreateOptions {
        path: Some(path.clone()),
        skip_fsync: true,
        locking: Locking::None,
        credential: credential.clone(),
        ..Default::default()
    })?;
    let mut s = db.session(SessionOptions::default());
    s.execute("CREATE TABLE t (id i64 PRIMARY KEY, v text)", &[])?;
    let start = Instant::now();
    let mut lo = 1;
    while lo <= rows {
        let hi = (lo + 999).min(rows);
        s.execute(
            &format!("INSERT INTO t SELECT g, repeat(md5(g::text), 3) || 'abcd' FROM generate_series({lo}, {hi}) g"),
            &[],
        )?;
        lo = hi + 1;
    }
    report("commit_1000_row_batches", start, rows);
    drop(s);
    db.close()?;

    for (phase, cache) in [("cold", 1usize << 20), ("hot", 1usize << 30)] {
        let db = Database::open_with_options(
            &path,
            OpenOptions {
                skip_fsync: true,
                locking: Locking::None,
                cache_bytes: cache,
                credential: credential.clone(),
                ..Default::default()
            },
        )?;
        let mut s = db.session(SessionOptions::default());
        if phase == "cold" {
            let start = Instant::now();
            let mut q = s.query("SELECT count(*), sum(length(v)) FROM t", &[])?;
            let row = q.next().expect("one row");
            let sum = match &row[1] {
                Value::Int(n) => *n,
                other => panic!("{other:?}"),
            };
            drop(q);
            report("full_scan_cold", start, sum);
        } else {
            // Warm the cache so every lookup is a hit.
            s.query("SELECT count(*) FROM t", &[])?.for_each(drop);
        }
        let stmt = s.prepare("SELECT length(v) FROM t WHERE id = $1")?;
        let start = Instant::now();
        let mut checksum = 0i64;
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..LOOKUPS {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let id = (x % rows as u64) as i64 + 1;
            for row in s.query_prepared(&stmt, &[Value::Int(id)])? {
                if let Value::Int(n) = row[0] {
                    checksum += n + id;
                }
            }
        }
        report(&format!("point_lookups_{phase}"), start, checksum);
        drop(s);
        db.close()?;
    }
    let size = std::fs::metadata(&path)?.len();
    println!("file_bytes\t0\t{size}");
    std::fs::remove_file(&path)?;
    Ok(())
}

fn report(phase: &str, start: Instant, checksum: i64) {
    println!(
        "{phase}\t{:.3}\t{checksum}",
        start.elapsed().as_secs_f64() * 1000.0
    );
}
