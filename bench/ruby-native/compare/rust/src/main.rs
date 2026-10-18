use jed::{CreateOptions, Database, SessionOptions, Value};
use std::fs;
use std::io::{self, Write};
use std::time::Instant;

fn number(value: &str) -> usize {
    value.parse().unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let verify = args.get(2).is_some_and(|arg| arg == "--verify-only");
    let db = Database::create(CreateOptions::default()).unwrap();
    let mut session = db.session(SessionOptions::default());
    let mut repetitions = 0;
    let mut validation = 0;
    for line in fs::read_to_string(&args[1]).unwrap().lines() {
        let f: Vec<&str> = line.split('\t').collect();
        match f[0] {
            "config" => {
                repetitions = number(f[1]);
                validation = number(f[2]);
            }
            "setup" => {
                session.execute(f[1], &[]).unwrap();
            }
            "workload" => {
                let (name, iterations, warmup) = (f[1], number(f[2]), number(f[3]));
                let (offset, step, span): (i64, i64, i64) = (
                    f[4].parse().unwrap(),
                    f[5].parse().unwrap(),
                    f[6].parse().unwrap(),
                );
                let stmt = session.prepare(f[7]).unwrap();
                let parameters: Vec<Vec<Value>> = (0..iterations.max(warmup).max(validation))
                    .map(|i| {
                        if span == 0 {
                            vec![]
                        } else {
                            vec![Value::Int(offset + (i as i64 * step) % span)]
                        }
                    })
                    .collect();
                let mut hash = 0xcbf29ce484222325_u64;
                let mut run = |i: usize, checksum: bool| -> usize {
                    let rows = session.query_prepared(&stmt, &parameters[i]).unwrap();
                    let mut count = 0;
                    for row in rows {
                        count += 1;
                        if checksum {
                            for value in row {
                                let text = match value {
                                    Value::Null => "NULL".to_owned(),
                                    Value::Int(value) => value.to_string(),
                                    Value::Text(value) => value.to_string(),
                                    _ => panic!("unexpected result value"),
                                };
                                for byte in text.bytes().chain([0x1f]) {
                                    hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
                                }
                            }
                            hash = (hash ^ 0x1e).wrapping_mul(0x100000001b3);
                        }
                    }
                    count
                };
                let validation_rows: usize = (0..validation).map(|i| run(i, true)).sum();
                let mut samples: Vec<u128> = vec![];
                let mut total_rows = 0;
                if !verify {
                    for i in 0..warmup {
                        run(i, false);
                    }
                    for _ in 0..repetitions {
                        let start = Instant::now();
                        let rows: usize = (0..iterations).map(|i| run(i, false)).sum();
                        samples.push(start.elapsed().as_nanos());
                        total_rows += rows;
                    }
                }
                println!("{{\"workload\":\"{}\",\"checksum\":\"{:016x}\",\"validation_rows\":{},\"samples_ns\":{:?},\"rows_total\":{},\"iterations\":{},\"warmup\":{},\"runtime\":\"rust-release\"}}", name, hash, validation_rows, samples, total_rows, iterations, warmup);
                io::stdout().flush().unwrap();
            }
            _ => panic!("unknown plan entry"),
        }
    }
}
