//! bench-jed benchmarks the Rust jed core (spec/design/benchmarks.md §6/§7).

use std::time::Instant;

use jed::{Database, DatabaseOptions, PreparedStatement, Session, SessionOptions, SharedCore, Value};
use jed_bench::{
    Arg, BoxResult, Checksum, ConcurrentOutcome, Config, Engine, main_with, read_sidecar,
};

fn main() {
    main_with(Config {
        engine: "jed",
        lang: "rust",
        variant: "core",
        open,
    });
}

struct JedEngine {
    // The persistent connection the bench drives (BEGIN/COMMIT/ROLLBACK span calls). It owns an
    // `Arc<Shared>`, so it keeps the storage alive after the local `Database` handle is dropped.
    sess: Session,
    stmt: Option<PreparedStatement>,
    data_dir: String,
    dataset: String,
    scratch: Option<String>, // temp dir holding the scratch file, removed on Drop
}

fn open(data_dir: &str, dataset: &str) -> BoxResult<Box<dyn Engine>> {
    if dataset == "scratch" {
        let dir = format!("{data_dir}/scratch-rust-{}", std::process::id());
        std::fs::create_dir_all(&dir)?;
        let db = Database::create(format!("{dir}/scratch.jed"), DatabaseOptions::default())
            .map_err(|e| e.to_string())?;
        let sess = db.session(SessionOptions::default());
        return Ok(Box::new(JedEngine {
            sess,
            stmt: None,
            data_dir: data_dir.to_string(),
            dataset: dataset.to_string(),
            scratch: Some(dir),
        }));
    }
    let db = Database::open(format!("{data_dir}/{dataset}.jed")).map_err(|e| e.to_string())?;
    let sess = db.session(SessionOptions::default());
    Ok(Box::new(JedEngine {
        sess,
        stmt: None,
        data_dir: data_dir.to_string(),
        dataset: dataset.to_string(),
        scratch: None,
    }))
}

impl Drop for JedEngine {
    fn drop(&mut self) {
        if let Some(dir) = &self.scratch {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn bind_args(args: &[Arg]) -> Vec<Value> {
    args.iter()
        .map(|a| match a {
            Arg::Int(n) => Value::Int(*n),
            Arg::Text(s) => Value::Text(s.clone()),
        })
        .collect()
}

impl Engine for JedEngine {
    fn exec(&mut self, sql: &str) -> BoxResult<()> {
        self.sess.execute(sql, &[]).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn prepare(&mut self, sql: &str) -> BoxResult<()> {
        self.stmt = Some(self.sess.prepare(sql).map_err(|e| e.to_string())?);
        Ok(())
    }

    fn query_prepared(&mut self, args: &[Arg], sum: Option<&mut Checksum>) -> BoxResult<usize> {
        let params = bind_args(args);
        let stmt = self.stmt.as_ref().expect("prepare first");
        let rows = self
            .sess
            .query_prepared(stmt, &params)
            .map_err(|e| e.to_string())?;
        let mut n = 0;
        match sum {
            None => {
                for _ in rows {
                    n += 1;
                }
            }
            Some(sum) => {
                for row in rows {
                    n += 1;
                    for v in row {
                        match v {
                            Value::Null => sum.null(),
                            Value::Int(x) => sum.int(x),
                            Value::Text(s) => sum.text(&s),
                            other => {
                                return Err(format!("unexpected result value {other:?}").into());
                            }
                        }
                    }
                    sum.end_row();
                }
            }
        }
        Ok(n)
    }

    fn exec_prepared(&mut self, args: &[Arg]) -> BoxResult<()> {
        let params = bind_args(args);
        let stmt = self.stmt.as_ref().expect("prepare first");
        self.sess
            .execute_prepared(stmt, &params)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn query_int(&mut self, sql: &str) -> BoxResult<i64> {
        let mut rows = self.sess.query(sql, &[]).map_err(|e| e.to_string())?;
        match rows.next().and_then(|r| r.into_iter().next()) {
            Some(Value::Int(n)) => Ok(n),
            other => Err(format!("expected one integer from {sql:?}, got {other:?}").into()),
        }
    }

    fn stored_fingerprint(&mut self) -> BoxResult<String> {
        Ok(read_sidecar(&self.data_dir, &self.dataset, "jed"))
    }

    // concurrent_read opens ONE SharedCore over the dataset file and runs each param block
    // on its own thread + reader Session (the slice-7 convergence, session.md §2.4/§10) —
    // every Session shares the core's committed snapshot + buffer pool and reads without
    // blocking (§3). The first pass warms the shared pool; the second is wall-clock-timed.
    fn concurrent_read(
        &mut self,
        sql: &str,
        warm: Vec<Vec<Vec<Arg>>>,
        meas: Vec<Vec<Vec<Arg>>>,
        expect_rows: Option<usize>,
    ) -> BoxResult<Option<ConcurrentOutcome>> {
        let path = format!("{}/{}.jed", self.data_dir, self.dataset);
        let core = SharedCore::open(&path).map_err(|e| e.to_string())?;
        let sql = sql.to_string();

        // Pass 1 — warmup, untimed.
        let warm_handles: Vec<_> = warm
            .into_iter()
            .map(|block| {
                let core = core.clone();
                let sql = sql.clone();
                std::thread::spawn(move || -> Result<(), String> {
                    let mut sess = core.read_session();
                    for args in &block {
                        reader_query(&mut sess, &sql, args, None)?;
                    }
                    sess.close();
                    Ok(())
                })
            })
            .collect();
        for h in warm_handles {
            h.join()
                .map_err(|_| "warmup reader panicked".to_string())??;
        }

        // Pass 2 — measured, timed by wall clock around the spawn/join of all readers.
        let start = Instant::now();
        let handles: Vec<_> = meas
            .into_iter()
            .map(|block| {
                let core = core.clone();
                let sql = sql.clone();
                std::thread::spawn(move || -> Result<(String, Vec<i64>, i64), String> {
                    let mut sess = core.read_session();
                    let mut sum = Checksum::new();
                    let mut elapsed = Vec::with_capacity(block.len());
                    let mut rows = 0i64;
                    for args in &block {
                        let t0 = Instant::now();
                        let n = reader_query(&mut sess, &sql, args, Some(&mut sum))?;
                        elapsed.push(t0.elapsed().as_nanos() as i64);
                        rows += n as i64;
                        if let Some(exp) = expect_rows
                            && n != exp
                        {
                            return Err(format!("expected {exp} rows per iteration, got {n}"));
                        }
                    }
                    sess.close();
                    Ok((sum.hex(), elapsed, rows))
                })
            })
            .collect();

        // Join in spawn (= reader-index) order so block_hexes folds in that order (§6).
        let mut block_hexes = Vec::new();
        let mut elapsed = Vec::new();
        let mut rows_total = 0i64;
        for h in handles {
            let (hex, el, rows) = h.join().map_err(|_| "reader panicked".to_string())??;
            block_hexes.push(hex);
            elapsed.extend(el);
            rows_total += rows;
        }
        let wall_ns = start.elapsed().as_nanos() as i64;
        Ok(Some(ConcurrentOutcome {
            block_hexes,
            elapsed,
            rows_total,
            wall_ns,
        }))
    }
}

// reader_query runs one query through a reader Session, re-parsing the SQL each call (the
// host session API has no prepared-statement form; benchmarks.md §8.1), folding rows into
// sum when measuring.
fn reader_query(
    sess: &mut Session,
    sql: &str,
    args: &[Arg],
    sum: Option<&mut Checksum>,
) -> Result<usize, String> {
    let params = bind_args(args);
    let rows = sess.query(sql, &params).map_err(|e| e.to_string())?;
    let mut n = 0;
    match sum {
        None => {
            for _ in rows {
                n += 1;
            }
        }
        Some(sum) => {
            for row in rows {
                n += 1;
                for v in row {
                    match v {
                        Value::Null => sum.null(),
                        Value::Int(x) => sum.int(x),
                        Value::Text(s) => sum.text(&s),
                        other => return Err(format!("unexpected result value {other:?}")),
                    }
                }
                sum.end_row();
            }
        }
    }
    Ok(n)
}
