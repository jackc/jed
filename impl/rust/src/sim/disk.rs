//! The deterministic-simulation disk (spec/design/simulation-testing.md §6): a `BlockStore` whose
//! durability, crashes, and I/O errors are decided by a seeded, content-keyed function, so a whole
//! multi-commit history replays exactly from its seed and a crash image is identical in every core.
//! A line-for-line port of the Go reference (`impl/go/sim_disk_test.go`).
//!
//! State (§6.1): `durable` bytes survive a power loss unconditionally; `pending` holds, per byte
//! offset, every version written since that offset was last covered by a barrier; `cache` is what
//! reads see (durable overlaid with the newest pending version). `sync` and a growing `set_size` are
//! barriers.
//!
//! Every fault decision is [`decide`]`(seed, lane, step, offset, version)` (§6.2): keyed by WHAT was
//! written WHERE, never by a write's position in the op log, so cores whose write order differs
//! within a commit still produce the same post-crash bytes.

use crate::blockstore::BlockStore;
use crate::error::{EngineError, Result, SqlState};
use std::collections::BTreeMap;
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
}

/// One armed fault (simulation-testing.md §6.3). Permille fields are 0..1000.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fault {
    pub(crate) kind: FaultKind,
    pub(crate) survive: i64, // power_loss
    pub(crate) tear: i64,    // power_loss
    pub(crate) issue: i64,   // process_crash
    pub(crate) p: i64,       // io_error op=write, read_error
    pub(crate) nth: i64,     // io_error op=sync
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

/// The simulated device. It outlives every Database opened over it: a crash discards the process
/// (the Database) but keeps the disk, as a real file would.
pub(crate) struct SimDisk {
    seed: u64,
    sector: usize,
    durable: Vec<u8>,
    pub(crate) cache: Vec<u8>,
    pending: BTreeMap<u64, Vec<Vec<u8>>>,

    // Per-step state, reset by begin_step.
    step: u64,
    fault: Option<Fault>,
    /// "" | "barrier" | "write" | "sync" | "read" | "enospc"
    pub(crate) fired: &'static str,
    /// The process died mid-step; every later call fails.
    dead: bool,
    syncs: i64,
    pub(crate) writes: usize,
    /// A barrier completed this step.
    pub(crate) barrier_ok: bool,
    /// The runner's own oracle reads are in progress: read faults do not apply.
    pub(crate) oracle: bool,

    /// FNV-1a fold of every mutating call, for the self-determinism check (§8.5).
    pub(crate) ops: u64,

    /// The writes an armed process_crash decided never reached the kernel; process_death removes them.
    unissued: Vec<(u64, usize)>,
}

fn injected(what: &str) -> EngineError {
    EngineError::new(SqlState::IoError, format!("I/O error: simulated {what}"))
}

impl SimDisk {
    pub(crate) fn new(seed: u64, sector: usize, image: &[u8]) -> SimDisk {
        SimDisk {
            seed,
            sector,
            durable: image.to_vec(),
            cache: image.to_vec(),
            pending: BTreeMap::new(),
            step: 0,
            fault: None,
            fired: "",
            dead: false,
            syncs: 0,
            writes: 0,
            barrier_ok: false,
            oracle: false,
            ops: FNV_OFFSET,
            unissued: Vec::new(),
        }
    }

    /// Reset the per-step state and arm `f` (`None` for an unfaulted step).
    pub(crate) fn begin_step(&mut self, step: u64, f: Option<Fault>) {
        self.step = step;
        self.fault = f;
        self.fired = "";
        self.dead = false;
        self.syncs = 0;
        self.writes = 0;
        self.barrier_ok = false;
        self.unissued.clear();
    }

    fn fold(&mut self, kind: u8, off: u64, p: &[u8]) {
        let mut buf = [0u8; 17];
        buf[0..8].copy_from_slice(&self.ops.to_be_bytes());
        buf[8] = kind;
        buf[9..17].copy_from_slice(&off.to_be_bytes());
        let h = fnv_fold(FNV_OFFSET, &buf);
        self.ops = fnv_fold(h, p);
    }

    fn read_at(&mut self, off: u64, len: usize) -> Result<Vec<u8>> {
        if let Some(f) = self.fault
            && f.kind == FaultKind::ReadError
            && !self.dead
            && !self.oracle
            && decide(self.seed, LANE_READ, self.step, off, 0) % 1000 < permille(f.p)
        {
            self.fired = "read";
            return Err(injected("read error"));
        }
        let end = off.checked_add(len as u64);
        match end {
            Some(end) if end <= self.cache.len() as u64 => {
                Ok(self.cache[off as usize..end as usize].to_vec())
            }
            _ => Err(EngineError::new(
                SqlState::IoError,
                "I/O error: short read past the end of the simulated disk",
            )),
        }
    }

    fn write_at(&mut self, off: u64, p: &[u8]) -> Result<()> {
        self.fold(b'w', off, p);
        if self.dead {
            return Err(injected("write after process death"));
        }
        let version = self.pending.get(&off).map_or(0, |vs| vs.len()) as u64;
        if let Some(f) = self.fault {
            match f.kind {
                FaultKind::ProcessCrash => {
                    // The live process sees all its own writes; only at its death does a write that
                    // was never issued to the kernel disappear (process_death).
                    if decide(self.seed, LANE_ISSUE, self.step, off, version) % 1000
                        >= permille(f.issue)
                    {
                        self.unissued.push((off, version as usize));
                    }
                }
                FaultKind::IoWrite => {
                    if self.fired.is_empty()
                        && decide(self.seed, LANE_IO_WRITE, self.step, off, version) % 1000
                            < permille(f.p)
                    {
                        self.fired = "write";
                        return Err(injected("write error"));
                    }
                }
                _ => {}
            }
        }
        self.writes += 1;
        let off_u = off as usize;
        let end = off_u + p.len();
        if end > self.cache.len() {
            self.cache.resize(end, 0);
        }
        self.cache[off_u..end].copy_from_slice(p);
        self.pending.entry(off).or_default().push(p.to_vec());
        Ok(())
    }

    fn put_durable(&mut self, off: u64, p: &[u8]) {
        let off = off as usize;
        let end = off + p.len();
        if end > self.durable.len() {
            self.durable.resize(end, 0);
        }
        self.durable[off..end].copy_from_slice(p);
    }

    /// Make every pending write durable: a successful barrier.
    fn flush(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        for (off, vs) in &pending {
            self.put_durable(*off, vs.last().expect("a pending offset holds a version"));
        }
        if self.durable.len() < self.cache.len() {
            self.durable.resize(self.cache.len(), 0);
        }
        self.barrier_ok = true;
        self.unissued.clear(); // reaching a barrier means every earlier write was issued
    }

    fn sync(&mut self) -> Result<()> {
        self.fold(b's', 0, &[]);
        if self.dead {
            return Err(injected("sync after process death"));
        }
        self.syncs += 1;
        if let Some(f) = self.fault
            && self.fired.is_empty()
        {
            match f.kind {
                FaultKind::PowerLoss | FaultKind::ProcessCrash => {
                    // The barrier never completes: the machine or process is gone.
                    self.fired = "barrier";
                    self.dead = true;
                    return Err(injected("crash at a durability barrier"));
                }
                FaultKind::IoSync => {
                    if self.syncs == f.nth {
                        self.fired = "sync";
                        self.fsyncgate();
                        return Err(injected("sync error"));
                    }
                }
                _ => {}
            }
        }
        self.flush();
        Ok(())
    }

    /// A failed fsync on Linux: each pending write independently became durable or was dropped from
    /// the cache, and the error is reported once (§6.3).
    fn fsyncgate(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        for (off, vs) in &pending {
            let newest = vs.last().expect("a pending offset holds a version");
            if decide(self.seed, LANE_LAND, self.step, *off, (vs.len() - 1) as u64) % 2 == 0 {
                self.put_durable(*off, newest);
                continue;
            }
            let start = *off as usize;
            for i in start..start + newest.len() {
                self.cache[i] = if i < self.durable.len() {
                    self.durable[i]
                } else {
                    0
                };
            }
        }
    }

    fn size(&self) -> u64 {
        self.cache.len() as u64
    }

    /// Grow durably (zero blocks + a full fsync, which also flushes every pending write — hosts.md
    /// §2.1) or truncate. It is never a fault point (simulation-testing.md §6.3).
    fn set_size(&mut self, n: u64) -> Result<()> {
        self.fold(b'z', n, &[]);
        if self.dead {
            return Err(injected("set_size after process death"));
        }
        let cur = self.cache.len() as u64;
        if let Some(f) = self.fault
            && f.kind == FaultKind::Enospc
            && n > cur
            && self.fired.is_empty()
        {
            self.fired = "enospc";
            return Err(injected("no space left on device"));
        }
        if n > cur {
            self.cache.resize(n as usize, 0);
            self.flush();
        } else if n < cur {
            self.cache.truncate(n as usize);
            if self.durable.len() as u64 > n {
                self.durable.truncate(n as usize);
            }
            self.pending.retain(|off, _| *off < n);
        }
        Ok(())
    }

    /// Replace the device contents with what survives a power cut during `step` (§6.3): for each
    /// pending offset, nothing lands or one pending version (any, not only the newest) lands,
    /// possibly torn sector by sector against the durable bytes. The cache is discarded.
    pub(crate) fn power_loss(&mut self, step: u64, survive: i64, tear: i64) {
        let pending = std::mem::take(&mut self.pending);
        for (off, vs) in &pending {
            let off = *off;
            if decide(self.seed, LANE_SURVIVE, step, off, 0) % 1000 >= permille(survive) {
                continue;
            }
            let idx = decide(self.seed, LANE_VERSION, step, off, 0) % vs.len() as u64;
            let mut data = vs[idx as usize].clone();
            if decide(self.seed, LANE_TEAR, step, off, idx) % 1000 < permille(tear) {
                let mut s = 0usize;
                while s < data.len() {
                    if decide(self.seed, LANE_SECTOR, step, off + s as u64, idx) & 1 == 1 {
                        s += self.sector;
                        continue; // this sector took the new bytes
                    }
                    let mut i = s;
                    while i < s + self.sector && i < data.len() {
                        let at = off as usize + i;
                        data[i] = if at < self.durable.len() {
                            self.durable[at]
                        } else {
                            0
                        };
                        i += 1;
                    }
                    s += self.sector;
                }
            }
            self.put_durable(off, &data);
        }
        self.cache = self.durable.clone();
    }

    /// End the process: the kernel cache survives with every issued write still pending (not
    /// durable), so a later power loss can still drop it (§6.3).
    pub(crate) fn process_death(&mut self) {
        // Undo the never-issued writes, newest first so each version index stays valid. A barrier that
        // completed after such a write already made it durable — the process could not have reached
        // that barrier without issuing it — so only still-pending versions are removed.
        let unissued = std::mem::take(&mut self.unissued);
        for &(off, version) in unissued.iter().rev() {
            let Some(vs) = self.pending.get_mut(&off) else {
                continue;
            };
            if version >= vs.len() {
                continue;
            }
            let size = vs[version].len();
            vs.remove(version);
            let newest = vs.last().cloned();
            if vs.is_empty() {
                self.pending.remove(&off);
            }
            for j in 0..size {
                let at = off as usize + j;
                self.cache[at] = match &newest {
                    Some(v) => v[j],
                    None if at < self.durable.len() => self.durable[at],
                    None => 0,
                };
            }
        }
        self.fault = None;
        self.dead = false;
    }
}

/// The `BlockStore` handed to the pager: a shared reference to the runner's disk, so the disk
/// outlives every pager (process) opened over it.
pub(crate) struct SimStore(pub(crate) Arc<Mutex<SimDisk>>);

impl SimStore {
    fn disk(&self) -> std::sync::MutexGuard<'_, SimDisk> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl BlockStore for SimStore {
    fn read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.disk().read_at(offset, len)
    }
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        self.disk().write_at(offset, bytes)
    }
    fn sync(&mut self) -> Result<()> {
        self.disk().sync()
    }
    fn size(&mut self) -> Result<u64> {
        Ok(self.disk().size())
    }
    fn set_size(&mut self, bytes: u64) -> Result<()> {
        self.disk().set_size(bytes)
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
