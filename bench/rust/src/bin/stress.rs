//! stress is the Rust runner for Layer 3 of the concurrency contract
//! (spec/design/concurrency-testing.md §6): the parallelism-stress format. Unlike the Layer 1/2
//! `# format: concurrency` schedules (an explicit total order, run inside the conformance harness),
//! a `stress/*.stress.toml` file has NO order — writers and readers run concurrently and
//! correctness is checked by INVARIANTS, not a transcript. It is bench-family (outside `rake ci`):
//! timing-nondeterministic, but its answers are still checked (the confluent final state + a
//! cross-core answer checksum). Lives in the bench package so it reuses the shared splitmix64 PRNG
//! and the FNV-1a answer checksum (benchmarks.md §6) with no new dependency.
//!
//! Two execution modes drive the SAME worker definitions:
//!   - threaded   (Rust's native mode): one OS thread per worker over a cloned `SharedCore` handle
//!     (proving `Send + Sync` by moving it into each worker); writers contend on the single-writer
//!     gate for real (`write()` blocks on a held gate via the condvar), readers pin real snapshots.
//!     A deadline turns a wedged worker into a timeout (TSan optional — the threaded run already
//!     exercises the real acquire/condvar path).
//!   - sequential (`--sequential`): the seeded interleaver (§6) — the same algorithm the
//!     single-thread TS core uses. Deterministic given the file's seed; never truly blocks (a
//!     writer is scheduled to acquire the gate only while it is free).

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use jed::{Session, SharedCore, Value};
use jed_bench::{Checksum, Prng};

/// Bounds a threaded run: the balance workload finishes in well under a second, so a minute with no
/// completion means a worker is wedged (the §6 deadlock check).
const DEADLOCK_TIMEOUT: Duration = Duration::from_secs(60);

const LANG: &str = "rust";
const CAN_THREAD: bool = true;

// --- the stress file format (concurrency-testing.md §6) --------------------------------------

#[derive(Clone)]
struct Worker {
    kind: String, // "writer" | "reader"
    count: i64,
    iterations: i64,
    op: String,               // writer: BEGIN; … ; COMMIT;
    invariant_query: String,  // reader
    invariant_expect: String, // reader: the rendered scalar it must return
}

struct StressFile {
    name: String,
    parallel: String, // "optional" → sequential fallback here; "required" → skip in sequential mode
    seed: u64,
    setup: Vec<String>,
    workers: Vec<Worker>,
    final_query: Option<String>,
    final_expect: Vec<Vec<i64>>, // confluent final rows (empty = invariant-only)
    cross_core: bool,
}

struct StressResult {
    name: String,
    mode: String,   // "threaded" | "sequential" | "skipped"
    status: String, // "pass" | "fail" | "skip"
    invariant_checks: i64,
    writers: i64,
    writer_iters: i64,
    final_ok: bool,
    checksum: String,
    cross_core: bool,
    duration_ms: u128,
    error: String,
}

fn parse_stress(path: &str) -> Result<StressFile, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
    let root: toml::Table = text.parse().map_err(|e| format!("parse {path}: {e}"))?;

    let meta = root
        .get("meta")
        .and_then(|v| v.as_table())
        .ok_or("missing [meta]")?;
    let name = meta
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let parallel = meta
        .get("parallel")
        .and_then(|v| v.as_str())
        .unwrap_or("optional")
        .to_string();
    let seed = meta.get("seed").and_then(|v| v.as_integer()).unwrap_or(0) as u64;

    let setup = root
        .get("setup")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("sql"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let mut workers = Vec::new();
    if let Some(list) = root.get("worker").and_then(|v| v.as_array()) {
        for w in list {
            let t = w.as_table().ok_or("worker entry: not a table")?;
            workers.push(Worker {
                kind: t
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                count: t.get("count").and_then(|v| v.as_integer()).unwrap_or(0),
                iterations: t
                    .get("iterations")
                    .and_then(|v| v.as_integer())
                    .unwrap_or(0),
                op: t
                    .get("op")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                invariant_query: t
                    .get("invariant_query")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                invariant_expect: t
                    .get("invariant_expect")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }

    let mut final_query = None;
    let mut final_expect = Vec::new();
    let mut cross_core = false;
    if let Some(fin) = root.get("final").and_then(|v| v.as_table()) {
        final_query = fin
            .get("query")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        cross_core = fin
            .get("cross_core_checksum")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if let Some(rows) = fin.get("expect").and_then(|v| v.as_array()) {
            for row in rows {
                if let Some(cells) = row.as_array() {
                    final_expect.push(cells.iter().filter_map(|c| c.as_integer()).collect());
                }
            }
        }
    }

    Ok(StressFile {
        name,
        parallel,
        seed,
        setup,
        workers,
        final_query,
        final_expect,
        cross_core,
    })
}

/// parse_op splits a writer's `op` into the executable statements: bare BEGIN/COMMIT/ROLLBACK are
/// transaction MARKERS, mapped onto the handle's open/commit (§6), so they are dropped here.
fn parse_op(op: &str) -> Vec<String> {
    op.split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| {
            !matches!(
                s.to_ascii_uppercase().as_str(),
                "BEGIN" | "COMMIT" | "ROLLBACK"
            )
        })
        .map(str::to_string)
        .collect()
}

/// query_scalar runs a single-column, single-row query on a read handle and renders the scalar to
/// its canonical string (Value::render — so the decimal `sum(bigint)` result `1000` renders
/// identically across cores). The invariant is a string compare, not folded into the checksum.
fn query_scalar(rh: &mut Session, sql: &str) -> Result<String, String> {
    let mut rows = rh.query(sql, &[]).map_err(|e| e.to_string())?;
    match rows.next() {
        Some(row) => row
            .into_iter()
            .next()
            .map(|v| v.render())
            .ok_or_else(|| format!("empty row from {sql:?}")),
        None => Err(format!("no row from {sql:?}")),
    }
}

// --- setup + the final check (shared by both modes) ------------------------------------------

/// setup runs the file's setup SQL as one durable write transaction (committed version 1).
fn setup(db: &SharedCore, f: &StressFile) -> Result<(), String> {
    let mut wh = db.write_session();
    for s in &f.setup {
        if let Err(e) = wh.execute(s, &[]) {
            let _ = wh.rollback();
            return Err(format!("setup {s:?}: {e}"));
        }
    }
    wh.commit().map_err(|e| format!("setup commit: {e}"))
}

/// check_final runs `final.query` against the final committed snapshot, folds the integer rows into
/// the answer checksum, and compares them to `final.expect` (when present — a confluent workload).
fn check_final(db: &SharedCore, f: &StressFile) -> Result<(String, bool), String> {
    let Some(query) = &f.final_query else {
        return Ok((String::new(), true));
    };
    let mut rh = db.read_session();
    let rows = rh.query(query, &[]).map_err(|e| e.to_string())?;
    let mut sum = Checksum::new();
    let mut got: Vec<Vec<i64>> = Vec::new();
    for row in rows {
        let mut ints = Vec::with_capacity(row.len());
        for v in row {
            match v {
                Value::Int(n) => {
                    sum.int(n);
                    ints.push(n);
                }
                Value::Null => sum.null(),
                other => {
                    return Err(format!(
                        "stress final query must return integer columns, got {other:?}"
                    ));
                }
            }
        }
        sum.end_row();
        got.push(ints);
    }
    Ok((sum.hex(), final_equal(&got, &f.final_expect)))
}

/// final_equal compares the observed final rows to the pinned expectation. An empty expectation
/// means the workload is invariant-only (not confluent) — the exact-rows check is skipped.
fn final_equal(got: &[Vec<i64>], want: &[Vec<i64>]) -> bool {
    if want.is_empty() {
        return true;
    }
    got == want
}

// --- threaded mode (Rust's native mode; real OS threads, Send + Sync coverage) ----------------

fn run_writer(db: &SharedCore, stmts: &[String], iterations: i64) -> Result<(), String> {
    for _ in 0..iterations {
        let mut wh = db.write_session(); // blocks while another writer holds the gate (real contention)
        for s in stmts {
            if let Err(e) = wh.execute(s, &[]) {
                let _ = wh.rollback();
                return Err(format!("writer exec {s:?}: {e}"));
            }
        }
        wh.commit().map_err(|e| format!("writer commit: {e}"))?;
    }
    Ok(())
}

fn run_reader(
    db: &SharedCore,
    query: &str,
    expect: &str,
    iterations: i64,
    checks: &AtomicI64,
) -> Result<(), String> {
    for _ in 0..iterations {
        let mut rh = db.read_session();
        let got = query_scalar(&mut rh, query)?; // rh drops at end of iteration → deregistered
        checks.fetch_add(1, Ordering::Relaxed);
        if got != expect {
            return Err(format!("invariant {query:?}: got {got}, want {expect}"));
        }
    }
    Ok(())
}

/// run_threaded spawns one OS thread per worker (a cloned handle moved into each), all concurrent,
/// and collects their results with a deadline (a wedged worker → timeout).
fn run_threaded(db: &SharedCore, f: &StressFile) -> Result<i64, String> {
    let checks = Arc::new(AtomicI64::new(0));
    let (tx, rx) = mpsc::channel::<Result<(), String>>();
    let mut spawned = 0;
    for wk in &f.workers {
        for _ in 0..wk.count {
            spawned += 1;
            let db = db.clone();
            let tx = tx.clone();
            let checks = checks.clone();
            let wk = wk.clone();
            std::thread::spawn(move || {
                let r = match wk.kind.as_str() {
                    "writer" => run_writer(&db, &parse_op(&wk.op), wk.iterations),
                    "reader" => run_reader(
                        &db,
                        &wk.invariant_query,
                        &wk.invariant_expect,
                        wk.iterations,
                        &checks,
                    ),
                    other => Err(format!("unknown worker kind {other:?}")),
                };
                let _ = tx.send(r);
            });
        }
    }
    drop(tx); // only the workers hold senders now

    let deadline = Instant::now() + DEADLOCK_TIMEOUT;
    let mut first_err: Option<String> = None;
    for _ in 0..spawned {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
            Err(_) => {
                return Err(format!(
                    "deadlock: workers did not finish within {DEADLOCK_TIMEOUT:?}"
                ));
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(checks.load(Ordering::Relaxed)),
    }
}

// --- seeded-sequential mode (the §6 interleaver; the same algorithm TS uses) -------------------

/// SeqWorker is one worker modeled as a program of atomic ops over the shared handle (writer:
/// acquire · exec… · commit; reader: open · check · close), advanced one op at a time.
struct SeqWorker {
    kind: String,
    stmts: Vec<String>,
    query: String,
    expect: String,
    iterations: i64,
    iter: i64,
    op: usize,
    wh: Option<Session>,
    rh: Option<Session>,
}

impl SeqWorker {
    fn done(&self) -> bool {
        self.iter >= self.iterations
    }

    /// runnable: the only gated op is a writer's acquire (op 0), which needs the gate free; every
    /// other op is always runnable, so the gate holder always progresses and there is no deadlock.
    fn runnable(&self, gate_free: bool) -> bool {
        if self.done() {
            return false;
        }
        if self.kind == "writer" && self.op == 0 {
            return gate_free;
        }
        true
    }
}

/// run_sequential walks the workers through the seeded interleaver: at each step the splitmix64
/// stream picks one runnable worker (fixed index order) and advances it one op. Deterministic given
/// the seed; reproduces the logical interleavings without ever truly blocking.
fn run_sequential(db: &SharedCore, f: &StressFile) -> Result<i64, String> {
    let mut workers: Vec<SeqWorker> = Vec::new();
    for wk in &f.workers {
        for _ in 0..wk.count {
            workers.push(SeqWorker {
                kind: wk.kind.clone(),
                stmts: parse_op(&wk.op),
                query: wk.invariant_query.clone(),
                expect: wk.invariant_expect.clone(),
                iterations: wk.iterations,
                iter: 0,
                op: 0,
                wh: None,
                rh: None,
            });
        }
    }
    let mut prng = Prng::new(f.seed);
    let mut gate_held = false;
    let mut checks: i64 = 0;
    loop {
        let runnable: Vec<usize> = workers
            .iter()
            .enumerate()
            .filter(|(_, w)| w.runnable(!gate_held))
            .map(|(i, _)| i)
            .collect();
        if runnable.is_empty() {
            break; // all done (the gate holder is always runnable, so empty ⇒ none remain)
        }
        let idx = runnable[(prng.next_u64() % runnable.len() as u64) as usize];
        step_seq(db, &mut workers[idx], &mut gate_held, &mut checks)?;
    }
    Ok(checks)
}

fn step_seq(
    db: &SharedCore,
    w: &mut SeqWorker,
    gate_held: &mut bool,
    checks: &mut i64,
) -> Result<(), String> {
    if w.kind == "writer" {
        if w.op == 0 {
            w.wh = Some(db.write_session()); // gate is free (guaranteed by runnable)
            *gate_held = true;
            w.op += 1;
        } else if w.op <= w.stmts.len() {
            let s = &w.stmts[w.op - 1];
            if let Err(e) = w.wh.as_mut().unwrap().execute(s, &[]) {
                let _ = w.wh.take().unwrap().rollback();
                return Err(format!("writer exec {s:?}: {e}"));
            }
            w.op += 1;
        } else {
            w.wh.take()
                .unwrap()
                .commit()
                .map_err(|e| format!("writer commit: {e}"))?;
            *gate_held = false;
            w.op = 0;
            w.iter += 1;
        }
    } else {
        match w.op {
            0 => {
                w.rh = Some(db.read_session());
                w.op += 1;
            }
            1 => {
                let got = query_scalar(w.rh.as_mut().unwrap(), &w.query)?;
                *checks += 1;
                if got != w.expect {
                    w.rh = None;
                    return Err(format!(
                        "invariant {:?}: got {got}, want {}",
                        w.query, w.expect
                    ));
                }
                w.op += 1;
            }
            _ => {
                w.rh = None; // drop → deregister (advance the watermark)
                w.op = 0;
                w.iter += 1;
            }
        }
    }
    Ok(())
}

// --- driving one file ------------------------------------------------------------------------

fn run_file(path: &str, force_sequential: bool) -> StressResult {
    let start = Instant::now();
    let mut res = StressResult {
        name: String::new(),
        mode: String::new(),
        status: String::new(),
        invariant_checks: 0,
        writers: 0,
        writer_iters: 0,
        final_ok: false,
        checksum: String::new(),
        cross_core: false,
        duration_ms: 0,
        error: String::new(),
    };
    let f = match parse_stress(path) {
        Ok(f) => f,
        Err(e) => {
            res.status = "fail".into();
            res.error = e;
            return res;
        }
    };
    res.name = f.name.clone();
    res.cross_core = f.cross_core;
    for wk in &f.workers {
        if wk.kind == "writer" {
            res.writers += wk.count;
            res.writer_iters += wk.count * wk.iterations;
        }
    }

    let sequential = force_sequential || !CAN_THREAD;
    if sequential && f.parallel == "required" {
        res.mode = "skipped".into();
        res.status = "skip".into();
        return res;
    }
    res.mode = if sequential { "sequential" } else { "threaded" }.into();

    let db = SharedCore::new_in_memory();
    if let Err(e) = setup(&db, &f) {
        res.status = "fail".into();
        res.error = e;
        return res;
    }

    let run = if sequential {
        run_sequential(&db, &f)
    } else {
        run_threaded(&db, &f)
    };
    res.duration_ms = start.elapsed().as_millis();
    match run {
        Ok(checks) => res.invariant_checks = checks,
        Err(e) => {
            res.status = "fail".into();
            res.error = e;
            return res;
        }
    }

    match check_final(&db, &f) {
        Ok((checksum, final_ok)) => {
            res.checksum = checksum;
            res.final_ok = final_ok;
            if !final_ok {
                res.status = "fail".into();
                res.error = "final state did not match [final].expect".into();
                return res;
            }
        }
        Err(e) => {
            res.status = "fail".into();
            res.error = e;
            return res;
        }
    }
    res.status = "pass".into();
    res
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn result_json(r: &StressResult) -> String {
    let mut s = format!(
        "{{\"schema\":1,\"name\":{},\"lang\":\"{LANG}\",\"mode\":{},\"status\":{},\"invariant_checks\":{},\"writers\":{},\"writer_iters\":{},\"final_ok\":{},\"checksum\":{},\"cross_core_checksum\":{},\"duration_ms\":{}",
        json_str(&r.name),
        json_str(&r.mode),
        json_str(&r.status),
        r.invariant_checks,
        r.writers,
        r.writer_iters,
        r.final_ok,
        json_str(&r.checksum),
        r.cross_core,
        r.duration_ms,
    );
    if !r.error.is_empty() {
        s.push_str(&format!(",\"error\":{}", json_str(&r.error)));
    }
    s.push('}');
    s
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut force_sequential = false;
    let mut positional: Vec<String> = Vec::new();
    for a in &args[1..] {
        if a == "--sequential" {
            force_sequential = true;
        } else {
            positional.push(a.clone());
        }
    }
    if positional.len() < 2 {
        eprintln!("usage: stress <stress_dir> <out_path> [name_filter] [--sequential]");
        std::process::exit(2);
    }
    let stress_dir = &positional[0];
    let out_path = &positional[1];
    let filter = positional.get(2).cloned().unwrap_or_default();

    let mut files: Vec<String> = std::fs::read_dir(stress_dir)
        .unwrap_or_else(|e| {
            eprintln!("read {stress_dir}: {e}");
            std::process::exit(1);
        })
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".stress.toml"))
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|p| filter.is_empty() || p.contains(&filter))
        .collect();
    files.sort();

    let mut out = String::new();
    let mut exit = 0;
    for file in &files {
        let res = run_file(file, force_sequential);
        out.push_str(&result_json(&res));
        out.push('\n');
        match res.status.as_str() {
            "pass" => eprintln!(
                "  PASS  {:<36} {:<10} checks={} checksum={} ({}ms)",
                res.name, res.mode, res.invariant_checks, res.checksum, res.duration_ms
            ),
            "skip" => eprintln!("  SKIP  {:<36} {}", res.name, res.mode),
            _ => {
                exit = 1;
                eprintln!("  FAIL  {:<36} {}: {}", res.name, res.mode, res.error);
            }
        }
    }
    if let Err(e) = std::fs::write(out_path, out) {
        eprintln!("write {out_path}: {e}");
        std::process::exit(1);
    }
    std::process::exit(exit);
}
