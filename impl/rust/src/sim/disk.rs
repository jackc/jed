//! The deterministic-simulation filesystem (spec/design/simulation-testing.md §6, slice S4): named
//! files, each a simulated disk, and a directory whose entries become durable only at a directory
//! barrier. Durability, crashes, and I/O errors are decided by a seeded, content-keyed function, so a
//! whole multi-commit history replays exactly from its seed and a crash image is identical in every
//! core. A line-for-line port of the Go reference (`impl/go/sim_disk_test.go`).
//!
//! A file's state (§6.1): `durable` bytes survive a power loss unconditionally; `pending` holds, per
//! byte offset, every version written since that offset was last covered by a barrier; `cache` is
//! what reads see (durable overlaid with the newest pending version). `sync` and a growing
//! `set_size` are barriers. The filesystem implements the engine's [`FileHost`], so create's and
//! compaction's temp files, renames, and directory barriers, and the open of a file attachment, go
//! through it too. Spill scratch files go through a second seam, the [`SpillHost`]: the simulated one
//! keeps them in memory, outside the filesystem, and `spill_error` fails their operations.
//!
//! Every fault decision is [`decide`]`(seed, lane, step, offset, version)` (§6.2): keyed by WHAT was
//! written WHERE, never by a write's position in the op log, so cores whose write order differs
//! within a commit still produce the same post-crash bytes.

use crate::blockstore::BlockStore;
use crate::error::{EngineError, Result, SqlState};
use crate::filehost::{FileHost, WriteAt};
use crate::spillhost::{SpillFile, SpillHost};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// Decision lanes keep independent choices from correlating (§6.2). The values are part of the
// cross-core contract: every core's runner uses the same lane for the same decision.
const LANE_SURVIVE: u64 = 1; // power loss: does anything land at this offset?
const LANE_VERSION: u64 = 2; // power loss: which pending version lands
const LANE_TEAR: u64 = 3; // power loss: is the landing version torn?
const LANE_SECTOR: u64 = 4; // power loss: per-sector old/new choice of a torn version
const LANE_IO_WRITE: u64 = 5; // io_error op=write: does this write fail?
const LANE_ISSUE: u64 = 6; // process_crash: was this write issued before the process died?
const LANE_LAND: u64 = 7; // io_error op=sync: does this pending write land or vanish (fsyncgate)?
pub(crate) const LANE_SESSION: u64 = 8; // a session's entropy-seam seed
const LANE_READ: u64 = 9; // read_error: do reads of this offset fail during the step?
const LANE_DIR: u64 = 10; // power loss: how many pending directory changes survive (% changes+1)
const LANE_SPILL: u64 = 11; // spill_error: does this spill-file operation fail?
const LANE_PAGE: u64 = 12; // corrupt: which page a flip lands in
const LANE_BIT: u64 = 13; // corrupt: which bit of that page it flips
pub(crate) const LANE_KEY_SEED: u64 = 14; // an encrypted trace's process key-entropy seed (S13)
pub(crate) const LANE_REPLAY_PAGE: u64 = 15; // replay: which candidate page
pub(crate) const LANE_REPLAY_VERSION: u64 = 16; // replay: which older version of it

/// How many durable images of one page index an archive keeps (§6.1, slice S13).
const ARCHIVE_DEPTH: usize = 32;
/// The first body page of an encrypted file: the archive keeps only body pages.
const ARCHIVE_FIRST_PAGE: u64 = 4;

const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

const FNV_OFFSET: u64 = 14695981039346656037;
const FNV_PRIME: u64 = 1099511628211;

/// Fold `bytes` into a running FNV-1a 64 state.
pub(crate) fn fnv_fold(mut h: u64, bytes: &[u8]) -> u64 {
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// FNV-1a 64 of `bytes`.
pub(crate) fn fnv(bytes: &[u8]) -> u64 {
    fnv_fold(FNV_OFFSET, bytes)
}

/// The splitmix64 finalizer (spec/encoding/prng.toml's mixer).
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The content-keyed decision function (simulation-testing.md §6.2).
pub(crate) fn decide(seed: u64, lane: u64, step: u64, offset: u64, version: u64) -> u64 {
    let mut h = seed;
    for w in [lane, step, offset, version] {
        h = mix((h ^ w).wrapping_add(GAMMA));
    }
    h
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FaultKind {
    PowerLoss,
    ProcessCrash,
    IoWrite,
    IoSync,
    /// Per-core: which reads happen depends on buffer-pool policy.
    ReadError,
    /// Per-core: when the file grows depends on preallocation policy.
    Enospc,
    /// Per-core: what a core spills, and in which operations, is its own codec's business.
    SpillError,
    /// A page-service connection ends at the step's nth frame (S8): the network's, not the disk's.
    Disconnect,
}

/// One armed fault (simulation-testing.md §6.3). Permille fields are 0..1000.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fault {
    pub(crate) kind: FaultKind,
    pub(crate) survive: i64, // power_loss
    pub(crate) tear: i64,    // power_loss
    pub(crate) issue: i64,   // process_crash
    pub(crate) p: i64,       // io_error op=write, read_error, spill_error
    /// power_loss/process_crash: the barrier it fires at (default 1); io_error op=sync: the sync
    pub(crate) nth: i64,
}

impl Fault {
    pub(crate) fn new(kind: FaultKind) -> Fault {
        Fault {
            kind,
            survive: 0,
            tear: 0,
            issue: 0,
            p: 0,
            nth: 0,
        }
    }
}

/// A permille bound for `decide(..) % 1000 < p`, converted as the Go reference does (`uint64(p)`):
/// a negative permille wraps to a huge value. Mirrored exactly so a malformed trace behaves alike.
fn permille(p: i64) -> u64 {
    p as u64
}

/// The database file's name in the simulated filesystem — the path the engine is given.
pub(crate) const MAIN_FILE: &str = "sim.jed";

/// Keeps two files' fault decisions independent (§6.2): the database file's key is 0, so its
/// decisions are unchanged by the existence of other files; another file's is its name's FNV-1a 32
/// plus one, shifted above any offset (wrapping, as the Go reference's `uint64` arithmetic does).
fn file_key(name: &str) -> u64 {
    if name == MAIN_FILE {
        return 0;
    }
    let mut h: u32 = 2166136261;
    for b in name.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(16777619);
    }
    (h as u64).wrapping_add(1) << 40
}

/// One simulated file. Files live in the filesystem's arena and are named by their index there, so
/// a file keeps its identity across renames and stays reachable from a pending directory change.
struct SimFile {
    /// Folded into fault decisions' offsets; 0 for the database file.
    key: u64,
    durable: Vec<u8>,
    cache: Vec<u8>,
    pending: BTreeMap<u64, Vec<Vec<u8>>>,
    /// The writes an armed process_crash decided never reached the kernel; process death removes them.
    unissued: Vec<(u64, usize)>,

    // Per-step state, reset by begin_step.
    /// This file's own barrier completed with every directory change durable (the floor, §8.2).
    barrier_ok: bool,
    /// How many writes reached it, and which pages.
    writes: usize,
    pages: BTreeSet<u64>,

    /// Per page index ≥ 4, the distinct images a successful barrier made durable there, the most
    /// recently archived last (§6.1, slice S13). It follows the file's path across a rename onto it
    /// and a restore.
    archive: BTreeMap<u64, Vec<Vec<u8>>>,
}

impl SimFile {
    /// Add one durable image of page `pg`, unless the archive already holds it, keeping the
    /// [`ARCHIVE_DEPTH`] most recent.
    fn archive_image(&mut self, pg: u64, image: &[u8]) {
        let vs = self.archive.entry(pg).or_default();
        if vs.iter().any(|v| v == image) {
            return;
        }
        vs.push(image.to_vec());
        if vs.len() > ARCHIVE_DEPTH {
            let cut = vs.len() - ARCHIVE_DEPTH;
            vs.drain(..cut);
        }
    }

    /// Take `old`'s archive — the file whose path this one now takes — ahead of this file's own.
    fn inherit(&mut self, old: &BTreeMap<u64, Vec<Vec<u8>>>) {
        if old.is_empty() {
            return;
        }
        let own = std::mem::take(&mut self.archive);
        for pages in [old, &own] {
            for (pg, vs) in pages {
                for v in vs {
                    self.archive_image(*pg, v);
                }
            }
        }
    }
}

/// One directory change: create a file, rename one over another, or remove one.
#[derive(Clone)]
struct DirOp {
    kind: u8, // b'c' create, b'r' rename, b'd' remove
    from: String,
    to: String,
    file: usize,
}

fn apply_dir_op(dir: &mut BTreeMap<String, usize>, op: &DirOp) {
    match op.kind {
        b'c' => {
            dir.insert(op.to.clone(), op.file);
        }
        b'r' => {
            dir.insert(op.to.clone(), op.file);
            dir.remove(&op.from);
        }
        _ => {
            dir.remove(&op.from);
        }
    }
}

/// The simulated filesystem. It outlives every Database opened over it: a crash discards the
/// process but keeps the disk, as a real filesystem would. The per-step fault state lives here,
/// because a fault arms a step, not a file.
pub(crate) struct SimFs {
    seed: u64,
    sector: usize,
    page_size: u64,

    arena: Vec<SimFile>,
    /// The kernel's view of the directory.
    files: BTreeMap<String, usize>,
    /// The directory as of the last directory barrier.
    durable: BTreeMap<String, usize>,
    /// Directory changes since that barrier, in order.
    dir_ops: Vec<DirOp>,

    // Per-step state, reset by begin_step.
    step: u64,
    fault: Option<Fault>,
    /// "" | "barrier" | "write" | "sync" | "read" | "enospc" | "spill"
    pub(crate) fired: &'static str,
    /// The process died mid-step; every later call fails.
    dead: bool,
    /// File syncs this step (io_error op=sync nth).
    syncs: i64,
    /// File and directory barriers this step (power_loss/process_crash nth).
    barriers: i64,
    /// The runner's own oracle reads are in progress: read and spill faults do not apply.
    pub(crate) oracle: bool,
    /// Spill-file operations this step (spill_error decisions).
    spill_ops: u64,

    /// FNV-1a fold of every mutating call, for the self-determinism check (§8.5).
    pub(crate) ops: u64,

    /// Whether each file keeps its archive of durable page images (§6.1, slice S13): an encrypted
    /// trace's replay steps draw on it.
    pub(crate) archiving: bool,
}

fn injected(what: &str) -> EngineError {
    EngineError::new(SqlState::IoError, format!("I/O error: simulated {what}"))
}

impl SimFs {
    /// An empty simulated filesystem; the runner creates the database in it (S4).
    pub(crate) fn new(seed: u64, sector: usize, page_size: u32) -> SimFs {
        SimFs {
            seed,
            sector,
            page_size: page_size as u64,
            arena: Vec::new(),
            files: BTreeMap::new(),
            durable: BTreeMap::new(),
            dir_ops: Vec::new(),
            step: 0,
            fault: None,
            fired: "",
            dead: false,
            syncs: 0,
            barriers: 0,
            oracle: false,
            spill_ops: 0,
            ops: FNV_OFFSET,
            archiving: false,
        }
    }

    fn new_file(&mut self, name: &str) -> usize {
        self.arena.push(SimFile {
            key: file_key(name),
            durable: Vec::new(),
            cache: Vec::new(),
            pending: BTreeMap::new(),
            unissued: Vec::new(),
            barrier_ok: false,
            writes: 0,
            pages: BTreeSet::new(),
            archive: BTreeMap::new(),
        });
        self.arena.len() - 1
    }

    /// Whether a fault is armed for the current step.
    /// Whether the process died mid-step (every later call fails).
    pub(crate) fn dead(&self) -> bool {
        self.dead
    }

    pub(crate) fn armed(&self) -> bool {
        self.fault.is_some()
    }

    /// The file named `name`, or `None` if there is none (a crash may have lost it).
    pub(crate) fn file(&self, name: &str) -> Option<usize> {
        self.files.get(name).copied()
    }

    /// A copy of file `name`'s cache — what a new process would open — or `None` if it is gone.
    pub(crate) fn image(&self, name: &str) -> Option<Vec<u8>> {
        self.file(name).map(|id| self.arena[id].cache.clone())
    }

    /// Whether file `id`'s own barrier completed this step with every directory change durable.
    pub(crate) fn barrier_ok(&self, id: usize) -> bool {
        self.arena[id].barrier_ok
    }

    /// Whether any write to file `id` is still pending, not yet covered by a barrier.
    pub(crate) fn has_pending(&self, id: usize) -> bool {
        !self.arena[id].pending.is_empty()
    }

    /// Whether file `name`'s directory entry is durable: a power loss would keep the file a reader
    /// of that name sees now, not revert a pending create or rename (§6.1).
    pub(crate) fn entry_durable(&self, name: &str) -> bool {
        self.files.get(name) == self.durable.get(name)
    }

    /// Whether a meta-slot write of file `id` is still pending, not yet covered by a barrier: the
    /// disk fact the relaxed-durability oracle checks an engine's flush claim against (§8.2).
    pub(crate) fn meta_pending(&self, id: usize) -> bool {
        let file = &self.arena[id];
        if file.cache.len() < 12 {
            return false;
        }
        let ps = u32::from_be_bytes(file.cache[8..12].try_into().unwrap()) as u64;
        file.pending.contains_key(&0) || file.pending.contains_key(&ps)
    }

    /// How many writes reached file `id` this step.
    pub(crate) fn file_writes(&self, id: usize) -> usize {
        self.arena[id].writes
    }

    /// File `id`'s length in bytes, as its process sees it.
    pub(crate) fn file_len(&self, id: usize) -> u64 {
        self.arena[id].cache.len() as u64
    }

    /// The distinct pages of file `id` this step wrote.
    pub(crate) fn written_pages(&self, id: usize) -> &BTreeSet<u64> {
        &self.arena[id].pages
    }

    /// Reset the per-step state and arm `f` (`None` for an unfaulted step).
    pub(crate) fn begin_step(&mut self, step: u64, f: Option<Fault>) {
        self.step = step;
        self.fault = f;
        self.fired = "";
        self.dead = false;
        self.syncs = 0;
        self.barriers = 0;
        self.spill_ops = 0;
        for id in self.all_files() {
            let file = &mut self.arena[id];
            file.unissued.clear();
            file.barrier_ok = false;
            file.writes = 0;
            file.pages.clear();
        }
    }

    /// Every file the directory, its durable state, or a pending change refers to, each once, in a
    /// deterministic order: the directory by name, then its durable state by name, then the changes.
    fn all_files(&self) -> Vec<usize> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let ids = self
            .files
            .values()
            .chain(self.durable.values())
            .copied()
            .chain(self.dir_ops.iter().map(|op| op.file));
        for id in ids {
            if seen.insert(id) {
                out.push(id);
            }
        }
        out
    }

    fn fold(&mut self, kind: u8, key: u64, off: u64, p: &[u8]) {
        let mut buf = [0u8; 25];
        buf[0..8].copy_from_slice(&self.ops.to_be_bytes());
        buf[8] = kind;
        buf[9..17].copy_from_slice(&key.to_be_bytes());
        buf[17..25].copy_from_slice(&off.to_be_bytes());
        let h = fnv_fold(FNV_OFFSET, &buf);
        self.ops = fnv_fold(h, p);
    }

    /// Count one file or directory barrier and report whether an armed power loss or process crash
    /// fires at it: the barrier never completes, because the machine or process is gone.
    fn barrier(&mut self) -> bool {
        self.barriers += 1;
        let Some(f) = self.fault else {
            return false;
        };
        if !self.fired.is_empty()
            || (f.kind != FaultKind::PowerLoss && f.kind != FaultKind::ProcessCrash)
        {
            return false;
        }
        let nth = if f.nth <= 0 { 1 } else { f.nth };
        if self.barriers != nth {
            return false;
        }
        self.fired = "barrier";
        self.dead = true;
        true
    }

    fn decide_at(&self, id: usize, lane: u64, step: u64, off: u64, version: u64) -> u64 {
        decide(
            self.seed,
            lane,
            step,
            off.wrapping_add(self.arena[id].key),
            version,
        )
    }

    fn read_at(&mut self, id: usize, off: u64, len: usize) -> Result<Vec<u8>> {
        if let Some(f) = self.fault
            && f.kind == FaultKind::ReadError
            && !self.dead
            && !self.oracle
            && self.decide_at(id, LANE_READ, self.step, off, 0) % 1000 < permille(f.p)
        {
            self.fired = "read";
            return Err(injected("read error"));
        }
        let cache = &self.arena[id].cache;
        match off.checked_add(len as u64) {
            Some(end) if end <= cache.len() as u64 => {
                Ok(cache[off as usize..end as usize].to_vec())
            }
            _ => Err(EngineError::new(
                SqlState::IoError,
                "I/O error: short read past the end of the simulated disk",
            )),
        }
    }

    fn write_at(&mut self, id: usize, off: u64, p: &[u8]) -> Result<()> {
        let key = self.arena[id].key;
        self.fold(b'w', key, off, p);
        if self.dead {
            return Err(injected("write after process death"));
        }
        let version = self.arena[id].pending.get(&off).map_or(0, |vs| vs.len()) as u64;
        if let Some(f) = self.fault {
            match f.kind {
                FaultKind::ProcessCrash => {
                    // The live process sees all its own writes; only at its death does a write that
                    // was never issued to the kernel disappear (process_death).
                    if self.decide_at(id, LANE_ISSUE, self.step, off, version) % 1000
                        >= permille(f.issue)
                    {
                        self.arena[id].unissued.push((off, version as usize));
                    }
                }
                FaultKind::IoWrite => {
                    if self.fired.is_empty()
                        && self.decide_at(id, LANE_IO_WRITE, self.step, off, version) % 1000
                            < permille(f.p)
                    {
                        self.fired = "write";
                        return Err(injected("write error"));
                    }
                }
                _ => {}
            }
        }
        let ps = self.page_size;
        let file = &mut self.arena[id];
        file.writes += 1;
        let mut pg = off / ps;
        while pg * ps < off + p.len() as u64 {
            file.pages.insert(pg);
            pg += 1;
        }
        let start = off as usize;
        let end = start + p.len();
        if end > file.cache.len() {
            file.cache.resize(end, 0);
        }
        file.cache[start..end].copy_from_slice(p);
        file.pending.entry(off).or_default().push(p.to_vec());
        Ok(())
    }

    /// Make every pending write of file `id` durable: a successful barrier.
    fn flush(&mut self, id: usize) {
        let (ps, archiving) = (self.page_size, self.archiving);
        let file = &mut self.arena[id];
        let pending = std::mem::take(&mut file.pending);
        for (off, vs) in &pending {
            let newest = vs.last().expect("a pending offset holds a version");
            put_durable(&mut file.durable, *off, newest);
            // Archive each whole page of a write that just became durable (§6.1).
            if archiving && off % ps == 0 {
                let mut k = 0u64;
                while (k + 1) * ps <= newest.len() as u64 {
                    let pg = off / ps + k;
                    if pg >= ARCHIVE_FIRST_PAGE {
                        file.archive_image(pg, &newest[(k * ps) as usize..((k + 1) * ps) as usize]);
                    }
                    k += 1;
                }
            }
        }
        if file.durable.len() < file.cache.len() {
            let n = file.cache.len();
            file.durable.resize(n, 0);
        }
        file.unissued.clear(); // reaching a barrier means every earlier write was issued
        // Only a database file's own barrier, with its directory entry durable, makes the
        // generation it holds durable (the floor, §8.2); a compaction's temp file syncing proves
        // nothing about it.
        if self.dir_ops.is_empty() {
            file.barrier_ok = true;
        }
    }

    fn sync(&mut self, id: usize) -> Result<()> {
        let key = self.arena[id].key;
        self.fold(b's', key, 0, &[]);
        if self.dead {
            return Err(injected("sync after process death"));
        }
        self.syncs += 1;
        if self.barrier() {
            return Err(injected("crash at a durability barrier"));
        }
        if let Some(f) = self.fault
            && f.kind == FaultKind::IoSync
            && self.fired.is_empty()
            && self.syncs == f.nth
        {
            self.fired = "sync";
            self.fsyncgate(id);
            return Err(injected("sync error"));
        }
        self.flush(id);
        Ok(())
    }

    /// A failed fsync on Linux: each pending write independently became durable or was dropped from
    /// the cache, and the error is reported once (§6.3).
    fn fsyncgate(&mut self, id: usize) {
        let pending = std::mem::take(&mut self.arena[id].pending);
        for (off, vs) in &pending {
            let newest = vs.last().expect("a pending offset holds a version");
            let land =
                self.decide_at(id, LANE_LAND, self.step, *off, (vs.len() - 1) as u64) % 2 == 0;
            let file = &mut self.arena[id];
            if land {
                put_durable(&mut file.durable, *off, newest);
                continue;
            }
            let start = *off as usize;
            for i in start..start + newest.len() {
                file.cache[i] = if i < file.durable.len() {
                    file.durable[i]
                } else {
                    0
                };
            }
        }
    }

    fn size(&self, id: usize) -> u64 {
        self.arena[id].cache.len() as u64
    }

    /// Grow durably (zero blocks + a full fsync, which also flushes every pending write — hosts.md
    /// §2.1) or truncate. It is never a fault point for a crash (simulation-testing.md §6.3).
    fn set_size(&mut self, id: usize, n: u64) -> Result<()> {
        let key = self.arena[id].key;
        self.fold(b'z', key, n, &[]);
        if self.dead {
            return Err(injected("set_size after process death"));
        }
        let cur = self.arena[id].cache.len() as u64;
        if let Some(f) = self.fault
            && f.kind == FaultKind::Enospc
            && n > cur
            && self.fired.is_empty()
        {
            self.fired = "enospc";
            return Err(injected("no space left on device"));
        }
        if n > cur {
            self.arena[id].cache.resize(n as usize, 0);
            self.flush(id);
        } else if n < cur {
            let file = &mut self.arena[id];
            file.cache.truncate(n as usize);
            if file.durable.len() as u64 > n {
                file.durable.truncate(n as usize);
            }
            file.pending.retain(|off, _| *off < n);
        }
        Ok(())
    }

    /// Replace file `id`'s contents with what survives a power cut during `step` (§6.3): for each
    /// pending offset, nothing lands or one pending version (any, not only the newest) lands,
    /// possibly torn sector by sector against the durable bytes. The cache is discarded.
    fn file_power_loss(&mut self, id: usize, step: u64, survive: i64, tear: i64) {
        let sector = self.sector;
        let pending = std::mem::take(&mut self.arena[id].pending);
        for (off, vs) in &pending {
            let off = *off;
            if self.decide_at(id, LANE_SURVIVE, step, off, 0) % 1000 >= permille(survive) {
                continue;
            }
            let idx = self.decide_at(id, LANE_VERSION, step, off, 0) % vs.len() as u64;
            let mut data = vs[idx as usize].clone();
            if self.decide_at(id, LANE_TEAR, step, off, idx) % 1000 < permille(tear) {
                let mut s = 0usize;
                while s < data.len() {
                    let new_bytes =
                        self.decide_at(id, LANE_SECTOR, step, off.wrapping_add(s as u64), idx) & 1
                            == 1;
                    if !new_bytes {
                        let durable = &self.arena[id].durable;
                        let mut i = s;
                        while i < s + sector && i < data.len() {
                            let at = off as usize + i;
                            data[i] = if at < durable.len() { durable[at] } else { 0 };
                            i += 1;
                        }
                    }
                    s += sector;
                }
            }
            put_durable(&mut self.arena[id].durable, off, &data);
        }
        let file = &mut self.arena[id];
        file.cache = file.durable.clone();
    }

    /// A power cut to the whole filesystem: every file loses what its own decisions say, and the
    /// directory keeps its durable entries plus an ordered prefix of the pending changes — a
    /// journaling filesystem applies metadata changes in order (§6.3).
    pub(crate) fn power_loss(&mut self, step: u64, survive: i64, tear: i64) {
        for id in self.all_files() {
            self.file_power_loss(id, step, survive, tear);
        }
        let keep =
            (decide(self.seed, LANE_DIR, step, 0, 0) % (self.dir_ops.len() as u64 + 1)) as usize;
        let mut dir = self.durable.clone();
        for op in &self.dir_ops[..keep] {
            apply_dir_op(&mut dir, op);
        }
        self.durable = dir.clone();
        self.files = dir;
        self.dir_ops.clear();
    }

    /// End the process: the kernel cache and directory survive, every issued write still pending
    /// (not durable), so a later power loss can still drop it (§6.3).
    pub(crate) fn process_death(&mut self) {
        for id in self.all_files() {
            self.file_process_death(id);
        }
        self.fault = None;
        self.dead = false;
    }

    fn file_process_death(&mut self, id: usize) {
        // Undo the never-issued writes, newest first so each version index stays valid. A barrier that
        // completed after such a write already made it durable — the process could not have reached
        // that barrier without issuing it — so only still-pending versions are removed.
        let file = &mut self.arena[id];
        let unissued = std::mem::take(&mut file.unissued);
        for &(off, version) in unissued.iter().rev() {
            let Some(vs) = file.pending.get_mut(&off) else {
                continue;
            };
            if version >= vs.len() {
                continue;
            }
            let size = vs[version].len();
            vs.remove(version);
            let newest = vs.last().cloned();
            if vs.is_empty() {
                file.pending.remove(&off);
            }
            for j in 0..size {
                let at = off as usize + j;
                file.cache[at] = match &newest {
                    Some(v) => v[j],
                    None if at < file.durable.len() => file.durable[at],
                    None => 0,
                };
            }
        }
    }

    // --- the file host operations (crate::filehost), under the filesystem lock ---

    /// Create `name` as a new, empty file (replacing any entry of that name): a pending directory
    /// change until the next directory barrier.
    fn create(&mut self, name: &str) -> Result<usize> {
        if self.dead {
            return Err(injected("create after process death"));
        }
        let id = self.new_file(name);
        self.files.insert(name.to_string(), id);
        self.dir_ops.push(DirOp {
            kind: b'c',
            from: String::new(),
            to: name.to_string(),
            file: id,
        });
        let key = self.arena[id].key;
        self.fold(b'c', key, 0, &[]);
        Ok(id)
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        if self.dead {
            return Err(injected("rename after process death"));
        }
        let Some(id) = self.files.get(from).copied() else {
            return Err(EngineError::new(
                SqlState::IoError,
                format!("I/O error: rename of a missing file {from}"),
            ));
        };
        if let Some(old) = self.files.get(to).copied()
            && old != id
        {
            let archive = self.arena[old].archive.clone();
            self.arena[id].inherit(&archive);
        }
        self.files.insert(to.to_string(), id);
        self.files.remove(from);
        self.dir_ops.push(DirOp {
            kind: b'r',
            from: from.to_string(),
            to: to.to_string(),
            file: id,
        });
        let key = self.arena[id].key;
        self.fold(b'r', key, 0, &[]);
        Ok(())
    }

    fn remove(&mut self, name: &str) {
        if self.dead {
            return;
        }
        if let Some(id) = self.files.remove(name) {
            self.dir_ops.push(DirOp {
                kind: b'd',
                from: name.to_string(),
                to: String::new(),
                file: id,
            });
            let key = self.arena[id].key;
            self.fold(b'd', key, 0, &[]);
        }
    }

    /// A directory barrier. It is a fault point only when it has something to make durable: one
    /// with no pending change cannot lose anything, and counting it would shift every later
    /// barrier's number.
    fn sync_dir(&mut self) {
        self.fold(b'D', 0, 0, &[]);
        if self.dead || self.dir_ops.is_empty() || self.barrier() {
            return;
        }
        self.durable = self.files.clone();
        self.dir_ops.clear();
    }

    fn open(&self, name: &str) -> Result<usize> {
        if self.dead {
            return Err(injected("open after process death"));
        }
        self.files.get(name).copied().ok_or_else(|| {
            EngineError::new(
                SqlState::UndefinedFile,
                format!("database file does not exist: {name}"),
            )
        })
    }

    fn exists(&self, name: &str) -> Result<bool> {
        if self.dead {
            return Err(injected("stat after process death"));
        }
        Ok(self.files.contains_key(name))
    }

    // --- the spill host (crate::spillhost): scratch files in memory, whose operations can fail ---

    /// Whether the step's armed spill_error fails the next spill operation: each operation is decided
    /// by its ordinal in the step (lane 11). Which operations a core performs is its own business, so
    /// the fault is per-core (§6.3).
    fn spill_fault(&mut self) -> std::result::Result<(), String> {
        if self.dead {
            return Err("I/O error: simulated spill after process death".to_string());
        }
        let n = self.spill_ops;
        self.spill_ops += 1;
        if let Some(f) = self.fault
            && f.kind == FaultKind::SpillError
            && !self.oracle
            && decide(self.seed, LANE_SPILL, self.step, n, 0) % 1000 < permille(f.p)
        {
            self.fired = "spill";
            return Err("I/O error: simulated spill I/O error".to_string());
        }
        Ok(())
    }

    // --- corruption (simulation-testing.md §6.3 `corrupt`) ---

    /// Flip `n` bits in file `id`'s durable body pages below `page_count`, choosing each page (lane
    /// 12) and bit (lane 13) by the flip's ordinal, and skipping a page with a pending write: bit rot
    /// strikes data at rest. Both durable and cache change, as if the kernel's cached copy were
    /// dropped too. With `reseal`, each touched page's CRC is recomputed, so only the engine's
    /// structural checks stand between the flip and a wrong answer. Returns what to restore: each
    /// flipped page and its durable bytes before the flips.
    pub(crate) fn corrupt(
        &mut self,
        id: usize,
        step: u64,
        n: i64,
        page_count: u32,
        reseal: bool,
    ) -> Vec<(u64, Vec<u8>)> {
        let ps = self.page_size;
        if page_count <= 2 {
            return Vec::new();
        }
        let mut flips: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut touched: HashSet<u64> = HashSet::new();
        for k in 0..n.max(0) as u64 {
            let pg = 2 + decide(self.seed, LANE_PAGE, step, k, 0) % (page_count as u64 - 2);
            let bit = decide(self.seed, LANE_BIT, step, k, 0) % (ps * 8);
            let start = pg * ps;
            let file = &mut self.arena[id];
            if start + ps > file.durable.len() as u64
                || start + ps > file.cache.len() as u64
                || pending_in(file, start, start + ps)
            {
                continue;
            }
            let (lo, hi) = (start as usize, (start + ps) as usize);
            if touched.insert(pg) {
                flips.push((pg, file.durable[lo..hi].to_vec()));
            }
            file.durable[lo + (bit / 8) as usize] ^= 1 << (bit % 8);
        }
        let file = &mut self.arena[id];
        for (pg, _) in &flips {
            let (lo, hi) = ((pg * ps) as usize, ((pg + 1) * ps) as usize);
            if reseal {
                let crc = crate::format::page_crc(&file.durable[lo..hi]);
                file.durable[lo + 12..lo + 16].copy_from_slice(&crc.to_be_bytes());
            }
            let page = file.durable[lo..hi].to_vec();
            file.cache[lo..hi].copy_from_slice(&page);
        }
        flips
    }

    // --- restores and page replay (simulation-testing.md §8.3, slice S13) ---

    /// Copy file `from` over path `to` as a host restoring a backup does, with every process gone:
    /// the directory is synced, then `to` is a new file holding `from`'s bytes, durable, under a
    /// durable entry. It inherits the archive of the file it replaces, then `from`'s.
    pub(crate) fn restore(&mut self, from: &str, to: &str) {
        self.durable = self.files.clone();
        self.dir_ops.clear();
        let src = self.files[from];
        let id = self.new_file(to);
        let bytes = self.arena[src].cache.clone();
        self.arena[id].durable = bytes.clone();
        self.arena[id].cache = bytes;
        if let Some(old) = self.files.get(to).copied() {
            let archive = self.arena[old].archive.clone();
            self.arena[id].inherit(&archive);
        }
        let archive = self.arena[src].archive.clone();
        self.arena[id].inherit(&archive);
        self.files.insert(to.to_string(), id);
        self.durable.insert(to.to_string(), id);
        let key = self.arena[id].key;
        self.fold(b'R', key, 0, &[]);
    }

    /// The archived images of file `id`'s page `pg` other than its current bytes, in byte order, or
    /// none when a write to it is still pending.
    pub(crate) fn replay_versions(&self, id: usize, pg: u64) -> Vec<Vec<u8>> {
        let ps = self.page_size;
        let file = &self.arena[id];
        let start = pg * ps;
        if start + ps > file.cache.len() as u64
            || start + ps > file.durable.len() as u64
            || pending_in(file, start, start + ps)
        {
            return Vec::new();
        }
        let current = &file.cache[start as usize..(start + ps) as usize];
        let mut out: Vec<Vec<u8>> = file
            .archive
            .get(&pg)
            .map(|vs| {
                vs.iter()
                    .filter(|v| v.as_slice() != current)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }

    /// Put `image` at file `id`'s page `pg`, durable and cached, and return what
    /// [`repair`](SimFs::repair) needs to undo it.
    pub(crate) fn replace(&mut self, id: usize, pg: u64, image: &[u8]) -> (u64, Vec<u8>) {
        let start = (pg * self.page_size) as usize;
        let file = &mut self.arena[id];
        let orig = file.durable[start..start + image.len()].to_vec();
        file.durable[start..start + image.len()].copy_from_slice(image);
        file.cache[start..start + image.len()].copy_from_slice(image);
        (pg, orig)
    }

    /// Restore the pages [`corrupt`](SimFs::corrupt) flipped.
    pub(crate) fn repair(&mut self, id: usize, flips: &[(u64, Vec<u8>)]) {
        let ps = self.page_size as usize;
        let file = &mut self.arena[id];
        for (pg, orig) in flips {
            let lo = *pg as usize * ps;
            file.durable[lo..lo + orig.len()].copy_from_slice(orig);
            file.cache[lo..lo + orig.len()].copy_from_slice(orig);
        }
    }
}

/// Whether any pending write of `file` overlaps `[lo, hi)`.
fn pending_in(file: &SimFile, lo: u64, hi: u64) -> bool {
    file.pending.iter().any(|(off, vs)| {
        *off < hi && off + vs.last().expect("a pending offset holds a version").len() as u64 > lo
    })
}

fn put_durable(durable: &mut Vec<u8>, off: u64, p: &[u8]) {
    let off = off as usize;
    let end = off + p.len();
    if end > durable.len() {
        durable.resize(end, 0);
    }
    durable[off..end].copy_from_slice(p);
}

/// The runner's shared handle to the filesystem.
pub(crate) type SharedFs = Arc<Mutex<SimFs>>;

fn lock(fs: &SharedFs) -> std::sync::MutexGuard<'_, SimFs> {
    fs.lock().unwrap_or_else(|e| e.into_inner())
}

/// The `BlockStore` handed to a pager: one file of the shared filesystem, so the filesystem outlives
/// every pager (process) opened over it.
pub(crate) struct SimStore {
    pub(crate) fs: SharedFs,
    pub(crate) file: usize,
}

impl BlockStore for SimStore {
    fn read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        lock(&self.fs).read_at(self.file, offset, len)
    }
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        lock(&self.fs).write_at(self.file, offset, bytes)
    }
    fn sync(&mut self) -> Result<()> {
        lock(&self.fs).sync(self.file)
    }
    fn size(&mut self) -> Result<u64> {
        Ok(lock(&self.fs).size(self.file))
    }
    fn set_size(&mut self, bytes: u64) -> Result<()> {
        lock(&self.fs).set_size(self.file, bytes)
    }
}

/// The file host the simulated database's create, compaction, attachment opens, and first-commit
/// directory barrier go through. Paths are names in the one simulated directory; `real_path` is the
/// identity.
pub(crate) struct SimHost(pub(crate) SharedFs);

fn name_of(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

impl FileHost for SimHost {
    fn real_path(&self, path: &Path) -> Result<PathBuf> {
        Ok(path.to_path_buf())
    }

    fn exists(&self, path: &Path) -> Result<bool> {
        lock(&self.0).exists(&name_of(path))
    }

    fn write_file(
        &self,
        path: &Path,
        _mode_from: Option<&Path>,
        no_sync: bool,
        fill: &mut dyn FnMut(&mut WriteAt<'_>) -> Result<()>,
    ) -> Result<()> {
        let id = lock(&self.0).create(&name_of(path))?;
        // The lock is not held across `fill`: it reads the source pages through the database's own
        // store, which locks the filesystem per call.
        fill(&mut |offset, bytes| lock(&self.0).write_at(id, offset, bytes))?;
        if !no_sync {
            return lock(&self.0).sync(id);
        }
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        lock(&self.0).rename(&name_of(from), &name_of(to))
    }

    fn remove(&self, path: &Path) {
        lock(&self.0).remove(&name_of(path));
    }

    fn sync_dir(&self, _dir: &Path) {
        lock(&self.0).sync_dir();
    }

    fn open_store(
        &self,
        path: &Path,
        _no_sync: bool,
        _read_only: bool,
    ) -> Result<Box<dyn BlockStore>> {
        let file = lock(&self.0).open(&name_of(path))?;
        Ok(Box::new(SimStore {
            fs: self.0.clone(),
            file,
        }))
    }
}

/// The simulated spill host: scratch bytes are never durable state, so its files live outside the
/// filesystem and vanish with the process; `spill_error` fails an operation (S3).
pub(crate) struct SimSpill(pub(crate) SharedFs);

/// One simulated scratch file.
struct SimSpillFile {
    fs: SharedFs,
    data: Mutex<Vec<u8>>,
}

fn spill_io(msg: String) -> std::io::Error {
    std::io::Error::other(msg)
}

impl SpillHost for SimSpill {
    fn create(&self, _kind: &str) -> Result<Box<dyn SpillFile>> {
        lock(&self.0)
            .spill_fault()
            .map_err(|msg| EngineError::new(SqlState::IoError, msg))?;
        Ok(Box::new(SimSpillFile {
            fs: self.0.clone(),
            data: Mutex::new(Vec::new()),
        }))
    }
}

impl SimSpillFile {
    fn data(&self) -> std::sync::MutexGuard<'_, Vec<u8>> {
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl SpillFile for SimSpillFile {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        lock(&self.fs).spill_fault().map_err(spill_io)?;
        let data = self.data();
        if offset >= data.len() as u64 {
            return Ok(0);
        }
        let from = &data[offset as usize..];
        let n = from.len().min(buf.len());
        buf[..n].copy_from_slice(&from[..n]);
        Ok(n)
    }

    fn write_at(&self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        lock(&self.fs).spill_fault().map_err(spill_io)?;
        let mut data = self.data();
        let end = offset as usize + bytes.len();
        if end > data.len() {
            data.resize(end, 0);
        }
        data[offset as usize..end].copy_from_slice(bytes);
        Ok(())
    }

    fn size(&self) -> std::io::Result<u64> {
        Ok(self.data().len() as u64)
    }

    fn truncate(&self, n: u64) -> std::io::Result<()> {
        self.data().resize(n as usize, 0);
        Ok(())
    }

    fn close(&self) {
        *self.data() = Vec::new();
    }
}

#[cfg(test)]
mod tests {
    use super::decide;

    /// Pins `decide` to the shared vectors in spec/encoding/prng.toml (`[[sim_decide]]`).
    #[test]
    fn decide_matches_the_shared_vectors() {
        for (seed, lane, step, offset, version, want) in [
            (0, 1, 0, 0, 0, 0xe28195ddd9ee4956u64),
            (1, 1, 7, 8192, 0, 0xa26f2f0c00568058),
            (81723, 2, 42, 16384, 3, 0x1ec7ed20c6738508),
            (9223372036854775807, 4, 300, 640, 1, 0x925c4929a8b5d11b),
            (12345, 8, 9, 0, 0, 0x47a4d3845325ea26),
        ] {
            assert_eq!(decide(seed, lane, step, offset, version), want);
        }
    }
}
