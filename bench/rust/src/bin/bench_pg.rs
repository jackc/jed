//! bench-pg benchmarks PostgreSQL via the sync `postgres` crate
//! (spec/design/benchmarks.md §6/§7). Unlike libpq, rust-postgres does not read the PG*
//! env itself, so the connection config is assembled from PGHOST/PGUSER here (the
//! devcontainer points PGHOST at the Unix socket; a path host uses the socket).

use postgres::types::{ToSql, Type};
use postgres::{Client, NoTls, Statement};

use jed_bench::{Arg, BoxResult, Checksum, Config, Engine, main_with};

fn main() {
    main_with(Config {
        engine: "postgres",
        lang: "rust",
        variant: "postgres-crate",
        open,
    });
}

struct PgEngine {
    client: Client,
    stmt: Option<Statement>,
}

fn open(_data_dir: &str, dataset: &str) -> BoxResult<Box<dyn Engine>> {
    let host = std::env::var("PGHOST").unwrap_or_else(|_| "localhost".to_string());
    let user = std::env::var("PGUSER").unwrap_or_else(|_| "postgres".to_string());
    let config = format!("host={host} user={user} dbname=jed_bench_{dataset}");
    let mut client = Client::connect(&config, NoTls)?;
    if dataset == "scratch" {
        // bench-setup created the empty scratch database once; reset it per run.
        client.execute("DROP TABLE IF EXISTS scratch", &[])?;
    }
    Ok(Box::new(PgEngine { client, stmt: None }))
}

// rust-postgres binds strictly by the statement's declared param types (i64 only fits
// INT8), so integer args are coerced to the prepared statement's expectation — the same
// effective behavior pgx/porsager get by inference. Owned values first, refs second.
enum Bound {
    I16(i16),
    I32(i32),
    I64(i64),
    Text(String),
}

fn coerce_args(args: &[Arg], stmt: &Statement) -> BoxResult<Vec<Bound>> {
    let types = stmt.params();
    let mut out = Vec::with_capacity(args.len());
    for (i, a) in args.iter().enumerate() {
        out.push(match (a, &types[i]) {
            (Arg::Int(n), &Type::INT2) => Bound::I16(i16::try_from(*n)?),
            (Arg::Int(n), &Type::INT4) => Bound::I32(i32::try_from(*n)?),
            (Arg::Int(n), _) => Bound::I64(*n),
            (Arg::Text(s), _) => Bound::Text(s.clone()),
        });
    }
    Ok(out)
}

fn bound_refs(bound: &[Bound]) -> Vec<&(dyn ToSql + Sync)> {
    bound
        .iter()
        .map(|b| match b {
            Bound::I16(v) => v as &(dyn ToSql + Sync),
            Bound::I32(v) => v as &(dyn ToSql + Sync),
            Bound::I64(v) => v as &(dyn ToSql + Sync),
            Bound::Text(v) => v as &(dyn ToSql + Sync),
        })
        .collect()
}

impl Engine for PgEngine {
    fn exec(&mut self, sql: &str) -> BoxResult<()> {
        self.client.batch_execute(sql)?;
        Ok(())
    }

    fn prepare(&mut self, sql: &str) -> BoxResult<()> {
        self.stmt = Some(self.client.prepare(sql)?);
        Ok(())
    }

    fn query_prepared(&mut self, args: &[Arg], sum: Option<&mut Checksum>) -> BoxResult<usize> {
        let stmt = self.stmt.as_ref().expect("prepare first");
        let bound = coerce_args(args, stmt)?;
        let rows = self.client.query(stmt, &bound_refs(&bound))?;
        if let Some(sum) = sum {
            for row in &rows {
                for (i, col) in row.columns().iter().enumerate() {
                    match *col.type_() {
                        Type::INT2 => match row.try_get::<_, Option<i16>>(i)? {
                            Some(v) => sum.int(v as i64),
                            None => sum.null(),
                        },
                        Type::INT4 => match row.try_get::<_, Option<i32>>(i)? {
                            Some(v) => sum.int(v as i64),
                            None => sum.null(),
                        },
                        Type::INT8 => match row.try_get::<_, Option<i64>>(i)? {
                            Some(v) => sum.int(v),
                            None => sum.null(),
                        },
                        Type::TEXT | Type::VARCHAR => match row.try_get::<_, Option<&str>>(i)? {
                            Some(v) => sum.text(v),
                            None => sum.null(),
                        },
                        ref other => return Err(format!("unexpected result type {other}").into()),
                    }
                }
                sum.end_row();
            }
        }
        Ok(rows.len())
    }

    fn exec_prepared(&mut self, args: &[Arg]) -> BoxResult<()> {
        let stmt = self.stmt.as_ref().expect("prepare first");
        let bound = coerce_args(args, stmt)?;
        self.client.execute(stmt, &bound_refs(&bound))?;
        Ok(())
    }

    fn query_int(&mut self, sql: &str) -> BoxResult<i64> {
        let row = self.client.query_one(sql, &[])?;
        Ok(row.get(0))
    }

    fn stored_fingerprint(&mut self) -> BoxResult<String> {
        match self.client.query_opt(
            "SELECT value FROM _bench_meta WHERE key = 'fingerprint'",
            &[],
        ) {
            Ok(Some(row)) => Ok(row.get(0)),
            _ => Ok(String::new()), // absent table/row reads as no fingerprint → stale
        }
    }
}
