//! Standalone spill benchmark worker; the shared Ruby driver owns the workload.
use std::io::{BufRead, BufReader};
use std::time::Instant;

use jed::{CreateOptions, Database, Locking, OpenOptions, SessionOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 6 {
        return Err("usage: spill create|open database work_mem sql.tsv cache_bytes".into());
    }
    let budget: usize = args[3].parse()?;
    let cache: usize = args[5].parse()?;
    if cache == 0 {
        return Err("cache_bytes must be positive".into());
    }
    let db = if args[1] == "create" {
        Database::create(CreateOptions {
            path: Some(args[2].clone().into()),
            skip_fsync: true,
            locking: Locking::None,
            ..Default::default()
        })?
    } else {
        Database::open_with_options(
            &args[2],
            OpenOptions {
                cache_bytes: cache,
                locking: Locking::None,
                ..Default::default()
            },
        )?
    };
    let mut session = db.session(SessionOptions::default());
    session.set_work_mem(budget);
    for line in BufReader::new(std::fs::File::open(&args[4])?).lines() {
        let line = line?;
        let (name, sql) = line.split_once('\t').ok_or("invalid workload line")?;
        let start = Instant::now();
        let mut rows = session.query(sql, &[])?;
        let mut hash = 14_695_981_039_346_656_037_u64;
        let mut count = 0_u64;
        for row in rows.by_ref() {
            count += 1;
            for value in row {
                let s = value.render();
                for byte in format!("{}:", s.len()).bytes().chain(s.bytes()) {
                    hash = (hash ^ u64::from(byte)).wrapping_mul(1_099_511_628_211);
                }
            }
            hash = (hash ^ u64::from(b'\n')).wrapping_mul(1_099_511_628_211);
        }
        rows.error()?;
        if name != "-" {
            println!(
                "{{\"name\":\"{name}\",\"rows\":{count},\"cost\":{},\"checksum\":\"{hash:016x}\",\"ms\":{}}}",
                rows.cost(),
                start.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
    Ok(())
}
