//! The deterministic-simulation lock host (spec/design/simulation-testing.md §14 S5): simulated
//! processes sharing one table of whole-file locks, standing in for the OS host of
//! `crate::coordinator`. A port of the Go reference (`impl/go/sim_locks_test.go`).
//!
//! A process that dies loses its locks at once, as the kernel releases a dead process's locks. A
//! contended acquisition never blocks the single simulation thread: each poll runs the other
//! processes' presence probes and advances the simulated clock, so the holder can react and
//! deadlines expire in simulated time.

use crate::coordinator::{LockFiles, LockHandle, LockHost, ProbeSignal};
use crate::error::Result;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Microseconds of simulated time per poll of a contended lock.
const LOCK_POLL: i64 = 2000;
/// Polls without success before a wait is reported as a deadlock.
const LOCK_MAX_POLLS: u64 = 100_000;

type Probe = Arc<dyn Fn() + Send + Sync>;

/// The kernel's view of every bundle lock: lock name → holding process → exclusive.
pub(crate) struct LockTable {
    holders: BTreeMap<String, BTreeMap<u32, bool>>,
    /// The runner's simulated clock, in microseconds.
    clock: Arc<AtomicI64>,
    /// The instant `now()` reports for the clock reading `base_clock`.
    base: Instant,
    base_clock: i64,
    /// Each live process's presence probe.
    probes: BTreeMap<u32, Probe>,
    /// Processes whose probe would block: each holds an open write session (its writer gate). The
    /// runner keeps this in step with its sessions.
    busy: BTreeSet<u32>,
    polls: u64,
    /// The incarnation of each process's current host, so a late drop of a dead process's
    /// coordinator cannot release the locks of a new process with the same id.
    current: BTreeMap<u32, u64>,
    next_incarnation: u64,
}

pub(crate) type SharedTable = Arc<Mutex<LockTable>>;

fn lock(table: &SharedTable) -> MutexGuard<'_, LockTable> {
    table.lock().unwrap_or_else(|e| e.into_inner())
}

pub(crate) fn new_table(clock: Arc<AtomicI64>) -> SharedTable {
    let base_clock = clock.load(Ordering::Relaxed);
    Arc::new(Mutex::new(LockTable {
        holders: BTreeMap::new(),
        clock,
        base: Instant::now(),
        base_clock,
        probes: BTreeMap::new(),
        busy: BTreeSet::new(),
        polls: 0,
        current: BTreeMap::new(),
        next_incarnation: 0,
    }))
}

/// Drop every lock `pid` holds and forget its probe: the process is gone.
pub(crate) fn release(table: &SharedTable, pid: u32) {
    let mut t = lock(table);
    for holders in t.holders.values_mut() {
        holders.remove(&pid);
    }
    t.probes.remove(&pid);
}

/// Record which processes hold an open write session.
pub(crate) fn set_busy(table: &SharedTable, busy: BTreeSet<u32>) {
    lock(table).busy = busy;
}

/// Run every live process's probe except `skip`'s, in process order. The table lock is never held
/// while a probe runs: a probe takes and polls locks itself.
pub(crate) fn run_probes(table: &SharedTable, skip: Option<u32>) {
    let pids: Vec<u32> = lock(table).probes.keys().copied().collect();
    for pid in pids {
        if Some(pid) == skip {
            continue;
        }
        let probe = {
            let t = lock(table);
            if t.busy.contains(&pid) {
                None
            } else {
                t.probes.get(&pid).cloned()
            }
        };
        if let Some(probe) = probe {
            probe();
        }
    }
}

/// One simulated process's lock host.
pub(crate) struct SimLockHost {
    table: SharedTable,
    pid: u32,
    incarnation: u64,
    /// The process-local open registry.
    claimed: Mutex<HashSet<PathBuf>>,
}

impl SimLockHost {
    pub(crate) fn new(table: SharedTable, pid: u32) -> SimLockHost {
        let incarnation = {
            let mut t = lock(&table);
            t.next_incarnation += 1;
            let n = t.next_incarnation;
            t.current.insert(pid, n);
            n
        };
        SimLockHost {
            table,
            pid,
            incarnation,
            claimed: Mutex::new(HashSet::new()),
        }
    }
}

fn lock_name(handle: &LockHandle) -> &str {
    handle
        .downcast_ref::<String>()
        .expect("the simulated lock host's handles are names")
}

impl LockHost for SimLockHost {
    fn claim_path(&self, path: &Path) -> bool {
        self.claimed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path.to_path_buf())
    }

    fn release_path(&self, path: &Path) {
        self.claimed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(path);
    }

    fn open_bundle(&self, _path: &Path) -> Result<LockFiles> {
        Ok(LockFiles {
            presence: Box::new("presence".to_string()),
            arrival: Box::new("arrival".to_string()),
            transition: Box::new("transition".to_string()),
            writer: Box::new("writer".to_string()),
            commit: Box::new("commit".to_string()),
        })
    }

    fn validate_marker(&self, _path: &Path) -> Result<()> {
        Ok(())
    }

    fn close_files(&self, _files: &LockFiles) {
        let mut t = lock(&self.table);
        if t.current.get(&self.pid) != Some(&self.incarnation) {
            return; // a later process now has this id; its locks are not ours
        }
        for holders in t.holders.values_mut() {
            holders.remove(&self.pid);
        }
    }

    /// Take or convert the lock if no other process conflicts: an exclusive lock needs no other
    /// holder, a shared one no other exclusive holder.
    fn try_lock(&self, handle: &LockHandle, exclusive: bool) -> std::io::Result<bool> {
        let mut t = lock(&self.table);
        let holders = t.holders.entry(lock_name(handle).to_string()).or_default();
        if holders
            .iter()
            .any(|(pid, ex)| *pid != self.pid && (exclusive || *ex))
        {
            return Ok(false);
        }
        holders.insert(self.pid, exclusive);
        t.polls = 0;
        Ok(true)
    }

    /// The Go reference's `lockBlocking`: poll, pausing between attempts.
    fn lock(&self, handle: &LockHandle, exclusive: bool) -> std::io::Result<()> {
        loop {
            if self.try_lock(handle, exclusive)? {
                return Ok(());
            }
            self.pause();
        }
    }

    fn unlock(&self, handle: &LockHandle) -> std::io::Result<()> {
        let mut t = lock(&self.table);
        if let Some(holders) = t.holders.get_mut(lock_name(handle)) {
            holders.remove(&self.pid);
        }
        Ok(())
    }

    fn now(&self) -> Instant {
        let t = lock(&self.table);
        let micros = t.clock.load(Ordering::Relaxed).saturating_sub(t.base_clock);
        t.base + Duration::from_micros(micros.max(0) as u64)
    }

    fn pause(&self) {
        let polls = {
            let mut t = lock(&self.table);
            t.polls += 1;
            t.polls
        };
        if polls > LOCK_MAX_POLLS {
            panic!(
                "simulated deadlock: process p{} waited {polls} polls for a lock",
                self.pid
            );
        }
        run_probes(&self.table, Some(self.pid));
        lock(&self.table)
            .clock
            .fetch_add(LOCK_POLL, Ordering::Relaxed);
    }

    fn start_probe(
        &self,
        _signal: Arc<ProbeSignal>,
        callback: Arc<dyn Fn() + Send + Sync>,
    ) -> Option<JoinHandle<()>> {
        lock(&self.table).probes.insert(self.pid, callback);
        None
    }

    fn pid(&self) -> u32 {
        self.pid
    }
}
