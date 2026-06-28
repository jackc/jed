//! bench-jed benchmarks the Rust jed core (spec/design/benchmarks.md §6/§7).

use jed::{DatabaseOptions, Engine as JedDb, PreparedStatement, Value};
use jed_bench::{Arg, BoxResult, Checksum, Config, Engine, main_with, read_sidecar};

fn main() {
    main_with(Config {
        engine: "jed",
        lang: "rust",
        variant: "core",
        open,
    });
}

struct JedEngine {
    db: JedDb,
    stmt: Option<PreparedStatement>,
    data_dir: String,
    dataset: String,
    scratch: Option<String>, // temp dir holding the scratch file, removed on Drop
}

fn open(data_dir: &str, dataset: &str) -> BoxResult<Box<dyn Engine>> {
    if dataset == "scratch" {
        let dir = format!("{data_dir}/scratch-rust-{}", std::process::id());
        std::fs::create_dir_all(&dir)?;
        let db = JedDb::create(format!("{dir}/scratch.jed"), DatabaseOptions::default())
            .map_err(|e| e.to_string())?;
        return Ok(Box::new(JedEngine {
            db,
            stmt: None,
            data_dir: data_dir.to_string(),
            dataset: dataset.to_string(),
            scratch: Some(dir),
        }));
    }
    let db = JedDb::open(format!("{data_dir}/{dataset}.jed")).map_err(|e| e.to_string())?;
    Ok(Box::new(JedEngine {
        db,
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
        self.db.execute(sql, &[]).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn prepare(&mut self, sql: &str) -> BoxResult<()> {
        self.stmt = Some(self.db.prepare(sql).map_err(|e| e.to_string())?);
        Ok(())
    }

    fn query_prepared(&mut self, args: &[Arg], sum: Option<&mut Checksum>) -> BoxResult<usize> {
        let params = bind_args(args);
        let rows = self
            .stmt
            .as_ref()
            .expect("prepare first")
            .query(&mut self.db, &params)
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
        self.stmt
            .as_ref()
            .expect("prepare first")
            .execute(&mut self.db, &params)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn query_int(&mut self, sql: &str) -> BoxResult<i64> {
        let mut rows = self.db.query(sql, &[]).map_err(|e| e.to_string())?;
        match rows.next().and_then(|r| r.into_iter().next()) {
            Some(Value::Int(n)) => Ok(n),
            other => Err(format!("expected one integer from {sql:?}, got {other:?}").into()),
        }
    }

    fn stored_fingerprint(&mut self) -> BoxResult<String> {
        Ok(read_sidecar(&self.data_dir, &self.dataset, "jed"))
    }
}
