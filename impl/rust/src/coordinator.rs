//! Stable cross-process file coordination (spec/design/locking.md).
//!
//! The database file is never itself the lock identity. Five empty files in the persistent
//! `<database>.lock/` bundle carry whole-file OS locks; the database remains one copyable data file.
//!
//! Every lock operation goes through a [`LockHost`]: the lock bundle, whole-file locks on it, the
//! clock deadlines read, the pause between polls of a contended lock, the background presence probe,
//! and the process-local duplicate-open registry. Production uses the OS ([`OsLockHost`]). The
//! deterministic simulator (spec/design/simulation-testing.md §14 S5) substitutes simulated
//! processes sharing one lock table, so the protocol runs, crashes, and interleaves deterministically.

use std::collections::HashSet;
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::error::{EngineError, Result, SqlState};
use crate::file::Locking;

const PROTOCOL_MARKER: &str = "protocol-v1";
const LOCK_NAMES: [&str; 5] = ["presence", "arrival", "transition", "writer", "commit"];
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const RETRY_INTERVAL: Duration = Duration::from_millis(2);

static OPEN_PATHS: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LeaseState {
    Alone = 1,
    Shared = 2,
    Exclusive = 3,
    Poisoned = 4,
}

impl LeaseState {
    fn from_u8(value: u8) -> LeaseState {
        match value {
            1 => LeaseState::Alone,
            2 => LeaseState::Shared,
            3 => LeaseState::Exclusive,
            _ => LeaseState::Poisoned,
        }
    }
}

/// One bundle file's lock, opaque to the coordinator: a [`File`] for the OS host.
pub(crate) type LockHandle = Box<dyn std::any::Any + Send + Sync>;

/// The five bundle locks of one participant.
pub(crate) struct LockFiles {
    pub(crate) presence: LockHandle,
    pub(crate) arrival: LockHandle,
    pub(crate) transition: LockHandle,
    pub(crate) writer: LockHandle,
    pub(crate) commit: LockHandle,
}

/// The stop signal of an OS presence-probe thread.
pub(crate) struct ProbeSignal {
    stop: AtomicBool,
    wake_mu: Mutex<()>,
    wake: Condvar,
}

/// Everything the coordinator needs from its environment (see the module docs).
pub(crate) trait LockHost: Send + Sync {
    /// Register `path` as open in this process; false if it already is.
    fn claim_path(&self, path: &Path) -> bool;
    fn release_path(&self, path: &Path);
    /// Create or open the lock bundle beside `path` and open its five lock handles.
    fn open_bundle(&self, path: &Path) -> Result<LockFiles>;
    /// Re-validate the protocol marker (held under arrival SH, the migration barrier).
    fn validate_marker(&self, path: &Path) -> Result<()>;
    /// Release this participant's hold on its bundle (the OS closes the files when they drop).
    fn close_files(&self, files: &LockFiles);
    /// Take or convert the lock without waiting: `Ok(false)` when another holder conflicts.
    fn try_lock(&self, handle: &LockHandle, exclusive: bool) -> std::io::Result<bool>;
    /// Take or convert the lock, waiting as long as it takes.
    fn lock(&self, handle: &LockHandle, exclusive: bool) -> std::io::Result<()>;
    fn unlock(&self, handle: &LockHandle) -> std::io::Result<()>;
    /// The clock lock deadlines read.
    fn now(&self) -> Instant;
    /// Wait between polls of a contended lock.
    fn pause(&self);
    /// Run `callback` periodically until `signal` stops; the OS returns the probe thread to join.
    fn start_probe(
        &self,
        signal: Arc<ProbeSignal>,
        callback: Arc<dyn Fn() + Send + Sync>,
    ) -> Option<JoinHandle<()>>;
    /// The process id the fork check compares.
    fn pid(&self) -> u32;
}

/// The production coordination host: `flock` / `LockFileEx` on the bundle files.
pub(crate) struct OsLockHost;

fn os_file(handle: &LockHandle) -> &File {
    handle
        .downcast_ref::<File>()
        .expect("the OS lock host's handles are files")
}

impl LockHost for OsLockHost {
    fn claim_path(&self, path: &Path) -> bool {
        OPEN_PATHS
            .lock()
            .expect("open-path registry poisoned")
            .insert(path.to_path_buf())
    }

    fn release_path(&self, path: &Path) {
        OPEN_PATHS
            .lock()
            .expect("open-path registry poisoned")
            .remove(path);
    }

    fn open_bundle(&self, path: &Path) -> Result<LockFiles> {
        open_bundle(path)
    }

    fn validate_marker(&self, path: &Path) -> Result<()> {
        validate_protocol_marker(path)
    }

    fn close_files(&self, _files: &LockFiles) {}

    fn try_lock(&self, handle: &LockHandle, exclusive: bool) -> std::io::Result<bool> {
        let file = os_file(handle);
        let result = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        match result {
            Ok(()) => Ok(true),
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }

    fn lock(&self, handle: &LockHandle, exclusive: bool) -> std::io::Result<()> {
        let file = os_file(handle);
        if exclusive {
            file.lock()
        } else {
            file.lock_shared()
        }
    }

    fn unlock(&self, handle: &LockHandle) -> std::io::Result<()> {
        os_file(handle).unlock()
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn pause(&self) {
        thread::sleep(RETRY_INTERVAL);
    }

    fn start_probe(
        &self,
        signal: Arc<ProbeSignal>,
        callback: Arc<dyn Fn() + Send + Sync>,
    ) -> Option<JoinHandle<()>> {
        Some(thread::spawn(move || {
            loop {
                // Release the wait mutex before the callback: Drop takes it to publish `stop`, and
                // may run on this thread when the callback releases the last handle.
                let timed_out = {
                    let guard = signal.wake_mu.lock().expect("probe wake lock poisoned");
                    let (_guard, wait) = signal
                        .wake
                        .wait_timeout_while(guard, POLL_INTERVAL, |_| {
                            !signal.stop.load(Ordering::Acquire)
                        })
                        .expect("probe wake lock poisoned");
                    wait.timed_out()
                };
                if signal.stop.load(Ordering::Acquire) {
                    break;
                }
                if timed_out {
                    callback();
                }
            }
        }))
    }

    fn pid(&self) -> u32 {
        std::process::id()
    }
}

/// One registered participant in a file's stable lock domain.
pub(crate) struct FileCoordinator {
    host: Arc<dyn LockHost>,
    path: PathBuf,
    files: LockFiles,
    state: AtomicU8,
    signal: Arc<ProbeSignal>,
    probe: Mutex<Option<JoinHandle<()>>>,
    probe_started: AtomicBool,
    opener_pid: u32,
}

/// An RAII whole-file lock used for the short commit/meta adoption gates.
pub(crate) struct FileLockGuard<'a> {
    host: &'a dyn LockHost,
    handle: &'a LockHandle,
}

impl Drop for FileLockGuard<'_> {
    fn drop(&mut self) {
        let _ = self.host.unlock(self.handle);
    }
}

impl FileCoordinator {
    /// Prepare coordination for an existing database before any database byte is read.
    pub(crate) fn open(path: &Path, mode: Locking, timeout_ms: u64) -> Result<Option<Self>> {
        let mode = resolve_mode(mode)?;
        if mode == Locking::None {
            return Ok(None);
        }
        let path = resolve_open_path(path)?;
        Self::acquire(Arc::new(OsLockHost), path, mode, timeout_ms).map(Some)
    }

    /// The registered participant of a page-service owner elected at open
    /// (spec/pageservice/README.md §6.1): presence EX held for its whole tenure and no join-window
    /// probe, exactly like an exclusive opener. `files` are the bundle locks the election opened;
    /// the path is already claimed.
    pub(crate) fn elected_owner(
        host: Arc<dyn LockHost>,
        path: PathBuf,
        files: LockFiles,
    ) -> FileCoordinator {
        let opener_pid = host.pid();
        FileCoordinator {
            host,
            path,
            files,
            state: AtomicU8::new(LeaseState::Exclusive as u8),
            signal: Arc::new(ProbeSignal {
                stop: AtomicBool::new(false),
                wake_mu: Mutex::new(()),
                wake: Condvar::new(),
            }),
            probe: Mutex::new(None),
            probe_started: AtomicBool::new(false),
            opener_pid,
        }
    }

    /// Prepare coordination for an absent database path before the initial image is published.
    pub(crate) fn create(path: &Path, mode: Locking, timeout_ms: u64) -> Result<Option<Self>> {
        let mode = resolve_mode(mode)?;
        if mode == Locking::None {
            return Ok(None);
        }
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let parent =
            fs::canonicalize(parent).map_err(|e| io_error("resolve database parent", e))?;
        let name = path.file_name().ok_or_else(|| {
            EngineError::new(SqlState::IoError, "database path has no final component")
        })?;
        Self::acquire(Arc::new(OsLockHost), parent.join(name), mode, timeout_ms).map(Some)
    }

    /// Register as a participant in `path`'s lock domain through `host` (`mode` is Shared or
    /// Exclusive; the caller has resolved Auto and handled None).
    pub(crate) fn acquire(
        host: Arc<dyn LockHost>,
        path: PathBuf,
        mode: Locking,
        timeout_ms: u64,
    ) -> Result<Self> {
        if !host.claim_path(&path) {
            return Err(EngineError::new(
                SqlState::ObjectInUse,
                format!(
                    "database is already open in this process: {} (share one Database handle)",
                    path.display()
                ),
            ));
        }
        let files = match host.open_bundle(&path) {
            Ok(files) => files,
            Err(e) => {
                host.release_path(&path);
                return Err(e);
            }
        };
        let h = host.as_ref();
        let result = (|| {
            let deadline = deadline(h, timeout_ms);
            lock_until(h, &files.arrival, false, deadline, &path, false)?;
            h.validate_marker(&path)?;

            let state = if mode == Locking::Exclusive {
                loop {
                    lock_until(h, &files.transition, true, deadline, &path, false)?;
                    match try_lock(h, &files.presence, true)? {
                        true => {
                            h.unlock(&files.transition)
                                .map_err(|e| lock_error(e, &path))?;
                            break LeaseState::Exclusive;
                        }
                        false => {
                            h.unlock(&files.transition)
                                .map_err(|e| lock_error(e, &path))?;
                            check_deadline(h, deadline, &path, false)?;
                            h.pause();
                        }
                    }
                }
            } else {
                // A first opener takes presence EX and immediately gets the fast path. A participant
                // already present makes the EX attempt fail; its arrival lock then drives the holder's
                // downgrade and this opener joins with presence SH.
                lock_until(h, &files.transition, true, deadline, &path, false)?;
                let alone = try_lock(h, &files.presence, true)?;
                h.unlock(&files.transition)
                    .map_err(|e| lock_error(e, &path))?;
                if alone {
                    LeaseState::Alone
                } else {
                    lock_until(h, &files.presence, false, deadline, &path, false)?;
                    LeaseState::Shared
                }
            };
            h.unlock(&files.arrival).map_err(|e| lock_error(e, &path))?;
            Ok(state)
        })();
        match result {
            Ok(state) => {
                let opener_pid = host.pid();
                Ok(FileCoordinator {
                    host,
                    path,
                    files,
                    state: AtomicU8::new(state as u8),
                    signal: Arc::new(ProbeSignal {
                        stop: AtomicBool::new(false),
                        wake_mu: Mutex::new(()),
                        wake: Condvar::new(),
                    }),
                    probe: Mutex::new(None),
                    probe_started: AtomicBool::new(false),
                    opener_pid,
                })
            }
            Err(e) => {
                host.close_files(&files);
                drop(files);
                host.release_path(&path);
                Err(e)
            }
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn state(&self) -> LeaseState {
        LeaseState::from_u8(self.state.load(Ordering::Acquire))
    }

    pub(crate) fn set_state(&self, state: LeaseState) {
        self.state.store(state as u8, Ordering::Release);
    }

    pub(crate) fn check_pid(&self) -> Result<()> {
        if self.host.pid() == self.opener_pid {
            Ok(())
        } else {
            Err(EngineError::new(
                SqlState::ObjectInUse,
                "database handle was inherited across fork; reopen it in the child",
            ))
        }
    }

    fn guard<'a>(&'a self, handle: &'a LockHandle) -> FileLockGuard<'a> {
        FileLockGuard {
            host: self.host.as_ref(),
            handle,
        }
    }

    pub(crate) fn lock_commit_shared(&self) -> Result<FileLockGuard<'_>> {
        self.host
            .lock(&self.files.commit, false)
            .map_err(|e| lock_error(e, &self.path))?;
        Ok(self.guard(&self.files.commit))
    }

    pub(crate) fn lock_commit_exclusive(&self) -> Result<FileLockGuard<'_>> {
        self.host
            .lock(&self.files.commit, true)
            .map_err(|e| lock_error(e, &self.path))?;
        Ok(self.guard(&self.files.commit))
    }

    pub(crate) fn lock_writer(&self, timeout_ms: u64) -> Result<()> {
        let h = self.host.as_ref();
        lock_until(
            h,
            &self.files.writer,
            true,
            if timeout_ms == 0 {
                None
            } else {
                deadline(h, timeout_ms)
            },
            &self.path,
            true,
        )
    }

    pub(crate) fn unlock_writer(&self) {
        let _ = self.host.unlock(&self.files.writer);
    }

    pub(crate) fn lock_transition(&self) -> Result<FileLockGuard<'_>> {
        self.host
            .lock(&self.files.transition, true)
            .map_err(|e| lock_error(e, &self.path))?;
        Ok(self.guard(&self.files.transition))
    }

    pub(crate) fn try_arrival_exclusive(&self) -> Result<Option<FileLockGuard<'_>>> {
        if try_lock(self.host.as_ref(), &self.files.arrival, true)? {
            Ok(Some(self.guard(&self.files.arrival)))
        } else {
            Ok(None)
        }
    }

    /// Convert the held presence EX lease to SH. The caller holds transition EX and the local barrier.
    pub(crate) fn downgrade_presence(&self) -> Result<()> {
        self.set_state(LeaseState::Shared);
        self.host
            .unlock(&self.files.presence)
            .map_err(|e| lock_error(e, &self.path))?;
        self.host
            .lock(&self.files.presence, false)
            .map_err(|e| lock_error(e, &self.path))
    }

    /// Try SH→EX while arrival EX and transition EX make the temporary unlock invisible.
    pub(crate) fn try_upgrade_presence(&self) -> Result<bool> {
        self.host
            .unlock(&self.files.presence)
            .map_err(|e| lock_error(e, &self.path))?;
        if try_lock(self.host.as_ref(), &self.files.presence, true)? {
            Ok(true)
        } else {
            self.host
                .lock(&self.files.presence, false)
                .map_err(|e| lock_error(e, &self.path))?;
            Ok(false)
        }
    }

    /// Start the unreferenced-equivalent background lease probe. The callback performs one complete
    /// transition attempt under the core's local writer/reader barrier.
    pub(crate) fn start_probe(&self, callback: Arc<dyn Fn() + Send + Sync>) {
        if self.state() == LeaseState::Exclusive {
            return;
        }
        let mut slot = self.probe.lock().expect("probe lock poisoned");
        if self.probe_started.swap(true, Ordering::AcqRel) {
            return;
        }
        *slot = self.host.start_probe(Arc::clone(&self.signal), callback);
    }
}

impl Drop for FileCoordinator {
    fn drop(&mut self) {
        // Publish `stop` under the probe's wait mutex: set without it, the store + notify could land
        // between the probe's predicate check and its park, losing the wakeup and stalling this
        // join for a full POLL_INTERVAL.
        {
            let _guard = self
                .signal
                .wake_mu
                .lock()
                .expect("probe wake lock poisoned");
            self.signal.stop.store(true, Ordering::Release);
        }
        self.signal.wake.notify_all();
        if let Some(handle) = self.probe.lock().expect("probe lock poisoned").take() {
            if handle.thread().id() != thread::current().id() {
                let _ = handle.join();
            }
        }
        self.host.close_files(&self.files);
        self.host.release_path(&self.path);
    }
}

/// Resolve an existing database path as a coordinated open does (locking.md §3.1): symlinks
/// followed, the result absolute, and a hard-linked file rejected.
pub(crate) fn resolve_open_path(path: &Path) -> Result<PathBuf> {
    let resolved = fs::canonicalize(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            EngineError::new(
                SqlState::UndefinedFile,
                format!("database file does not exist: {}", path.display()),
            )
        } else {
            io_error("resolve database path", e)
        }
    })?;
    #[cfg(any(unix, windows))]
    reject_hard_link(&resolved)?;
    Ok(resolved)
}

pub(crate) fn try_lock(host: &dyn LockHost, handle: &LockHandle, exclusive: bool) -> Result<bool> {
    host.try_lock(handle, exclusive)
        .map_err(classify_lock_error)
}

pub(crate) fn lock_until(
    host: &dyn LockHost,
    handle: &LockHandle,
    exclusive: bool,
    deadline: Option<Instant>,
    path: &Path,
    writer: bool,
) -> Result<()> {
    loop {
        if try_lock(host, handle, exclusive)? {
            return Ok(());
        }
        check_deadline(host, deadline, path, writer)?;
        host.pause();
    }
}

pub(crate) fn deadline(host: &dyn LockHost, timeout_ms: u64) -> Option<Instant> {
    Some(host.now() + Duration::from_millis(timeout_ms))
}

pub(crate) fn check_deadline(
    host: &dyn LockHost,
    deadline: Option<Instant>,
    path: &Path,
    writer: bool,
) -> Result<()> {
    if deadline.is_some_and(|d| host.now() >= d) {
        let state = if writer {
            SqlState::LockNotAvailable
        } else {
            SqlState::ObjectInUse
        };
        let message = if writer {
            "could not obtain the database writer lock".to_string()
        } else {
            format!("database file is in use: {}", path.display())
        };
        Err(EngineError::new(state, message))
    } else {
        Ok(())
    }
}

fn resolve_mode(mode: Locking) -> Result<Locking> {
    if mode == Locking::Service {
        // Creating under the test-only page-service mode takes presence EX like an exclusive opener:
        // the creator is the owner (spec/pageservice/README.md §6.1). Windows serving is not yet
        // specified (page-service.md §14).
        return if cfg!(unix) {
            Ok(Locking::Exclusive)
        } else {
            Err(service_unsupported())
        };
    }
    #[cfg(any(unix, windows))]
    {
        Ok(if mode == Locking::Auto {
            Locking::Shared
        } else {
            mode
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        if mode == Locking::None {
            Ok(mode)
        } else {
            Err(EngineError::new(
                SqlState::FeatureNotSupported,
                "file locking is unavailable on this host; use locking=none only with external coordination",
            ))
        }
    }
}

/// `locking = service` off Unix (page-service.md §14).
pub(crate) fn service_unsupported() -> EngineError {
    EngineError::new(
        SqlState::FeatureNotSupported,
        "the page service is unavailable on this platform",
    )
}

fn open_bundle(path: &Path) -> Result<LockFiles> {
    let bundle = PathBuf::from(format!("{}.lock", path.display()));
    match fs::create_dir(&bundle) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(io_error("create coordination directory", e)),
    }
    let meta =
        fs::symlink_metadata(&bundle).map_err(|e| io_error("inspect coordination directory", e))?;
    if !meta.file_type().is_dir() || meta.file_type().is_symlink() {
        return Err(EngineError::new(
            SqlState::IoError,
            format!("coordination path is not a directory: {}", bundle.display()),
        ));
    }

    validate_protocol_marker(path)?;
    ensure_regular_file(&bundle.join(PROTOCOL_MARKER))?;
    for name in LOCK_NAMES {
        ensure_regular_file(&bundle.join(name))?;
    }
    // Revalidate after creation/open. The arrival lock acquired by the caller protects the final
    // protocol-marker check against a cooperating future migration.
    ensure_regular_file(&bundle.join(PROTOCOL_MARKER))?;
    Ok(LockFiles {
        presence: Box::new(open_regular(&bundle.join("presence"))?),
        arrival: Box::new(open_regular(&bundle.join("arrival"))?),
        transition: Box::new(open_regular(&bundle.join("transition"))?),
        writer: Box::new(open_regular(&bundle.join("writer"))?),
        commit: Box::new(open_regular(&bundle.join("commit"))?),
    })
}

/// Validate once during bundle assembly and once after arrival SH is held. The latter is the
/// protocol-migration barrier required by locking.md: a cooperating future migrator cannot change
/// the version domain between validation and presence acquisition.
fn validate_protocol_marker(path: &Path) -> Result<()> {
    let bundle = PathBuf::from(format!("{}.lock", path.display()));
    for entry in fs::read_dir(&bundle).map_err(|e| io_error("read coordination directory", e))? {
        let entry = entry.map_err(|e| io_error("read coordination entry", e))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("protocol-v") && name != PROTOCOL_MARKER {
            return Err(EngineError::new(
                SqlState::FeatureNotSupported,
                format!(
                    "unsupported coordination protocol marker {name}; supported: {PROTOCOL_MARKER}"
                ),
            ));
        }
    }
    ensure_regular_file(&bundle.join(PROTOCOL_MARKER))
}

fn ensure_regular_file(path: &Path) -> Result<()> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(io_error("create coordination entry", e)),
    }
    let meta = fs::symlink_metadata(path).map_err(|e| io_error("inspect coordination entry", e))?;
    if !meta.file_type().is_file() || meta.file_type().is_symlink() {
        return Err(EngineError::new(
            SqlState::IoError,
            format!(
                "coordination entry is not a regular file: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn open_regular(path: &Path) -> Result<File> {
    ensure_regular_file(path)?;
    fs::OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|e| io_error("open coordination entry", e))
}

#[cfg(unix)]
fn reject_hard_link(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if fs::metadata(path)
        .map_err(|e| io_error("inspect database identity", e))?
        .nlink()
        != 1
    {
        Err(EngineError::new(
            SqlState::ObjectInUse,
            format!(
                "hard-linked database paths cannot use jed locking: {}",
                path.display()
            ),
        ))
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn reject_hard_link(path: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    if fs::metadata(path)
        .map_err(|e| io_error("inspect database identity", e))?
        .number_of_links()
        != 1
    {
        Err(EngineError::new(
            SqlState::ObjectInUse,
            format!(
                "hard-linked database paths cannot use jed locking: {}",
                path.display()
            ),
        ))
    } else {
        Ok(())
    }
}

fn classify_lock_error(error: std::io::Error) -> EngineError {
    if error.kind() == std::io::ErrorKind::Unsupported {
        EngineError::new(
            SqlState::FeatureNotSupported,
            "OS file locks are unavailable",
        )
    } else {
        EngineError::new(SqlState::IoError, format!("file lock failed: {error}"))
    }
}

pub(crate) fn lock_error(error: std::io::Error, path: &Path) -> EngineError {
    let mut classified = classify_lock_error(error);
    classified.message = format!("{}: {}", classified.message, path.display());
    classified
}

fn io_error(operation: &str, error: std::io::Error) -> EngineError {
    EngineError::new(SqlState::IoError, format!("{operation}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_is_stable_and_duplicate_open_is_rejected() {
        let root = std::env::temp_dir().join(format!("jed-lock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir(&root).unwrap();
        let path = root.join("db.jed");
        fs::write(&path, b"not read by coordinator").unwrap();
        let first = FileCoordinator::open(&path, Locking::Shared, 0)
            .unwrap()
            .unwrap();
        let err = match FileCoordinator::open(&path, Locking::Shared, 0) {
            Ok(_) => panic!("duplicate coordinated open unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(err.state, SqlState::ObjectInUse);
        drop(first);
        assert!(root.join("db.jed.lock/protocol-v1").is_file());
        fs::remove_dir_all(root).unwrap();
    }
}
