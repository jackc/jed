//! Shared plumbing for the Rust benchmark harness binaries (spec/design/benchmarks.md).
//! Mirrors bench/go/internal/bench exactly: the splitmix64 param stream, the FNV-1a
//! answer checksum, corpus/dataset parsing, fingerprint checks, and the engine-agnostic
//! run loop. Each src/bin/bench_*.rs contributes only its driver.

use std::error::Error;
use std::fmt::Write as _;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub type BoxResult<T> = Result<T, Box<dyn Error>>;

// --- splitmix64 (benchmarks.md §4; vectors pinned in tests below) ---

pub struct Prng {
    z: u64,
}

impl Prng {
    pub fn new(seed: u64) -> Prng {
        Prng { z: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.z = self.z.wrapping_add(0x9E3779B97F4A7C15);
        let mut x = self.z;
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
        x ^ (x >> 31)
    }

    /// Bounded draw in [lo, hi] — modulo bias accepted, identical everywhere.
    pub fn int_uniform(&mut self, lo: i64, hi: i64) -> i64 {
        let span = (hi - lo) as u64 + 1;
        lo + (self.next_u64() % span) as i64
    }

    /// Lowercase ASCII string, length in [min_len, max_len].
    pub fn text(&mut self, min_len: i64, max_len: i64) -> String {
        let n = self.int_uniform(min_len, max_len);
        let mut s = String::with_capacity(n as usize);
        for _ in 0..n {
            s.push((b'a' + (self.next_u64() % 26) as u8) as char);
        }
        s
    }
}

// --- FNV-1a 64 answer checksum (benchmarks.md §6) ---

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

pub struct Checksum {
    h: u64,
}

impl Checksum {
    pub fn new() -> Checksum {
        Checksum { h: FNV_OFFSET }
    }

    fn bytes(&mut self, s: &[u8]) {
        for &b in s {
            self.h = (self.h ^ b as u64).wrapping_mul(FNV_PRIME);
        }
    }

    fn sep(&mut self, b: u8) {
        self.h = (self.h ^ b as u64).wrapping_mul(FNV_PRIME);
    }

    pub fn null(&mut self) {
        self.bytes(b"NULL");
        self.sep(0x1F);
    }

    pub fn int(&mut self, n: i64) {
        self.bytes(n.to_string().as_bytes());
        self.sep(0x1F);
    }

    pub fn text(&mut self, s: &str) {
        self.bytes(s.as_bytes());
        self.sep(0x1F);
    }

    pub fn end_row(&mut self) {
        self.sep(0x1E);
    }

    pub fn hex(&self) -> String {
        format!("{:016x}", self.h)
    }
}

impl Default for Checksum {
    fn default() -> Self {
        Self::new()
    }
}

// --- corpus (benchmarks.toml — benchmarks.md §3) ---

#[derive(Clone)]
pub struct Param {
    pub generator: String,
    pub min: i64,
    pub max: i64,
    pub start: i64,
    pub min_len: i64,
    pub max_len: i64,
    /// int_window: `base` is the 0-based index of an EARLIER param; the value is that param's value +
    /// int_uniform(off_min, off_max). Lets a bench express a selective fixed-width range around a
    /// base param (both endpoints const-sources).
    pub base: i64,
    pub off_min: i64,
    pub off_max: i64,
}

pub struct Bench {
    pub name: String,
    pub dataset: String,
    pub kind: String,
    pub sql: String,
    pub warmup: usize,
    pub iterations: usize,
    pub seed: u64,
    pub expect_rows_per_iter: Option<usize>,
    pub engines: Vec<String>,
    pub batch: usize,
    pub readers: usize,
    pub setup_sql: Vec<String>,
    pub sql_override: Vec<(String, String)>,
    pub setup_sql_override: Vec<(String, Vec<String>)>,
    pub params: Vec<Param>,
}

impl Bench {
    pub fn sql_for(&self, engine: &str) -> &str {
        for (e, s) in &self.sql_override {
            if e == engine {
                return s;
            }
        }
        &self.sql
    }

    pub fn setup_sql_for(&self, engine: &str) -> &[String] {
        for (e, s) in &self.setup_sql_override {
            if e == engine {
                return s;
            }
        }
        &self.setup_sql
    }

    pub fn runs_on(&self, engine: &str) -> bool {
        self.engines.is_empty() || self.engines.iter().any(|e| e == engine)
    }
}

fn str_field(t: &toml::value::Table, key: &str) -> String {
    t.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn int_field(t: &toml::value::Table, key: &str) -> i64 {
    t.get(key).and_then(|v| v.as_integer()).unwrap_or(0)
}

fn str_list(t: &toml::value::Table, key: &str) -> Vec<String> {
    t.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub fn load_corpus(corpus_dir: &str) -> BoxResult<Vec<Bench>> {
    let text = std::fs::read_to_string(format!("{corpus_dir}/benchmarks.toml"))?;
    let root: toml::Table = text.parse()?;
    let root = &root;
    if int_field(root, "schema_version") != 1 {
        return Err("benchmarks.toml: unsupported schema_version".into());
    }
    let mut benches = Vec::new();
    for b in root
        .get("bench")
        .and_then(|v| v.as_array())
        .ok_or("no [[bench]]")?
    {
        let t = b.as_table().ok_or("bench entry: not a table")?;
        let mut params = Vec::new();
        if let Some(list) = t.get("param").and_then(|v| v.as_array()) {
            for p in list {
                let pt = p.as_table().ok_or("param entry: not a table")?;
                params.push(Param {
                    generator: str_field(pt, "gen"),
                    min: int_field(pt, "min"),
                    max: int_field(pt, "max"),
                    start: int_field(pt, "start"),
                    min_len: int_field(pt, "min_len"),
                    max_len: int_field(pt, "max_len"),
                    base: int_field(pt, "base"),
                    off_min: int_field(pt, "off_min"),
                    off_max: int_field(pt, "off_max"),
                });
            }
        }
        let table_of_lists = |key: &str| -> Vec<(String, Vec<String>)> {
            t.get(key)
                .and_then(|v| v.as_table())
                .map(|ov| {
                    ov.iter()
                        .map(|(k, v)| {
                            let list = v
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|s| s.as_str().map(str::to_string))
                                        .collect()
                                })
                                .unwrap_or_default();
                            (k.clone(), list)
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        benches.push(Bench {
            name: str_field(t, "name"),
            dataset: str_field(t, "dataset"),
            kind: str_field(t, "kind"),
            sql: str_field(t, "sql"),
            warmup: int_field(t, "warmup") as usize,
            iterations: int_field(t, "iterations") as usize,
            seed: int_field(t, "seed") as u64,
            expect_rows_per_iter: t
                .get("expect_rows_per_iter")
                .and_then(|v| v.as_integer())
                .map(|n| n as usize),
            engines: str_list(t, "engines"),
            batch: int_field(t, "batch") as usize,
            readers: int_field(t, "readers") as usize,
            setup_sql: str_list(t, "setup_sql"),
            sql_override: t
                .get("sql_override")
                .and_then(|v| v.as_table())
                .map(|ov| {
                    ov.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default(),
            setup_sql_override: table_of_lists("setup_sql_override"),
            params,
        });
    }
    Ok(benches)
}

// --- datasets (datasets.toml — only what the harness needs: committed row counts) ---

pub fn dataset_table_rows(corpus_dir: &str, dataset: &str, table: &str) -> BoxResult<i64> {
    let text = std::fs::read_to_string(format!("{corpus_dir}/datasets.toml"))?;
    let root: toml::Table = text.parse()?;
    let root = &root;
    for ds in root
        .get("dataset")
        .and_then(|v| v.as_array())
        .ok_or("no [[dataset]]")?
    {
        let dt = ds.as_table().ok_or("dataset entry: not a table")?;
        if str_field(dt, "name") != dataset {
            continue;
        }
        for tb in dt
            .get("table")
            .and_then(|v| v.as_array())
            .unwrap_or(&Vec::new())
        {
            let tt = tb.as_table().ok_or("table entry: not a table")?;
            if str_field(tt, "name") == table {
                return Ok(int_field(tt, "rows"));
            }
        }
    }
    Err(format!("datasets.toml: no table {table} in dataset {dataset}").into())
}

// --- fingerprint (benchmarks.md §5) ---

pub fn corpus_fingerprint(corpus_dir: &str) -> BoxResult<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(format!("{corpus_dir}/datasets.toml"))?;
    let mut out = String::with_capacity(64);
    for b in Sha256::digest(&bytes) {
        write!(out, "{b:02x}").unwrap();
    }
    Ok(out)
}

pub fn read_sidecar(data_dir: &str, dataset: &str, engine: &str) -> String {
    std::fs::read_to_string(format!("{data_dir}/{dataset}.{engine}.fingerprint"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

pub fn stale_err(dataset: &str, engine: &str) -> Box<dyn Error> {
    format!("stale benchmark data for {dataset}/{engine}: run 'rake bench:setup'").into()
}

// --- param stream (benchmarks.md §3: one stream across warmup + measured) ---

pub enum Arg {
    Int(i64),
    Text(String),
}

pub struct ParamStream {
    params: Vec<Param>,
    prng: Prng,
    serials: Vec<i64>,
}

impl ParamStream {
    pub fn new(b: &Bench) -> ParamStream {
        let serials = b.params.iter().map(|p| p.start).collect();
        ParamStream {
            params: b.params.clone(),
            prng: Prng::new(b.seed),
            serials,
        }
    }

    pub fn next_args(&mut self) -> Vec<Arg> {
        let mut args = Vec::with_capacity(self.params.len());
        for (i, p) in self.params.iter().enumerate() {
            match p.generator.as_str() {
                "serial" => {
                    args.push(Arg::Int(self.serials[i]));
                    self.serials[i] += 1;
                }
                "int_uniform" => args.push(Arg::Int(self.prng.int_uniform(p.min, p.max))),
                "int_window" => {
                    let base = match args[p.base as usize] {
                        Arg::Int(n) => n,
                        _ => panic!("int_window base must be an int param"),
                    };
                    args.push(Arg::Int(base + self.prng.int_uniform(p.off_min, p.off_max)));
                }
                "text" => args.push(Arg::Text(self.prng.text(p.min_len, p.max_len))),
                other => panic!("unknown param gen {other}"),
            }
        }
        args
    }
}

// --- the engine contract ---

/// One open handle onto one dataset's database. `prepare` takes the corpus's $N SQL as
/// authored (a driver that needs ?N rewrites it itself) and stores the statement
/// internally — the runner uses one bench statement at a time, which sidesteps
/// statement-borrows-connection lifetimes (rusqlite re-prepares via its statement cache).
pub trait Engine {
    fn exec(&mut self, sql: &str) -> BoxResult<()>;
    fn prepare(&mut self, sql: &str) -> BoxResult<()>;
    fn query_prepared(&mut self, args: &[Arg], sum: Option<&mut Checksum>) -> BoxResult<usize>;
    fn exec_prepared(&mut self, args: &[Arg]) -> BoxResult<()>;
    fn query_int(&mut self, sql: &str) -> BoxResult<i64>;
    fn stored_fingerprint(&mut self) -> BoxResult<String>;

    /// Run a concurrent_read bench (spec/design/benchmarks.md §8.1): open one reader per
    /// block over the same committed data (jed: one SharedCore + N reader Sessions, the
    /// slice-7 convergence) and drive each block in parallel. `warm`/`meas` are already
    /// partitioned into `readers` contiguous blocks. Returns the per-block answer hashes
    /// (reader order), the merged per-query latencies, the total rows, and the wall clock
    /// of the timed phase. The default returns `None` → the runner SKIPS the bench, so a
    /// driver with no concurrent-session story (none, today, beyond jed) opts out for free.
    fn concurrent_read(
        &mut self,
        sql: &str,
        warm: Vec<Vec<Vec<Arg>>>,
        meas: Vec<Vec<Vec<Arg>>>,
        expect_rows: Option<usize>,
    ) -> BoxResult<Option<ConcurrentOutcome>> {
        let _ = (sql, warm, meas, expect_rows);
        Ok(None)
    }
}

/// The result of a concurrent_read run (benchmarks.md §8.1). `block_hexes` are the
/// per-reader-block FNV checksums in reader-index order — the runner folds them in that
/// order into the one partition-invariant answer checksum.
pub struct ConcurrentOutcome {
    pub block_hexes: Vec<String>,
    pub elapsed: Vec<i64>,
    pub rows_total: i64,
    pub wall_ns: i64,
}

pub struct Config {
    pub engine: &'static str,
    pub lang: &'static str,
    pub variant: &'static str,
    /// dataset is "small" | "large" | "scratch"; "scratch" must yield a fresh, empty database.
    pub open: fn(data_dir: &str, dataset: &str) -> BoxResult<Box<dyn Engine>>,
}

// --- one JSONL result line (benchmarks.md §6; field order is the contract) ---

pub struct ResultLine {
    pub bench: String,
    pub dataset: String,
    pub iterations: usize,
    pub warmup: usize,
    pub readers: usize,
    pub total_ns: i64,
    pub ns_per_op: i64,
    pub min_ns: i64,
    pub p50_ns: i64,
    pub rows_total: i64,
    pub checksum: String,
    pub fingerprint: String,
    pub started_at: String,
}

impl ResultLine {
    pub fn to_json(&self, cfg: &Config) -> String {
        format!(
            concat!(
                "{{\"schema\":1,\"bench\":\"{}\",\"dataset\":\"{}\",\"engine\":\"{}\",",
                "\"lang\":\"{}\",\"variant\":\"{}\",\"iterations\":{},\"warmup\":{},",
                "\"readers\":{},\"total_ns\":{},\"ns_per_op\":{},\"min_ns\":{},\"p50_ns\":{},",
                "\"rows_total\":{},\"checksum\":\"{}\",\"fingerprint\":\"{}\",\"started_at\":\"{}\"}}"
            ),
            self.bench,
            self.dataset,
            cfg.engine,
            cfg.lang,
            cfg.variant,
            self.iterations,
            self.warmup,
            self.readers,
            self.total_ns,
            self.ns_per_op,
            self.min_ns,
            self.p50_ns,
            self.rows_total,
            self.checksum,
            self.fingerprint,
            self.started_at
        )
    }
}

/// UTC RFC3339 from the system clock (civil-from-days; no date dependency).
pub fn utc_now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

// --- the run loop (mirrors bench/go/internal/bench/runner.go) ---

pub fn run(cfg: &Config, corpus_dir: &str, data_dir: &str, filter: &str) -> BoxResult<Vec<String>> {
    let benches = load_corpus(corpus_dir)?;
    let want = corpus_fingerprint(corpus_dir)?;
    let mut lines = Vec::new();
    for b in &benches {
        if !filter.is_empty() && !b.name.contains(filter) {
            continue;
        }
        if !b.runs_on(cfg.engine) {
            continue;
        }
        eprintln!(
            "{}/{}/{}: {} ({}) ...",
            cfg.engine, cfg.lang, cfg.variant, b.name, b.dataset
        );
        let line = run_one(cfg, b, corpus_dir, data_dir, &want)
            .map_err(|e| format!("bench {:?}: {e}", b.name))?;
        if let Some(line) = line {
            lines.push(line.to_json(cfg));
        }
    }
    Ok(lines)
}

fn run_one(
    cfg: &Config,
    b: &Bench,
    corpus_dir: &str,
    data_dir: &str,
    want: &str,
) -> BoxResult<Option<ResultLine>> {
    let mut eng = (cfg.open)(data_dir, &b.dataset)?;

    if b.dataset != "scratch" {
        let stored = eng.stored_fingerprint()?;
        if stored != want {
            return Err(stale_err(&b.dataset, cfg.engine));
        }
    }
    for sql in b.setup_sql_for(cfg.engine) {
        eng.exec(sql)
            .map_err(|e| format!("setup_sql {sql:?}: {e}"))?;
    }

    if b.kind == "concurrent_read" {
        return run_concurrent(cfg, b, eng.as_mut(), want);
    }

    eng.prepare(b.sql_for(cfg.engine))?;

    let started_at = utc_now_rfc3339();
    let mut stream = ParamStream::new(b);
    let mut sum = Checksum::new();
    let mut elapsed: Vec<i64> = Vec::with_capacity(b.iterations);
    let mut rows_total: i64 = 0;

    for i in 0..(b.warmup + b.iterations) {
        let measured = i >= b.warmup;
        match b.kind.as_str() {
            "query" => {
                let args = stream.next_args();
                let start = Instant::now();
                let n = eng.query_prepared(&args, if measured { Some(&mut sum) } else { None })?;
                let d = start.elapsed().as_nanos() as i64;
                if measured {
                    elapsed.push(d);
                    rows_total += n as i64;
                    if let Some(expect) = b.expect_rows_per_iter
                        && n != expect
                    {
                        return Err(format!("expected {expect} rows per iteration, got {n}").into());
                    }
                }
            }
            "write_rollback" => {
                let start = Instant::now();
                eng.exec("BEGIN")?;
                for _ in 0..b.batch {
                    let args = stream.next_args();
                    eng.exec_prepared(&args)?;
                }
                eng.exec("ROLLBACK")?;
                if measured {
                    elapsed.push(start.elapsed().as_nanos() as i64);
                }
            }
            "write_durable" => {
                let args = stream.next_args();
                let start = Instant::now();
                eng.exec_prepared(&args)?;
                if measured {
                    elapsed.push(start.elapsed().as_nanos() as i64);
                }
            }
            other => return Err(format!("unknown bench kind {other}").into()),
        }
    }

    // Write kinds: the checksum is the post-run sanity count(*) (benchmarks.md §6).
    if b.kind != "query" {
        let table = insert_table(&b.sql);
        let n = eng.query_int(&format!("SELECT count(*) FROM {table}"))?;
        let expect = match b.kind.as_str() {
            "write_rollback" => dataset_table_rows(corpus_dir, &b.dataset, &table)?,
            _ => (b.warmup + b.iterations) as i64,
        };
        if n != expect {
            return Err(format!("post-run count(*) of {table}: got {n}, want {expect}").into());
        }
        sum.int(n);
        sum.end_row();
    }

    elapsed.sort_unstable();
    let total_ns: i64 = elapsed.iter().sum();
    Ok(Some(ResultLine {
        bench: b.name.clone(),
        dataset: b.dataset.clone(),
        iterations: b.iterations,
        warmup: b.warmup,
        readers: 0,
        total_ns,
        ns_per_op: total_ns / b.iterations as i64,
        min_ns: elapsed[0],
        p50_ns: elapsed[(elapsed.len() - 1) / 2],
        rows_total,
        checksum: sum.hex(),
        fingerprint: want.to_string(),
        started_at,
    }))
}

/// partition tiles items into n contiguous blocks (the first len%n get one extra) — the
/// deterministic per-reader split the concurrent_read checksum folds over (benchmarks.md §6).
fn partition<T>(items: Vec<T>, n: usize) -> Vec<Vec<T>> {
    let len = items.len();
    let (base, extra) = (len / n, len % n);
    let mut it = items.into_iter();
    let mut blocks = Vec::with_capacity(n);
    for r in 0..n {
        let size = base + usize::from(r < extra);
        blocks.push((&mut it).take(size).collect());
    }
    blocks
}

/// run_concurrent drives a concurrent_read bench through the driver's `concurrent_read`
/// hook (benchmarks.md §8.1). It materializes the deterministic param stream, partitions
/// it into `readers` contiguous blocks, and lets the driver run them in parallel. total_ns
/// is the wall clock of the timed phase (THROUGHPUT — ns_per_op = wall/iterations falls as
/// readers scale), and the answer checksum folds the per-block hashes in reader order. A
/// driver without concurrency support returns None → the bench is skipped.
fn run_concurrent(
    cfg: &Config,
    b: &Bench,
    eng: &mut dyn Engine,
    want: &str,
) -> BoxResult<Option<ResultLine>> {
    let started_at = utc_now_rfc3339();
    let mut stream = ParamStream::new(b);
    let warm: Vec<Vec<Arg>> = (0..b.warmup).map(|_| stream.next_args()).collect();
    let meas: Vec<Vec<Arg>> = (0..b.iterations).map(|_| stream.next_args()).collect();
    let warm_blocks = partition(warm, b.readers);
    let meas_blocks = partition(meas, b.readers);

    let outcome = eng.concurrent_read(
        b.sql_for(cfg.engine),
        warm_blocks,
        meas_blocks,
        b.expect_rows_per_iter,
    )?;
    let Some(out) = outcome else {
        eprintln!(
            "  skip: {}/{}/{} has no concurrent_read support",
            cfg.engine, cfg.lang, cfg.variant
        );
        return Ok(None);
    };

    let mut combined = Checksum::new();
    for hex in &out.block_hexes {
        combined.text(hex);
    }
    let mut elapsed = out.elapsed;
    elapsed.sort_unstable();
    Ok(Some(ResultLine {
        bench: b.name.clone(),
        dataset: b.dataset.clone(),
        iterations: b.iterations,
        warmup: b.warmup,
        readers: b.readers,
        total_ns: out.wall_ns,
        ns_per_op: out.wall_ns / b.iterations as i64,
        min_ns: elapsed[0],
        p50_ns: elapsed[(elapsed.len() - 1) / 2],
        rows_total: out.rows_total,
        checksum: combined.hex(),
        fingerprint: want.to_string(),
        started_at,
    }))
}

// The target table of a write statement — the word after INTO (INSERT INTO <table>) or
// FROM (DELETE FROM <table>) — for the post-run count.
fn insert_table(sql: &str) -> String {
    let fields: Vec<&str> = sql.split_whitespace().collect();
    for (i, f) in fields.iter().enumerate() {
        if (f.eq_ignore_ascii_case("INTO") || f.eq_ignore_ascii_case("FROM"))
            && i + 1 < fields.len()
        {
            return fields[i + 1].split('(').next().unwrap().to_string();
        }
    }
    panic!("write bench SQL has no INSERT INTO / DELETE FROM table: {sql}");
}

/// Uniform binary entrypoint: bench-<engine> <corpus_dir> <data_dir> <out_path> [filter].
pub fn main_with(cfg: Config) {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 || args.len() > 5 {
        eprintln!(
            "usage: {} <corpus_dir> <data_dir> <out_path> [name_filter]",
            args[0]
        );
        std::process::exit(2);
    }
    let filter = args.get(4).map(String::as_str).unwrap_or("");
    match run(&cfg, &args[1], &args[2], filter) {
        Ok(lines) => {
            let mut out = lines.join("\n");
            if !out.is_empty() {
                out.push('\n');
            }
            if let Err(e) = std::fs::write(&args[3], out) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

// --- the pinned cross-language vectors (benchmarks.md §4/§6) ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prng_vectors() {
        let cases: [(u64, [u64; 5]); 2] = [
            (
                1,
                [
                    0x910a2dec89025cc1,
                    0xbeeb8da1658eec67,
                    0xf893a2eefb32555e,
                    0x71c18690ee42c90b,
                    0x71bb54d8d101b5b9,
                ],
            ),
            (
                1234567,
                [
                    0x599ed017fb08fc85,
                    0x2c73f08458540fa5,
                    0x883ebce5a3f27c77,
                    0x3fbef740e9177b3f,
                    0xe3b8346708cb5ecd,
                ],
            ),
        ];
        for (seed, want) in cases {
            let mut p = Prng::new(seed);
            for (i, w) in want.iter().enumerate() {
                assert_eq!(p.next_u64(), *w, "seed {seed} output {i}");
            }
        }
    }

    #[test]
    fn checksum_vector() {
        let mut c = Checksum::new();
        c.int(1);
        c.null();
        c.text("abc");
        c.end_row();
        c.int(-7);
        c.end_row();
        assert_eq!(c.hex(), "dd6e60407d30d28b");
    }
}
