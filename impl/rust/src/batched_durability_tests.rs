//! Batched durability under synchronous = off (spec/design/validated-cow.md §4): host-API and
//! byte-level assertions the corpus cannot express — barrier counts, meta-slot placement, the batch
//! cap, close's flush, byte identity between the background and inline cap flush, and the time-based
//! flush (§4.1) on a manual clock. Crash recovery
//! under relaxed mode is the simulator's (spec/design/simulation-testing.md, the relaxed profile).
//! Mirrors the Go batched_durability_test.go.
use super::*;
use crate::blockstore::{BlockStore, ClosedBlockStore};
use crate::environment::Environment;
use crate::file::Synchronous;

/// One recorded host operation.
#[derive(Clone)]
enum Op {
    Write(u64, Vec<u8>),
    Sync,
    SetSize(u64),
}

/// A pass-through store that records every write, barrier, and resize, and can fail every barrier.
struct Recording {
    base: Box<dyn BlockStore>,
    ops: Arc<Mutex<Vec<Op>>>,
    fail_sync: bool,
}
impl BlockStore for Recording {
    fn read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.base.read_at(offset, len)
    }
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        self.ops
            .lock()
            .unwrap()
            .push(Op::Write(offset, bytes.to_vec()));
        self.base.write_at(offset, bytes)
    }
    fn sync(&mut self) -> Result<()> {
        if self.fail_sync {
            return Err(EngineError::new(SqlState::IoError, "injected sync failure"));
        }
        self.ops.lock().unwrap().push(Op::Sync);
        self.base.sync()
    }
    fn size(&mut self) -> Result<u64> {
        self.base.size()
    }
    fn set_size(&mut self, bytes: u64) -> Result<()> {
        self.ops.lock().unwrap().push(Op::SetSize(bytes));
        self.base.set_size(bytes)
    }
    fn skips_sync(&self) -> bool {
        self.base.skips_sync()
    }
}

/// Wrap main's pager store in a [`Recording`], returning its op log.
fn record(db: &Database, fail_sync: bool) -> Arc<Mutex<Vec<Op>>> {
    let ops = Arc::new(Mutex::new(Vec::new()));
    let st = db.0.storage.lock().unwrap();
    let mut pager = st.paging.pager();
    let base = pager.swap_store(Box::new(ClosedBlockStore));
    pager.swap_store(Box::new(Recording {
        base,
        ops: Arc::clone(&ops),
        fail_sync,
    }));
    ops
}

fn syncs(ops: &Arc<Mutex<Vec<Op>>>) -> usize {
    ops.lock()
        .unwrap()
        .iter()
        .filter(|op| matches!(op, Op::Sync))
        .count()
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("jed-batched-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A relaxed database with no time-based flush, so wall time never moves a flush.
fn relaxed_db(path: &std::path::Path, page_size: u32, inline: bool) -> Database {
    relaxed_db_timed(path, page_size, inline, 0, Environment::default())
}

/// A relaxed database whose batches end `interval_ms` after they start on `env`'s clock.
fn relaxed_db_timed(
    path: &std::path::Path,
    page_size: u32,
    inline: bool,
    interval_ms: u64,
    env: Environment,
) -> Database {
    let db = Database::create(CreateOptions {
        path: Some(path.to_path_buf()),
        page_size,
        synchronous: Synchronous::Off,
        flush_interval_ms: interval_ms,
        environment: env,
        ..CreateOptions::default()
    })
    .unwrap();
    db.0.storage.lock().unwrap().inline_flush = inline;
    db
}

fn exec(db: &mut Database, sql: &str) {
    if let Err(e) = db.execute_script(sql) {
        panic!("{sql}: {}", e.message);
    }
}

fn meta_txid_at(path: &std::path::Path, slot: usize) -> u64 {
    let img = std::fs::read(path).unwrap();
    let ps = u32::from_be_bytes(img[8..12].try_into().unwrap()) as usize;
    u64::from_be_bytes(img[slot * ps + 12..slot * ps + 20].try_into().unwrap())
}

#[test]
fn relaxed_commits_defer_the_barrier() {
    let dir = scratch("defer");
    let path = dir.join("relaxed.jed");
    let mut db = relaxed_db(&path, 0, false);
    exec(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v text)");
    let ops = record(&db, false);
    for i in 1..=10 {
        exec(&mut db, &format!("INSERT INTO t VALUES ({i}, 'x')"));
    }
    assert_eq!(
        syncs(&ops),
        0,
        "relaxed commits issued barriers below the batch cap"
    );
    // Every pending commit writes the slot that does not hold the floor (create's txid 1 in slot 1).
    assert_eq!(
        meta_txid_at(&path, 0),
        db.version(),
        "slot 0 holds the newest pending commit"
    );
    assert_eq!(meta_txid_at(&path, 1), 1, "slot 1 holds the floor (txid 1)");
    db.close().unwrap();
    assert_eq!(syncs(&ops), 1, "close issues the one flush");
    let reopened = Database::open(&path).unwrap();
    assert_eq!(reopened.rows_in_key_order("t").unwrap().len(), 10);
    reopened.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn relaxed_batch_cap_flushes_in_background_with_same_bytes() {
    let dir = scratch("cap");
    let mut images = Vec::new();
    let mut barriers = Vec::new();
    for inline in [false, true] {
        let path = dir.join(format!("cap{inline}.jed"));
        let mut db = relaxed_db(&path, 256, inline);
        exec(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v text)");
        exec(&mut db, "CREATE INDEX t_v ON t (v)");
        let ops = record(&db, false);
        for i in 1..=120 {
            exec(
                &mut db,
                &format!("INSERT INTO t VALUES ({i}, repeat('v', {}))", i % 40),
            );
            if i % 7 == 0 {
                exec(&mut db, &format!("DELETE FROM t WHERE id = {}", i - 3));
            }
        }
        let _ = db.0.storage.lock().unwrap().await_flush();
        barriers.push(syncs(&ops));
        db.close().unwrap();
        images.push(std::fs::read(&path).unwrap());
    }
    assert!(
        barriers[1] != 0 && barriers[0] == barriers[1],
        "cap flushes: background {}, inline {} — want equal and nonzero",
        barriers[0],
        barriers[1]
    );
    assert!(
        images[0] == images[1],
        "background and inline cap flushes wrote different files"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Churn updates through several batches: a commit inside a batch may reuse only pages free at the
/// floor, never one freed during the batch, and no free list may hold a pending page
/// (validated-cow.md §4 rules 3 and 4). The file must also stop growing: each batch's orphans
/// become reusable once a flush moves the floor past them.
#[test]
fn relaxed_batch_reuses_only_the_floors_free_pages() {
    let dir = scratch("reuse");
    let path = dir.join("reuse.jed");
    let mut db = relaxed_db(&path, 256, true);
    exec(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v text)");
    for i in 1..=40 {
        exec(
            &mut db,
            &format!("INSERT INTO t VALUES ({i}, repeat('v', 30))"),
        );
    }
    let state = |db: &Database| {
        let st = db.0.storage.lock().unwrap();
        let pending: HashSet<u32> = st
            .paging
            .pager()
            .pending_entries()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        (
            pending,
            st.free_pages.clone(),
            st.floor_free.clone(),
            st.page_count,
        )
    };
    let mut midway = 0;
    for i in 0..400u32 {
        let (before, _, floor_free, high_water) = state(&db);
        let letter = (b'a' + (i % 26) as u8) as char;
        exec(
            &mut db,
            &format!(
                "UPDATE t SET v = repeat('{letter}', 30) WHERE id = {}",
                1 + i % 40
            ),
        );
        let (after, free, _, _) = state(&db);
        for f in &free {
            assert!(
                !after.contains(f),
                "update {i}: the free list holds pending page {f}"
            );
        }
        if before.is_empty() || after.is_empty() {
            continue; // the batch's first commit plans as under synchronous = on; or a cap flush followed
        }
        for pg in &after {
            if before.contains(pg) || *pg >= high_water {
                continue;
            }
            assert!(
                floor_free.as_ref().is_some_and(|f| f.contains(pg)),
                "update {i} reused page {pg}, which was not free at the floor"
            );
        }
        if i == 200 {
            midway = db.0.storage.lock().unwrap().page_count;
        }
    }
    let end = db.0.storage.lock().unwrap().page_count;
    let cap = crate::format::manifest_capacities(256, crate::format::Cap::plain(256)).0 as u32;
    assert!(
        end <= midway + cap * 2,
        "the file kept growing under relaxed churn: {midway} pages at update 200, {end} at 400"
    );
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Crash a two-commit batch so that only the second commit's writes (its body pages and meta)
/// landed: the first commit's pages, which the second commit's tree still references, are missing.
/// The second meta's cumulative manifest lists them, so recovery must reject it and fall back to the
/// floor — never accept a root whose dependencies did not land (validated-cow.md §4 rule 2).
#[test]
fn relaxed_newest_meta_vouches_for_the_whole_batch() {
    let dir = scratch("vouch");
    let path = dir.join("vouch.jed");
    let mut db = relaxed_db(&path, 256, true);
    exec(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v text)");
    exec(
        &mut db,
        "INSERT INTO t SELECT g, repeat('a', 20) FROM generate_series(1, 60) g",
    );
    db.close().unwrap(); // the floor: every row 'a'
    let prior = std::fs::read(&path).unwrap();
    let mut reopened = Database::open_with_options(
        &path,
        OpenOptions {
            synchronous: Synchronous::Off,
            flush_interval_ms: 0,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    reopened.0.storage.lock().unwrap().inline_flush = true;
    // The first commit after an open stabilizes the adopted generation with a barrier
    // (validated-cow.md §3); it then starts the batch.
    exec(
        &mut reopened,
        "UPDATE t SET v = repeat('w', 20) WHERE id = 30",
    );
    let ops = record(&reopened, false);
    exec(
        &mut reopened,
        "UPDATE t SET v = repeat('b', 20) WHERE id = 1",
    ); // leaf A
    let first = ops.lock().unwrap().len();
    exec(
        &mut reopened,
        "UPDATE t SET v = repeat('c', 20) WHERE id = 60",
    ); // leaf B
    assert_eq!(
        syncs(&ops),
        0,
        "the two-commit batch reached a barrier; the test needs it pending"
    );
    // Power loss: the floor, plus every op of the second commit, and none of the first commit's.
    let mut img = prior.clone();
    for op in ops.lock().unwrap()[first..].iter() {
        match op {
            Op::Write(off, data) => {
                let end = *off as usize + data.len();
                if end > img.len() {
                    img.resize(end, 0);
                }
                img[*off as usize..end].copy_from_slice(data);
            }
            Op::SetSize(n) => {
                if *n as usize > img.len() {
                    img.resize(*n as usize, 0);
                }
            }
            Op::Sync => {}
        }
    }
    let mut recovered = Engine::from_image(&img).expect("recovery over a half-landed batch");
    let rows = match crate::execute(
        &mut recovered,
        "SELECT v FROM t WHERE id IN (1, 60) ORDER BY id",
    )
    .expect("recovery over a half-landed batch")
    {
        Outcome::Query { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    };
    let floor = Value::Text("a".repeat(20).into());
    assert!(
        rows.len() == 2 && rows[0][0] == floor && rows[1][0] == floor,
        "recovery over a half-landed batch returned {rows:?}, want the floor's rows"
    );
    drop(reopened);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Reader threads query while a writer commits across background cap flushes: every reader sees a
/// consistent snapshot, and the reopened file holds the end state.
#[test]
fn relaxed_background_flush_with_concurrent_readers() {
    let dir = scratch("readers");
    let path = dir.join("readers.jed");
    let mut db = relaxed_db(&path, 256, false);
    exec(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v i64)");
    exec(
        &mut db,
        "INSERT INTO t SELECT g, 0 FROM generate_series(1, 200) g",
    );
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let db = db.clone();
            let done = Arc::clone(&done);
            std::thread::spawn(move || -> std::result::Result<(), String> {
                while !done.load(std::sync::atomic::Ordering::Acquire) {
                    let mut s = db.read_session();
                    let mut rows = s
                        .query("SELECT count(*), min(v), max(v) FROM t", &[])
                        .map_err(|e| e.message.clone())?;
                    let row = rows.next().ok_or("no row")?;
                    rows.error().map_err(|e| e.message.clone())?;
                    let ints: Vec<i64> = row
                        .iter()
                        .map(|v| match v {
                            Value::Int(n) => *n,
                            _ => i64::MIN,
                        })
                        .collect();
                    if ints[0] != 200 || ints[1] != ints[2] {
                        return Err(format!(
                            "reader saw count {}, v from {} to {}",
                            ints[0], ints[1], ints[2]
                        ));
                    }
                    drop(rows);
                    s.close();
                }
                Ok(())
            })
        })
        .collect();
    for _ in 0..150 {
        exec(&mut db, "UPDATE t SET v = v + 1");
    }
    done.store(true, std::sync::atomic::Ordering::Release);
    for reader in readers {
        reader.join().unwrap().unwrap();
    }
    db.close().unwrap();
    let reopened = Database::open(&path).unwrap();
    let rows = reopened.rows_in_key_order("t").unwrap();
    assert_eq!(rows.len(), 200);
    assert!(
        rows.iter().all(|row| row[1] == Value::Int(150)),
        "reopened with rows not at v = 150"
    );
    reopened.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn relaxed_close_reports_a_failed_flush() {
    let dir = scratch("flushfail");
    let path = dir.join("flushfail.jed");
    let mut db = relaxed_db(&path, 0, false);
    exec(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY)");
    exec(&mut db, "INSERT INTO t VALUES (1)");
    record(&db, true);
    let err = db.close().expect_err("close with a failing flush");
    assert_eq!(err.code(), "58030", "close with a failing flush: {err:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn synchronous_on_keeps_txid_parity_slots() {
    let dir = scratch("on");
    let path = dir.join("on.jed");
    let mut db = Database::create(CreateOptions {
        path: Some(path.clone()),
        skip_fsync: true,
        ..CreateOptions::default()
    })
    .unwrap();
    exec(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY)");
    for i in 1..=5 {
        exec(&mut db, &format!("INSERT INTO t VALUES ({i})"));
        let v = db.version();
        assert_eq!(
            meta_txid_at(&path, (v & 1) as usize),
            v,
            "txid {v} not in slot {}",
            v & 1
        );
    }
    db.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Trailing truncation under synchronous = off (validated-cow.md §4, format.md *Trailing
/// truncation*): only a commit that begins with nothing pending lowers the high-water, and the file
/// is shortened only at the barrier that makes the lowering generation durable — the batch's cap
/// flush or close — never while the batch is pending. A table dropped at the top of the file leaves a
/// long free run there once a flush lets reclamation list its pages; until then the relaxed churn
/// grows the file past it. Mirrors the Go TestTrailingTruncationWaitsForTheFlushUnderSynchronousOff.
#[test]
fn relaxed_trailing_truncation_waits_for_the_flush() {
    const PAGE: u32 = 4096;
    let dir = scratch("truncate");
    let path = dir.join("truncate.jed");
    let mut db = Database::create(CreateOptions {
        path: Some(path.clone()),
        page_size: PAGE,
        skip_fsync: true,
        ..CreateOptions::default()
    })
    .unwrap();
    exec(
        &mut db,
        "CREATE TABLE t (id i64 PRIMARY KEY, pad text, n i64)",
    );
    exec(
        &mut db,
        "INSERT INTO t SELECT g, repeat('x', 300), g % 7 FROM generate_series(1, 100) g",
    );
    exec(&mut db, "CREATE TABLE big (id i64 PRIMARY KEY, pad text)");
    exec(
        &mut db,
        "INSERT INTO big SELECT g, repeat('z', 300) FROM generate_series(1, 20000) g",
    );
    db.close().unwrap();
    let mut db = Database::open_with_options(
        &path,
        OpenOptions {
            synchronous: Synchronous::Off,
            // No time-based flush: wall time must not end a batch here.
            flush_interval_ms: 0,
            skip_fsync: true,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    db.0.storage.lock().unwrap().inline_flush = true;
    let len = || std::fs::metadata(&path).unwrap().len();
    let state = |db: &Database| {
        let st = db.0.storage.lock().unwrap();
        let pending = st.paging.pager().has_pending();
        (st.page_count, pending)
    };
    let mut peak = state(&db).0;
    exec(&mut db, "DROP TABLE big");
    let mut deferred = false;
    for k in 0..1500u32 {
        let (before, (high, pending_before)) = (len(), state(&db));
        exec(
            &mut db,
            &format!(
                "UPDATE t SET pad = 'r{k}' || repeat('y', 200), n = n + 7 WHERE id = {}",
                1 + k % 100
            ),
        );
        let (length, (pc, pending)) = (len(), state(&db));
        peak = peak.max(pc);
        assert!(
            !(pending_before && pc < high),
            "commit {k} lowered the high-water from {high} to {pc} inside a pending batch"
        );
        assert!(
            !(pending && length < before),
            "commit {k} shortened the file from {before} to {length} bytes while its batch is pending"
        );
        let limit = (pc as u64 + crate::pager::growth_step(pc, PAGE) as u64) * PAGE as u64;
        if pending && pc < high && length > limit {
            deferred = true; // lowered now, shortened at the batch's flush
        }
    }
    let pc = state(&db).0;
    assert!(
        pc * 3 <= peak,
        "trailing truncation should return the dropped table's pages: {peak} pages at the peak, {pc} after churn"
    );
    assert!(
        deferred,
        "no relaxed commit lowered the high-water with its shrink left to the flush"
    );
    db.close().unwrap();
    let limit = (pc as u64 + crate::pager::growth_step(pc, PAGE) as u64) * PAGE as u64;
    assert!(
        len() <= limit,
        "close flushed the batch but left the file {} bytes (at most {limit})",
        len()
    );
    let mut re = Database::open(&path).unwrap();
    assert_eq!(state(&re).0, pc, "reopened page count");
    let mut s = re.session(SessionOptions::default());
    let mut rows = s.query("SELECT count(*) FROM t", &[]).unwrap();
    let row = rows.next().expect("a count row");
    assert!(matches!(row[0], Value::Int(100)), "reopened count");
    drop(rows);
    drop(s);
    re.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A manual clock and an environment that reads it.
fn manual_env() -> (Arc<crate::seam::ManualClock>, Environment) {
    let clock = crate::seam::manual_clock(0);
    let env = Environment::builder()
        .clock_source(Arc::clone(&clock) as Arc<dyn crate::seam::ClockSource>)
        .build();
    (clock, env)
}

fn pending(db: &Database) -> bool {
    db.0.storage.lock().unwrap().paging.pager().has_pending()
}

/// A commit that reads the clock at or past its batch's deadline flushes the batch before it plans
/// (validated-cow.md §4.1 rule 3), and starts the next batch itself.
#[test]
fn relaxed_deadline_flushes_before_the_next_commit() {
    let dir = scratch("deadline");
    let path = dir.join("deadline.jed");
    let (clock, env) = manual_env();
    let mut db = relaxed_db_timed(&path, 0, true, 1_000, env);
    exec(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY)");
    let ops = record(&db, false);
    exec(&mut db, "INSERT INTO t VALUES (1)");
    clock.advance(999_999);
    exec(&mut db, "INSERT INTO t VALUES (2)");
    assert_eq!(syncs(&ops), 0, "a commit before the deadline flushed");
    clock.advance(1);
    let before = db.version();
    exec(&mut db, "INSERT INTO t VALUES (3)");
    assert_eq!(
        syncs(&ops),
        1,
        "the commit at the deadline did not flush first"
    );
    // The flush made `before` the floor, so the new commit wrote the other slot and is pending.
    assert_eq!(meta_txid_at(&path, (before & 1) as usize), before);
    assert_eq!(
        meta_txid_at(&path, ((before & 1) ^ 1) as usize),
        db.version()
    );
    assert!(pending(&db), "the commit after the flush is not pending");
    // Its batch started at the reading of the flushing commit: the next deadline is a whole interval on.
    clock.advance(999_999);
    exec(&mut db, "INSERT INTO t VALUES (4)");
    assert_eq!(syncs(&ops), 1, "the new batch ended early");
    db.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Wait up to five seconds for `cond`.
fn eventually(mut cond: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(5) {
        if cond() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    cond()
}

/// The idle flusher flushes a batch no commit follows once the clock passes its deadline, and never
/// before (validated-cow.md §4.1 rule 5).
#[test]
fn relaxed_idle_flusher_waits_for_the_clock() {
    let dir = scratch("idle");
    let path = dir.join("idle.jed");
    let (clock, env) = manual_env();
    let mut db = relaxed_db_timed(&path, 0, false, 20, env);
    exec(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY)");
    let ops = record(&db, false);
    exec(&mut db, "INSERT INTO t VALUES (1)");
    std::thread::sleep(std::time::Duration::from_millis(150));
    assert_eq!(
        syncs(&ops),
        0,
        "the flusher flushed before the clock passed the deadline"
    );
    assert!(pending(&db));
    clock.advance(20_000);
    assert!(
        eventually(|| !pending(&db)),
        "the idle flusher never flushed the batch"
    );
    assert_eq!(syncs(&ops), 1);
    db.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The same commits and clock readings write the same file whether the idle flusher ran each flush
/// or the next commit did (validated-cow.md §4.1).
#[test]
fn relaxed_idle_flusher_writes_the_same_bytes() {
    let dir = scratch("idle-bytes");
    let mut images = Vec::new();
    for background in [true, false] {
        let path = dir.join(format!("idle{background}.jed"));
        let (clock, env) = manual_env();
        let mut db = relaxed_db_timed(&path, 256, !background, 10, env);
        exec(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, v text)");
        for i in 1..=40 {
            exec(
                &mut db,
                &format!("INSERT INTO t VALUES ({i}, repeat('v', {}))", i % 30),
            );
            if i % 6 == 0 {
                exec(&mut db, &format!("DELETE FROM t WHERE id = {}", i - 2));
            }
            if i % 5 == 0 {
                clock.advance(10_000);
                if background {
                    assert!(
                        eventually(|| !pending(&db)),
                        "the idle flusher never flushed after commit {i}"
                    );
                }
            }
        }
        db.close().unwrap();
        images.push(std::fs::read(&path).unwrap());
    }
    assert!(
        images[0] == images[1],
        "the idle flusher changed the file's bytes"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A batch's deadline is per database: an attachment follows its handle's interval and clock but
/// keeps its own batch.
#[test]
fn relaxed_deadline_is_per_database() {
    let dir = scratch("deadline-attach");
    let path = dir.join("main.jed");
    let (clock, env) = manual_env();
    let mut db = relaxed_db_timed(&path, 0, true, 1_000, env.clone());
    let other = dir.join("other.jed");
    Database::create(CreateOptions {
        path: Some(other.clone()),
        environment: env,
        ..CreateOptions::default()
    })
    .unwrap()
    .close()
    .unwrap();
    db.attach("o", AttachSource::file(&other), false).unwrap();
    exec(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY)");
    clock.advance(500_000);
    exec(&mut db, "CREATE TABLE o.u (id i32 PRIMARY KEY)");
    clock.advance(500_000);
    // Main's deadline has passed; the attachment's has not.
    exec(&mut db, "INSERT INTO o.u VALUES (1)");
    let attached_pending = |db: &Database| {
        db.0.attachments
            .lock()
            .unwrap()
            .get_mut("o")
            .unwrap()
            .with_storage(|st| st.paging.pager().has_pending())
    };
    assert!(
        attached_pending(&db),
        "the attachment's batch ended at main's deadline"
    );
    assert!(pending(&db), "main flushed without a commit of its own");
    exec(&mut db, "INSERT INTO t VALUES (1)");
    assert!(attached_pending(&db));
    db.close().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The `max_storage_bytes` precheck of a commit that also writes an in-memory attachment plans a
/// pending batch's reuse restriction, while the commit itself may first flush the batch at its
/// deadline (validated-cow.md §4.1) and plan without it. The answer cannot differ: the file form
/// limits live pages (memory.md §8.7), which do not depend on which page ids a commit takes. Run the
/// same transactions with no batching, with batches, and with a deadline flush before every commit:
/// every `54P06` lands on the same transaction.
#[test]
fn relaxed_storage_precheck_ignores_the_deadline() {
    let dir = scratch("precheck");
    let mut runs = Vec::new();
    for (k, mode) in ["on", "batched", "deadline"].into_iter().enumerate() {
        let path = dir.join(format!("precheck{k}.jed"));
        let (clock, env) = manual_env();
        let mut db = if mode == "on" {
            Database::create(CreateOptions {
                path: Some(path.clone()),
                page_size: 512,
                environment: env,
                ..CreateOptions::default()
            })
            .unwrap()
        } else {
            let interval = if mode == "deadline" { 1 } else { 0 };
            relaxed_db_timed(&path, 512, true, interval, env)
        };
        db.attach("m", AttachSource::memory(), false).unwrap();
        exec(&mut db, "CREATE TABLE t (id i64 PRIMARY KEY, v text)");
        exec(&mut db, "CREATE TABLE m.log (id i64 PRIMARY KEY)");
        db.set_max_storage_bytes("main", 40 * 512).unwrap();
        let mut outcomes = Vec::new();
        for i in 0..120i64 {
            let sql = match i % 4 {
                0 | 1 => format!(
                    "INSERT INTO t SELECT g, md5(g::text) || md5((g + 1)::text) FROM generate_series({}, {}) g",
                    i * 10,
                    i * 10 + 9
                ),
                2 => format!("DELETE FROM t WHERE id % 50 = {}", i % 50),
                _ => format!(
                    "UPDATE t SET v = left(md5(v), {}) || v WHERE id % 5 = 0",
                    1 + i % 20
                ),
            };
            let mut sess = db.session(SessionOptions::default());
            let run = |sess: &mut Session| -> Result<()> {
                sess.begin(true)?;
                sess.execute(&sql, &[])?;
                sess.execute(&format!("INSERT INTO m.log VALUES ({i})"), &[])?;
                sess.commit()
            };
            let failed = run(&mut sess).err().map(|e| e.code().to_string());
            drop(sess);
            outcomes.push(failed);
            clock.advance(1_000);
        }
        assert!(
            outcomes.iter().any(|o| o.as_deref() == Some("54P06")),
            "{mode}: the limit never tripped at {} bytes",
            db.storage_bytes("main").unwrap()
        );
        assert!(
            outcomes.iter().any(Option::is_none),
            "{mode}: every commit failed"
        );
        runs.push(outcomes);
        db.close().unwrap();
    }
    assert!(runs[0] == runs[1], "batching moved a 54P06");
    assert!(runs[0] == runs[2], "the deadline flush moved a 54P06");
    let _ = std::fs::remove_dir_all(&dir);
}
