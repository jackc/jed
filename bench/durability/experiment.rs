//! Research only: capture actual SQL commit writes, then replay identical traces through
//! alternative durability protocols. SQL/planning time is deliberately outside the I/O timer.
mod cow;
mod io;
mod wal;

use self::io::{Device, Write};
use crate::blockstore::{BlockStore, MemoryBlockStore};
use crate::executor::Engine;
use crate::pager::Pager;
use crate::{Outcome, Value};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write as _};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PAGE: usize = 8192;

#[derive(Clone, Debug)]
struct Commit {
    body: Vec<Write>,
    meta: Write,
}

impl Commit {
    fn txid(&self) -> u64 {
        u64::from_be_bytes(self.meta.bytes[12..20].try_into().unwrap())
    }
    fn writes(&self) -> Vec<Write> {
        let mut writes = self.body.clone();
        writes.push(self.meta.clone());
        writes
    }
}

struct Capture {
    inner: MemoryBlockStore,
    pending: Vec<Write>,
    commits: Arc<Mutex<Vec<Commit>>>,
}

impl BlockStore for Capture {
    fn read_at(&mut self, offset: u64, len: usize) -> crate::Result<Vec<u8>> {
        self.inner.read_at(offset, len)
    }
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> crate::Result<()> {
        self.inner.write_at(offset, bytes)?;
        self.pending.push(Write {
            offset,
            bytes: bytes.to_vec(),
        });
        Ok(())
    }
    fn sync(&mut self) -> crate::Result<()> {
        if self
            .pending
            .last()
            .is_some_and(|w| w.offset < (PAGE * 2) as u64)
        {
            let meta = self.pending.pop().unwrap();
            self.commits.lock().unwrap().push(Commit {
                body: std::mem::take(&mut self.pending),
                meta,
            });
        }
        Ok(())
    }
    fn size(&mut self) -> crate::Result<u64> {
        self.inner.size()
    }
    fn set_size(&mut self, bytes: u64) -> crate::Result<()> {
        self.inner.set_size(bytes)
    }
}

fn sql(db: &mut Engine, query: &str) -> Outcome {
    crate::execute(db, query).unwrap()
}

fn answer(db: &mut Engine) -> Vec<Vec<Value>> {
    match sql(db, "SELECT id, v, payload FROM t ORDER BY id") {
        Outcome::Query { rows, .. } => rows,
        _ => panic!("expected rows"),
    }
}

struct Trace {
    initial: Vec<u8>,
    commits: Vec<Commit>,
    expected: Vec<Vec<Value>>,
    data_bytes: usize,
}

fn trace(count: usize, batch: usize) -> Trace {
    trace_sized(count, batch, 512, 1)
}

fn trace_sized(count: usize, batch: usize, rows: usize, stride: usize) -> Trace {
    let mut seed = Engine::new();
    sql(
        &mut seed,
        "CREATE TABLE t (id i32 PRIMARY KEY, v i32, payload text)",
    );
    sql(&mut seed, "BEGIN");
    for id in 0..rows {
        sql(
            &mut seed,
            &format!(
                "INSERT INTO t VALUES ({id}, 0, 'abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz')"
            ),
        );
    }
    sql(&mut seed, "COMMIT");
    let initial = seed.to_image(PAGE as u32, 1).unwrap();
    let commits = Arc::new(Mutex::new(Vec::new()));
    let capture = Capture {
        inner: MemoryBlockStore::new(initial.clone()),
        pending: Vec::new(),
        commits: Arc::clone(&commits),
    };
    let pager = Pager::from_store(Box::new(capture)).unwrap();
    let mut db = Engine::open_paged(pager, 1024).unwrap();
    // The byte backing is captured in RAM, but run the FILE commit semantics,
    // including advancing txid and native slot alternation. No file opens use this path.
    db.path = Some("durability-capture".into());
    for txn in 0..count {
        sql(&mut db, "BEGIN");
        for j in 0..batch {
            let id = (txn * batch * stride + j) % rows;
            sql(&mut db, &format!("UPDATE t SET v = v + 1 WHERE id = {id}"));
        }
        sql(&mut db, "COMMIT");
    }
    let expected = answer(&mut db);
    let commits = commits.lock().unwrap().clone();
    assert_eq!(commits.len(), count);
    let data_bytes = commits
        .iter()
        .flat_map(|c| &c.body)
        .map(|w| w.offset as usize + w.bytes.len())
        .max()
        .unwrap_or(initial.len())
        .max(initial.len());
    Trace {
        initial,
        commits,
        expected,
        data_bytes,
    }
}

#[derive(Default, Clone, Copy)]
struct Stats {
    bytes: u64,
    syncs: u64,
    writes: u64,
}

struct Disk {
    file: File,
    stats: Stats,
    delay: Duration,
    full: bool,
}

impl Disk {
    fn create(path: &Path, bytes: usize, initial: &[u8], delay: Duration, full: bool) -> Self {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap();
        // Real zero allocation, not sparse set_len; initialization is outside the timer.
        let zero = vec![0; 1024 * 1024];
        let mut left = bytes;
        while left > 0 {
            let n = left.min(zero.len());
            file.write_all(&zero[..n]).unwrap();
            left -= n;
        }
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(initial).unwrap();
        file.sync_all().unwrap();
        File::open(path.parent().unwrap())
            .unwrap()
            .sync_all()
            .unwrap();
        Self {
            file,
            stats: Stats::default(),
            delay,
            full,
        }
    }
}

impl Device for Disk {
    fn read(&mut self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        self.file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0; len];
        self.file.read_exact(&mut bytes)?;
        Ok(bytes)
    }
    fn write(&mut self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(bytes)?;
        self.stats.bytes += bytes.len() as u64;
        self.stats.writes += 1;
        Ok(())
    }
    fn sync(&mut self) -> std::io::Result<()> {
        if self.full {
            self.file.sync_all()?;
        } else {
            self.file.sync_data()?;
        }
        self.stats.syncs += 1;
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        Ok(())
    }
}

fn apply(dev: &mut impl Device, writes: &[Write]) {
    for w in writes {
        dev.write(w.offset, &w.bytes).unwrap();
    }
}

fn coalesce(commits: &[Commit]) -> Vec<Write> {
    let mut latest = BTreeMap::new();
    for c in commits {
        for w in &c.body {
            latest.insert(w.offset, w.bytes.clone());
        }
    }
    latest
        .into_iter()
        .map(|(offset, bytes)| Write { offset, bytes })
        .collect()
}

fn check_image(dev: &mut impl Device, trace: &Trace) {
    let image = dev.read(0, trace.data_bytes).unwrap();
    let mut db = Engine::from_image(&image).unwrap();
    assert_eq!(answer(&mut db), trace.expected);
}

fn percentile(samples: &mut [u128], pct: usize) -> u128 {
    samples.sort_unstable();
    samples[((samples.len() * pct).div_ceil(100)).saturating_sub(1)]
}

// Included as an ignored core test so all private file-format/SQL APIs remain private.
#[test]
#[ignore = "durable I/O experiment; run mise run bench:durability"]
fn benchmark() {
    let dir = std::env::var("JED_DURABILITY_DIR")
        .expect("set JED_DURABILITY_DIR to a real disk directory");
    let dir = Path::new(&dir);
    std::fs::create_dir_all(dir).unwrap();
    let count: usize = std::env::var("JED_DURABILITY_COMMITS")
        .unwrap_or("1024".into())
        .parse()
        .unwrap();
    let repetitions: usize = std::env::var("JED_DURABILITY_REPEATS")
        .unwrap_or("3".into())
        .parse()
        .unwrap();
    let interval: usize = std::env::var("JED_DURABILITY_CHECKPOINT")
        .unwrap_or("64".into())
        .parse()
        .unwrap();
    let delay_us: u64 = std::env::var("JED_DURABILITY_DELAY_US")
        .unwrap_or("0".into())
        .parse()
        .unwrap();
    let full = std::env::var("JED_DURABILITY_SYNC").unwrap_or_default() == "all";
    let rows: usize = std::env::var("JED_DURABILITY_ROWS")
        .unwrap_or("512".into())
        .parse()
        .unwrap();
    let stride: usize = std::env::var("JED_DURABILITY_STRIDE")
        .unwrap_or("1".into())
        .parse()
        .unwrap();
    assert!(rows > 0 && stride > 0);
    assert!(count > 0 && repetitions > 0 && interval > 0);
    let modes = [
        "cow-two-sync",
        "wal-external",
        "wal-embedded",
        "cow-manifest",
    ];
    for batch in [1, 64] {
        let trace = trace_sized(count, batch, rows, stride);
        let max_frame = trace
            .commits
            .iter()
            .map(|c| wal::encode_frame(c.txid(), &c.writes()).len())
            .max()
            .unwrap();
        let log_capacity = max_frame.checked_mul(interval).unwrap();
        let base = trace.data_bytes.next_multiple_of(4096) as u64;
        let slot_bytes =
            (PAGE + trace.commits.iter().map(|c| c.body.len()).max().unwrap() * 32 + 4096)
                .next_multiple_of(4096);
        for rep in 0..repetitions {
            // Rotate protocol order to reduce systematic warmup/temperature bias.
            for mode_index in 0..modes.len() {
                let mode = modes[(mode_index + rep) % modes.len()];
                let file = dir.join(format!("{}-{batch}-{rep}-{mode}.db", std::process::id()));
                let log_file = file.with_extension("wal");
                let extra = if mode == "wal-embedded" {
                    log_capacity
                } else if mode == "cow-manifest" {
                    slot_bytes * 2
                } else {
                    0
                };
                let mut disk = Disk::create(
                    &file,
                    base as usize + extra,
                    &trace.initial,
                    Duration::from_micros(delay_us),
                    full,
                );
                let mut external = (mode == "wal-external").then(|| {
                    Disk::create(
                        &log_file,
                        log_capacity,
                        &[],
                        Duration::from_micros(delay_us),
                        full,
                    )
                });
                let mut journal =
                    wal::Journal::new(if external.is_some() { 0 } else { base }, log_capacity, 1)
                        .unwrap();
                let mut manifest = cow::ValidatedCow::new(base, slot_bytes, 1).unwrap();
                let mut samples = Vec::new();
                let mut checkpoint_start = 0;
                let mut checkpoint_slot = 0;
                let start = Instant::now();
                for (index, c) in trace.commits.iter().enumerate() {
                    let commit_start = Instant::now();
                    match mode {
                        "cow-two-sync" => wal::checkpoint(&mut disk, &c.body, &c.meta).unwrap(),
                        "cow-manifest" => {
                            manifest
                                .commit(&mut disk, c.txid(), &c.body, &c.meta)
                                .unwrap();
                            if index + 1 == count {
                                manifest.checkpoint(&mut disk).unwrap();
                            }
                        }
                        _ => {
                            let log: &mut dyn Device = match external.as_mut() {
                                Some(d) => d,
                                None => &mut disk,
                            };
                            assert_eq!(journal.append(log, &c.writes()).unwrap(), c.txid());
                            if (index + 1) % interval == 0 || index + 1 == count {
                                let body = coalesce(&trace.commits[checkpoint_start..=index]);
                                // A batch may span an even number of transactions. Alternate by
                                // CHECKPOINT, not txid parity, to preserve its recovery anchor.
                                let checkpoint_meta = Write {
                                    offset: ((checkpoint_slot ^ 1) * PAGE) as u64,
                                    bytes: c.meta.bytes.clone(),
                                };
                                wal::checkpoint(&mut disk, &body, &checkpoint_meta).unwrap();
                                checkpoint_slot ^= 1;
                                journal.reset(c.txid()).unwrap();
                                checkpoint_start = index + 1;
                            }
                        }
                    }
                    samples.push(commit_start.elapsed().as_nanos());
                }
                let elapsed = start.elapsed();
                // Reopen the normal jed image and compare every row, including payloads.
                check_image(&mut disk, &trace);
                let log_stats = external.as_ref().map_or(Stats::default(), |d| d.stats);
                let stats = Stats {
                    bytes: disk.stats.bytes + log_stats.bytes,
                    syncs: disk.stats.syncs + log_stats.syncs,
                    writes: disk.stats.writes + log_stats.writes,
                };
                let p50 = percentile(&mut samples, 50);
                let p95 = percentile(&mut samples, 95);
                let p99 = percentile(&mut samples, 99);
                println!(
                    "DURABILITY,mode={mode},batch={batch},rows={rows},stride={stride},rep={rep},commits={count},checkpoint={interval},delay_us={delay_us},sync={},elapsed_ns={},p50_ns={p50},p95_ns={p95},p99_ns={p99},bytes={},syncs={},writes={},file_bytes={},verified=true",
                    if full { "all" } else { "data" },
                    elapsed.as_nanos(),
                    stats.bytes,
                    stats.syncs,
                    stats.writes,
                    base as usize + extra + if external.is_some() { log_capacity } else { 0 }
                );
                drop(external);
                drop(disk);
                std::fs::remove_file(file).unwrap();
                if mode == "wal-external" {
                    std::fs::remove_file(log_file).unwrap();
                }
            }
        }
    }
}

#[test]
fn sql_trace_replays_with_production_page_reuse() {
    let trace = trace(24, 1);
    let mut image = trace.initial.clone();
    image.resize(trace.data_bytes, 0);
    let mut reused = false;
    let mut seen = std::collections::BTreeSet::new();
    for c in &trace.commits {
        for w in c.writes() {
            if w.offset >= (2 * PAGE) as u64 && !seen.insert(w.offset) {
                reused = true;
            }
            image[w.offset as usize..w.offset as usize + w.bytes.len()].copy_from_slice(&w.bytes);
        }
    }
    assert!(reused, "exercise the actual allocator's reuse path");
    let mut db = Engine::from_image(&image).unwrap();
    assert_eq!(answer(&mut db), trace.expected);
}

#[derive(Clone)]
struct Ram(Vec<u8>);
impl Device for Ram {
    fn read(&mut self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        self.0
            .get(offset as usize..offset as usize + len)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| std::io::ErrorKind::UnexpectedEof.into())
    }
    fn write(&mut self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        self.0[offset as usize..offset as usize + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }
    fn sync(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// Select just the checksum-protected meta: loading its body before WAL replay is unsafe.
fn baseline(dev: &mut impl Device) -> (u64, Write) {
    (0..2)
        .filter_map(|slot| {
            let bytes = dev.read((slot * PAGE) as u64, PAGE).unwrap();
            let crc = u32::from_be_bytes(bytes[32..36].try_into().unwrap());
            if crate::format::meta_crc(&bytes) != crc {
                return None;
            }
            let txid = u64::from_be_bytes(bytes[12..20].try_into().unwrap());
            Some((
                txid,
                Write {
                    offset: (slot * PAGE) as u64,
                    bytes,
                },
            ))
        })
        .max_by_key(|(seq, _)| *seq)
        .unwrap()
}

#[test]
fn real_sql_recovers_wal_before_and_during_second_checkpoint() {
    let trace = trace(8, 1);
    let mut initial = trace.initial.clone();
    initial.resize(trace.data_bytes, 0);
    let mut db = Ram(initial);
    // First even-sized checkpoint: seq5, slot1. The next must use slot0, not seq9's parity.
    let first = Write {
        offset: PAGE as u64,
        bytes: trace.commits[3].meta.bytes.clone(),
    };
    wal::checkpoint(&mut db, &coalesce(&trace.commits[..4]), &first).unwrap();
    let mut log = Ram(vec![0; 1024 * 1024]);
    let mut journal = wal::Journal::new(0, log.0.len(), 5).unwrap();
    for c in &trace.commits[4..] {
        journal.append(&mut log, &c.writes()).unwrap();
    }
    let body = coalesce(&trace.commits[4..]);
    let final_meta = Write {
        offset: 0,
        bytes: trace.commits[7].meta.bytes.clone(),
    };
    // Lost/reordered checkpoint body sectors and a torn root publication. The WAL was
    // acknowledged first, so recovery must restore every final row in all these images.
    for body_prefix in 0..=body.len() {
        for meta_prefix in [0, 1, 12, 20, 32, 35, 36, 512, PAGE] {
            if meta_prefix > 0 && body_prefix < body.len() {
                continue;
            } // first checkpoint barrier
            let mut crash = db.clone();
            apply(&mut crash, &body[..body_prefix]);
            crash
                .write(final_meta.offset, &final_meta.bytes[..meta_prefix])
                .unwrap();
            let (checkpoint_seq, _) = baseline(&mut crash);
            let recovery = wal::recover(&log.0, checkpoint_seq);
            for (_, writes) in recovery.frames {
                apply(&mut crash, &writes);
            }
            check_image(&mut crash, &trace);
        }
    }
}

#[test]
fn real_sql_recovers_manifests_before_native_open() {
    let trace = trace(24, 1);
    let base = trace.data_bytes.next_multiple_of(4096);
    let slot_bytes = 16384;
    let mut bytes = trace.initial.clone();
    bytes.resize(base + slot_bytes * 2, 0);
    let mut dev = Ram(bytes);
    let mut writer = cow::ValidatedCow::new(base as u64, slot_bytes, 1).unwrap();
    for c in &trace.commits {
        writer.commit(&mut dev, c.txid(), &c.body, &c.meta).unwrap();
    }
    let (seq, meta) = baseline(&mut dev);
    let selected =
        cow::ValidatedCow::recover(&mut dev, base as u64, slot_bytes, seq, &meta).unwrap();
    assert_eq!(selected.seq, trace.commits.last().unwrap().txid());
    dev.write(selected.meta.offset, &selected.meta.bytes)
        .unwrap();
    check_image(&mut dev, &trace);
}

#[test]
fn simply_removing_body_sync_can_publish_an_unreadable_sql_database() {
    let trace = trace(1, 1);
    let mut bytes = trace.initial.clone();
    bytes.resize(trace.data_bytes, 0);
    // Valid new root persisted; fresh body did not. Meta checksum alone still accepts it.
    let meta = &trace.commits[0].meta;
    // Simulate the former unvalidated protocol by clearing v33's descriptor. This negative
    // control intentionally bypasses the now-production one-barrier recovery validation.
    let mut unvalidated = meta.bytes.clone();
    unvalidated[36..].fill(0);
    let crc = crate::format::meta_crc(&unvalidated);
    unvalidated[32..36].copy_from_slice(&crc.to_be_bytes());
    bytes[meta.offset as usize..meta.offset as usize + PAGE].copy_from_slice(&unvalidated);
    let result =
        Engine::from_image(&bytes).and_then(|mut db| crate::execute(&mut db, "SELECT * FROM t"));
    assert!(
        result.is_err(),
        "negative control must exhibit missing-body corruption"
    );
}

#[test]
fn real_sql_cow_fallback_survives_pending_body_without_descriptor() {
    struct Recording {
        inner: Ram,
        writes: Vec<Write>,
    }
    impl Device for Recording {
        fn read(&mut self, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
            self.inner.read(offset, len)
        }
        fn write(&mut self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
            self.writes.push(Write {
                offset,
                bytes: bytes.to_vec(),
            });
            self.inner.write(offset, bytes)
        }
        fn sync(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // Includes the real allocator's free-list rebuild/reuse, rather than a
    // hand-built append-only page trace. Test both cheap and larger commits.
    for batch in [1, 64] {
        let trace = trace(32, batch);
        let base = trace.data_bytes.next_multiple_of(4096);
        let slot_bytes = 16384;
        let mut initial = trace.initial.clone();
        initial.resize(base + 2 * slot_bytes, 0);
        let mut normal = Ram(initial.clone());
        let mut device = Recording {
            inner: Ram(initial),
            writes: Vec::new(),
        };
        let mut writer = cow::ValidatedCow::new(base as u64, slot_bytes, 1).unwrap();

        for commit in &trace.commits {
            let mut prior = Engine::from_image(&normal.0[..trace.data_bytes]).unwrap();
            let expected_prior = answer(&mut prior);
            let mut crash = device.inner.clone();
            device.writes.clear();
            writer
                .commit(&mut device, commit.txid(), &commit.body, &commit.meta)
                .unwrap();

            // Keep any automatic checkpoint (which completed before reuse)
            // and every pending body write. Lose just the new descriptor.
            // This is the dangerous ordering if ordinary meta-only fallback
            // or the allocator's previous-root protection is insufficient.
            for write in &device.writes {
                if write.offset < base as u64 {
                    crash.write(write.offset, &write.bytes).unwrap();
                }
            }
            let (base_seq, base_meta) = baseline(&mut crash);
            let recovered = cow::ValidatedCow::recover(
                &mut crash,
                base as u64,
                slot_bytes,
                base_seq,
                &base_meta,
            )
            .unwrap();
            assert_eq!(recovered.seq, commit.txid() - 1);
            crash
                .write(recovered.meta.offset, &recovered.meta.bytes)
                .unwrap();
            let mut reopened = Engine::from_image(&crash.0[..trace.data_bytes]).unwrap();
            assert_eq!(answer(&mut reopened), expected_prior);
            apply(&mut normal, &commit.writes());
        }
    }
}
