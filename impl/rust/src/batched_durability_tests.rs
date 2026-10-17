//! Batched durability under synchronous = off (spec/design/validated-cow.md §4): host-API and
//! byte-level assertions the corpus cannot express — barrier counts, meta-slot placement, the batch
//! cap, close's flush, and byte identity between the background and inline cap flush. Crash recovery
//! under relaxed mode is the simulator's (spec/design/simulation-testing.md, the relaxed profile).
//! Mirrors the Go batched_durability_test.go.
use super::*;
use crate::blockstore::{BlockStore, ClosedBlockStore};
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

fn relaxed_db(path: &std::path::Path, page_size: u32, inline: bool) -> Database {
    let db = Database::create(CreateOptions {
        path: Some(path.to_path_buf()),
        page_size,
        synchronous: Synchronous::Off,
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
    let cap = crate::format::manifest_capacities(256).0 as u32;
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
