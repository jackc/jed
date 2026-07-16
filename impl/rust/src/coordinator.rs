//! Stable cross-process file coordination (spec/design/locking.md).
//!
//! The database file is never itself the lock identity. Five empty files in the persistent
//! `<database>.lock/` bundle carry whole-file OS locks; the database remains one copyable data file.

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

struct LockFiles {
    presence: File,
    arrival: File,
    transition: File,
    writer: File,
    commit: File,
}

struct Inner {
    files: LockFiles,
    state: AtomicU8,
    stop: AtomicBool,
    wake_mu: Mutex<()>,
    wake: Condvar,
}

/// One registered participant in a file's stable lock domain.
pub(crate) struct FileCoordinator {
    path: PathBuf,
    inner: Arc<Inner>,
    probe: Mutex<Option<JoinHandle<()>>>,
    opener_pid: u32,
}

/// An RAII whole-file lock used for the short commit/meta adoption gates.
pub(crate) struct FileLockGuard<'a> {
    file: &'a File,
}

impl Drop for FileLockGuard<'_> {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl FileCoordinator {
    /// Prepare coordination for an existing database before any database byte is read.
    pub(crate) fn open(path: &Path, mode: Locking, timeout_ms: u64) -> Result<Option<Self>> {
        let mode = resolve_mode(mode)?;
        if mode == Locking::None {
            return Ok(None);
        }
        let path = fs::canonicalize(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                EngineError::new(
                    SqlState::UndefinedFile,
                    format!("database file does not exist: {}", path.display()),
                )
            } else {
                io_error("resolve database path", e)
            }
        })?;
        reject_hard_link(&path)?;
        Self::acquire(path, mode, timeout_ms)
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
        Self::acquire(parent.join(name), mode, timeout_ms)
    }

    fn acquire(path: PathBuf, mode: Locking, timeout_ms: u64) -> Result<Option<Self>> {
        register_path(&path)?;
        let result = (|| {
            let files = open_bundle(&path)?;
            let deadline = deadline(timeout_ms);
            lock_until(&files.arrival, LockKind::Shared, deadline, &path, false)?;
            validate_protocol_marker(&path)?;

            let state = if mode == Locking::Exclusive {
                loop {
                    lock_until(
                        &files.transition,
                        LockKind::Exclusive,
                        deadline,
                        &path,
                        false,
                    )?;
                    match try_lock(&files.presence, LockKind::Exclusive)? {
                        true => {
                            files
                                .transition
                                .unlock()
                                .map_err(|e| lock_error(e, &path))?;
                            break LeaseState::Exclusive;
                        }
                        false => {
                            files
                                .transition
                                .unlock()
                                .map_err(|e| lock_error(e, &path))?;
                            check_deadline(deadline, &path, false)?;
                            thread::sleep(RETRY_INTERVAL);
                        }
                    }
                }
            } else {
                // A first opener takes presence EX and immediately gets the fast path. A participant
                // already present makes the EX attempt fail; its arrival lock then drives the holder's
                // downgrade and this opener joins with presence SH.
                lock_until(
                    &files.transition,
                    LockKind::Exclusive,
                    deadline,
                    &path,
                    false,
                )?;
                let alone = try_lock(&files.presence, LockKind::Exclusive)?;
                files
                    .transition
                    .unlock()
                    .map_err(|e| lock_error(e, &path))?;
                if alone {
                    LeaseState::Alone
                } else {
                    lock_until(&files.presence, LockKind::Shared, deadline, &path, false)?;
                    LeaseState::Shared
                }
            };
            files.arrival.unlock().map_err(|e| lock_error(e, &path))?;
            Ok(FileCoordinator {
                path: path.clone(),
                inner: Arc::new(Inner {
                    files,
                    state: AtomicU8::new(state as u8),
                    stop: AtomicBool::new(false),
                    wake_mu: Mutex::new(()),
                    wake: Condvar::new(),
                }),
                probe: Mutex::new(None),
                opener_pid: std::process::id(),
            })
        })();
        match result {
            Ok(coordinator) => Ok(Some(coordinator)),
            Err(e) => {
                unregister_path(&path);
                Err(e)
            }
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn state(&self) -> LeaseState {
        LeaseState::from_u8(self.inner.state.load(Ordering::Acquire))
    }

    pub(crate) fn set_state(&self, state: LeaseState) {
        self.inner.state.store(state as u8, Ordering::Release);
    }

    pub(crate) fn check_pid(&self) -> Result<()> {
        if std::process::id() == self.opener_pid {
            Ok(())
        } else {
            Err(EngineError::new(
                SqlState::ObjectInUse,
                "database handle was inherited across fork; reopen it in the child",
            ))
        }
    }

    pub(crate) fn lock_commit_shared(&self) -> Result<FileLockGuard<'_>> {
        self.inner
            .files
            .commit
            .lock_shared()
            .map_err(|e| lock_error(e, &self.path))?;
        Ok(FileLockGuard {
            file: &self.inner.files.commit,
        })
    }

    pub(crate) fn lock_commit_exclusive(&self) -> Result<FileLockGuard<'_>> {
        self.inner
            .files
            .commit
            .lock()
            .map_err(|e| lock_error(e, &self.path))?;
        Ok(FileLockGuard {
            file: &self.inner.files.commit,
        })
    }

    pub(crate) fn lock_writer(&self, timeout_ms: u64) -> Result<()> {
        lock_until(
            &self.inner.files.writer,
            LockKind::Exclusive,
            if timeout_ms == 0 {
                None
            } else {
                deadline(timeout_ms)
            },
            &self.path,
            true,
        )
    }

    pub(crate) fn unlock_writer(&self) {
        let _ = self.inner.files.writer.unlock();
    }

    pub(crate) fn lock_transition(&self) -> Result<FileLockGuard<'_>> {
        self.inner
            .files
            .transition
            .lock()
            .map_err(|e| lock_error(e, &self.path))?;
        Ok(FileLockGuard {
            file: &self.inner.files.transition,
        })
    }

    pub(crate) fn try_arrival_exclusive(&self) -> Result<Option<FileLockGuard<'_>>> {
        if try_lock(&self.inner.files.arrival, LockKind::Exclusive)? {
            Ok(Some(FileLockGuard {
                file: &self.inner.files.arrival,
            }))
        } else {
            Ok(None)
        }
    }

    /// Convert the held presence EX lease to SH. The caller holds transition EX and the local barrier.
    pub(crate) fn downgrade_presence(&self) -> Result<()> {
        self.set_state(LeaseState::Shared);
        self.inner
            .files
            .presence
            .unlock()
            .map_err(|e| lock_error(e, &self.path))?;
        self.inner
            .files
            .presence
            .lock_shared()
            .map_err(|e| lock_error(e, &self.path))
    }

    /// Try SH→EX while arrival EX and transition EX make the temporary unlock invisible.
    pub(crate) fn try_upgrade_presence(&self) -> Result<bool> {
        self.inner
            .files
            .presence
            .unlock()
            .map_err(|e| lock_error(e, &self.path))?;
        if try_lock(&self.inner.files.presence, LockKind::Exclusive)? {
            Ok(true)
        } else {
            self.inner
                .files
                .presence
                .lock_shared()
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
        if slot.is_some() {
            return;
        }
        let inner = Arc::clone(&self.inner);
        *slot = Some(thread::spawn(move || {
            loop {
                let guard = inner.wake_mu.lock().expect("probe wake lock poisoned");
                let (_guard, wait) = inner
                    .wake
                    .wait_timeout_while(guard, POLL_INTERVAL, |_| {
                        !inner.stop.load(Ordering::Acquire)
                    })
                    .expect("probe wake lock poisoned");
                if inner.stop.load(Ordering::Acquire) {
                    break;
                }
                if wait.timed_out() {
                    callback();
                }
            }
        }));
    }
}

impl Drop for FileCoordinator {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Release);
        self.inner.wake.notify_all();
        if let Some(handle) = self.probe.lock().expect("probe lock poisoned").take() {
            if handle.thread().id() != thread::current().id() {
                let _ = handle.join();
            }
        }
        unregister_path(&self.path);
    }
}

#[derive(Clone, Copy)]
enum LockKind {
    Shared,
    Exclusive,
}

fn try_lock(file: &File, kind: LockKind) -> Result<bool> {
    let result = match kind {
        LockKind::Shared => file.try_lock_shared(),
        LockKind::Exclusive => file.try_lock(),
    };
    match result {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(e)) => Err(classify_lock_error(e)),
    }
}

fn lock_until(
    file: &File,
    kind: LockKind,
    deadline: Option<Instant>,
    path: &Path,
    writer: bool,
) -> Result<()> {
    loop {
        if try_lock(file, kind)? {
            return Ok(());
        }
        check_deadline(deadline, path, writer)?;
        thread::sleep(RETRY_INTERVAL);
    }
}

fn deadline(timeout_ms: u64) -> Option<Instant> {
    Some(Instant::now() + Duration::from_millis(timeout_ms))
}

fn check_deadline(deadline: Option<Instant>, path: &Path, writer: bool) -> Result<()> {
    if deadline.is_some_and(|d| Instant::now() >= d) {
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

fn register_path(path: &Path) -> Result<()> {
    let mut paths = OPEN_PATHS.lock().expect("open-path registry poisoned");
    if !paths.insert(path.to_path_buf()) {
        return Err(EngineError::new(
            SqlState::ObjectInUse,
            format!(
                "database is already open in this process: {} (share one Database handle)",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn unregister_path(path: &Path) {
    OPEN_PATHS
        .lock()
        .expect("open-path registry poisoned")
        .remove(path);
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
        presence: open_regular(&bundle.join("presence"))?,
        arrival: open_regular(&bundle.join("arrival"))?,
        transition: open_regular(&bundle.join("transition"))?,
        writer: open_regular(&bundle.join("writer"))?,
        commit: open_regular(&bundle.join("commit"))?,
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

fn lock_error(error: std::io::Error, path: &Path) -> EngineError {
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
