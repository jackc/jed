//! bench-sqlite benchmarks SQLite via rusqlite (bundled C SQLite —
//! spec/design/benchmarks.md §7). The $N corpus placeholders are rewritten to SQLite's
//! explicit-numbered ?N at prepare time (§3); statements run through rusqlite's
//! statement cache, so per-iteration "re-prepare" is a hash lookup.

use rusqlite::Connection;
use rusqlite::types::ValueRef;

use jed_bench::{Arg, BoxResult, Checksum, Config, Engine, main_with, read_sidecar, stale_err};

fn main() {
    main_with(Config {
        engine: "sqlite",
        lang: "rust",
        variant: "rusqlite",
        open,
    });
}

struct SqliteEngine {
    conn: Connection,
    sql: String, // the current bench statement, ?N-rewritten
    data_dir: String,
    dataset: String,
    scratch: Option<String>,
}

/// $N → ?N (benchmarks.md §3).
fn rewrite_placeholders(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek().is_some_and(|d| d.is_ascii_digit()) {
            out.push('?');
        } else {
            out.push(c);
        }
    }
    out
}

fn open(data_dir: &str, dataset: &str) -> BoxResult<Box<dyn Engine>> {
    let mut scratch = None;
    let path = if dataset == "scratch" {
        let dir = format!("{data_dir}/scratch-rust-{}", std::process::id());
        std::fs::create_dir_all(&dir)?;
        scratch = Some(dir.clone());
        format!("{dir}/scratch.sqlite")
    } else {
        let p = format!("{data_dir}/{dataset}.sqlite");
        if !std::path::Path::new(&p).exists() {
            return Err(stale_err(dataset, "sqlite"));
        }
        p
    };
    let conn = Connection::open(&path)?;
    // The classic durable configuration (benchmarks.md §8); only matters for write benches.
    conn.pragma_update(None, "journal_mode", "DELETE")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    Ok(Box::new(SqliteEngine {
        conn,
        sql: String::new(),
        data_dir: data_dir.to_string(),
        dataset: dataset.to_string(),
        scratch,
    }))
}

impl Drop for SqliteEngine {
    fn drop(&mut self) {
        if let Some(dir) = &self.scratch {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn bind(stmt: &mut rusqlite::Statement<'_>, args: &[Arg]) -> rusqlite::Result<()> {
    for (i, a) in args.iter().enumerate() {
        match a {
            Arg::Int(n) => stmt.raw_bind_parameter(i + 1, n)?,
            Arg::Text(s) => stmt.raw_bind_parameter(i + 1, s)?,
        }
    }
    Ok(())
}

impl Engine for SqliteEngine {
    fn exec(&mut self, sql: &str) -> BoxResult<()> {
        self.conn.execute_batch(sql)?;
        Ok(())
    }

    fn prepare(&mut self, sql: &str) -> BoxResult<()> {
        self.sql = rewrite_placeholders(sql);
        self.conn.prepare_cached(&self.sql)?; // warm the cache; surface errors now
        Ok(())
    }

    fn query_prepared(&mut self, args: &[Arg], mut sum: Option<&mut Checksum>) -> BoxResult<usize> {
        let mut stmt = self.conn.prepare_cached(&self.sql)?;
        let cols = stmt.column_count();
        bind(&mut stmt, args)?;
        let mut rows = stmt.raw_query();
        let mut n = 0;
        while let Some(row) = rows.next()? {
            n += 1;
            if let Some(sum) = sum.as_deref_mut() {
                for i in 0..cols {
                    match row.get_ref(i)? {
                        ValueRef::Null => sum.null(),
                        ValueRef::Integer(v) => sum.int(v),
                        ValueRef::Text(t) => sum.text(std::str::from_utf8(t)?),
                        other => return Err(format!("unexpected result type {other:?}").into()),
                    }
                }
                sum.end_row();
            }
        }
        Ok(n)
    }

    fn exec_prepared(&mut self, args: &[Arg]) -> BoxResult<()> {
        let mut stmt = self.conn.prepare_cached(&self.sql)?;
        bind(&mut stmt, args)?;
        stmt.raw_execute()?;
        Ok(())
    }

    fn query_int(&mut self, sql: &str) -> BoxResult<i64> {
        Ok(self.conn.query_row(sql, [], |row| row.get(0))?)
    }

    fn stored_fingerprint(&mut self) -> BoxResult<String> {
        Ok(read_sidecar(&self.data_dir, &self.dataset, "sqlite"))
    }
}
