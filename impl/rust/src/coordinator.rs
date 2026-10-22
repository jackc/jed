//! Cross-process file coordination (spec/design/locking.md).
//!
//! The database file is never itself the lock identity. Empty files in the persistent
//! `<database>.lock/` bundle carry whole-file OS locks; the database remains one copyable data file.
//! presence EX is ownership: a page-service owner (page-service.md §4.1), an exclusive opener, or a
//! file attachment holds it for its whole tenure; arrival SH guards the protocol-marker check.
//!
//! Every lock operation goes through a [`LockHost`]: the lock bundle, whole-file locks on it, the
//! clock deadlines read, the pause between polls of a contended lock, and the process-local
//! duplicate-open registry. Production uses the OS ([`OsLockHost`]). The deterministic simulator
//! (spec/design/simulation-testing.md §14 S5) substitutes simulated processes sharing one lock
//! table, so the protocol runs, crashes, and interleaves deterministically.

use std::collections::HashSet;
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::{EngineError, Result, SqlState};
use crate::file::Locking;

/// The bundle's protocol: v2 is presence ownership under the page service. The legacy shared-mode
/// marker may remain in an existing bundle; a v1 binary rejects the v2 marker, so the two never run
/// on one file (page-service.md §12).
const PROTOCOL_MARKER: &str = "protocol-v2";
const LEGACY_PROTOCOL_MARKER: &str = "protocol-v1";
const LOCK_NAMES: [&str; 2] = ["presence", "arrival"];
const RETRY_INTERVAL: Duration = Duration::from_millis(2);

static OPEN_PATHS: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// One bundle file's lock, opaque to the coordinator: a [`File`] for the OS host.
pub(crate) type LockHandle = Box<dyn std::any::Any + Send + Sync>;

/// The bundle locks of one participant.
pub(crate) struct LockFiles {
    pub(crate) presence: LockHandle,
    pub(crate) arrival: LockHandle,
}

/// Everything the coordinator needs from its environment (see the module docs).
pub(crate) trait LockHost: Send + Sync {
    /// Register `path` as open in this process; false if it already is.
    fn claim_path(&self, path: &Path) -> bool;
    fn release_path(&self, path: &Path);
    /// Create or open the lock bundle beside `path` and open its lock handles.
    fn open_bundle(&self, path: &Path) -> Result<LockFiles>;
    /// Re-validate the protocol marker (held under arrival SH, the migration barrier).
    fn validate_marker(&self, path: &Path) -> Result<()>;
    /// Release this participant's hold on its bundle (the OS closes the files when they drop).
    fn close_files(&self, files: &LockFiles);
    /// Take or convert the lock without waiting: `Ok(false)` when another holder conflicts.
    fn try_lock(&self, handle: &LockHandle, exclusive: bool) -> std::io::Result<bool>;
    fn unlock(&self, handle: &LockHandle) -> std::io::Result<()>;
    /// The clock lock deadlines read.
    fn now(&self) -> Instant;
    /// Wait between polls of a contended lock.
    fn pause(&self);
    /// The process id the fork check compares.
    fn pid(&self) -> u32;

    // The page service's host operations (spec/pageservice/README.md §5): the owner record in the
    // bundle, this user's id, and the owner's endpoint. The simulator substitutes an in-memory
    // network.

    /// The owner record of the database at `db_path`, or `None` when absent or unparsable.
    #[cfg(unix)]
    fn read_owner_record(&self, db_path: &Path) -> Option<crate::pageservice::codec::OwnerRecord>;
    #[cfg(unix)]
    fn write_owner_record(
        &self,
        db_path: &Path,
        record: &crate::pageservice::codec::OwnerRecord,
    ) -> Result<()>;
    #[cfg(unix)]
    fn remove_owner_record(&self, db_path: &Path);
    /// This process's user id, as the owner record names it.
    #[cfg(unix)]
    fn current_uid(&self, db_path: &Path) -> Result<u32>;
    /// Where the owner of `db_path` binds its endpoint; `None` when it cannot serve.
    #[cfg(unix)]
    fn endpoint_path(
        &self,
        db_path: &Path,
        uid: u32,
        incarnation: &crate::pageservice::codec::Id16,
    ) -> Option<PathBuf>;
    #[cfg(unix)]
    fn listen(
        &self,
        endpoint: &Path,
    ) -> std::io::Result<Box<dyn crate::pageservice::net::PsListener>>;
    #[cfg(unix)]
    fn dial(&self, endpoint: &str) -> std::io::Result<Box<dyn crate::pageservice::net::PsStream>>;
    #[cfg(unix)]
    fn remove_endpoint(&self, endpoint: &Path);
    /// Whether page clients dialed through this host read ahead (spec/design/page-service.md §5.3):
    /// the stream transport's, whose round trips cross a network. A local socket's round trip is
    /// cheap, and its clients fault one leaf at a time.
    #[cfg(unix)]
    fn reads_ahead(&self) -> bool {
        false
    }
    /// The credential page clients dialed through this host send in `hello`
    /// (spec/pageservice/README.md §3.1): the stream transport's, whose serving end may have to own
    /// the database. A local owner already holds the data key, so a local socket's clients send none.
    #[cfg(unix)]
    fn hello_credential(&self) -> Option<crate::Credential> {
        None
    }
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

    fn unlock(&self, handle: &LockHandle) -> std::io::Result<()> {
        os_file(handle).unlock()
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn pause(&self) {
        thread::sleep(RETRY_INTERVAL);
    }

    fn pid(&self) -> u32 {
        std::process::id()
    }

    #[cfg(unix)]
    fn read_owner_record(&self, db_path: &Path) -> Option<crate::pageservice::codec::OwnerRecord> {
        crate::pageservice::endpoint::read_owner_record(db_path)
    }

    #[cfg(unix)]
    fn write_owner_record(
        &self,
        db_path: &Path,
        record: &crate::pageservice::codec::OwnerRecord,
    ) -> Result<()> {
        crate::pageservice::endpoint::write_owner_record(db_path, record)
    }

    #[cfg(unix)]
    fn remove_owner_record(&self, db_path: &Path) {
        let _ = fs::remove_file(crate::pageservice::endpoint::owner_record_path(db_path));
    }

    #[cfg(unix)]
    fn current_uid(&self, db_path: &Path) -> Result<u32> {
        use crate::pageservice::endpoint;
        endpoint::current_uid(&endpoint::bundle_dir(db_path))
    }

    #[cfg(unix)]
    fn endpoint_path(
        &self,
        db_path: &Path,
        uid: u32,
        incarnation: &crate::pageservice::codec::Id16,
    ) -> Option<PathBuf> {
        crate::pageservice::endpoint::endpoint_path(db_path, uid, incarnation)
    }

    #[cfg(unix)]
    fn listen(
        &self,
        endpoint: &Path,
    ) -> std::io::Result<Box<dyn crate::pageservice::net::PsListener>> {
        // Only a presence-EX holder binds, so a file already at the path is a dead owner's.
        let _ = fs::remove_file(endpoint);
        Ok(Box::new(crate::pageservice::net::UnixEndpoint::bind(
            endpoint.to_path_buf(),
        )?))
    }

    #[cfg(unix)]
    fn dial(&self, endpoint: &str) -> std::io::Result<Box<dyn crate::pageservice::net::PsStream>> {
        crate::pageservice::net::dial_unix(endpoint)
    }

    #[cfg(unix)]
    fn remove_endpoint(&self, endpoint: &Path) {
        let _ = fs::remove_file(endpoint);
    }
}

/// One owner of a file: the holder of presence EX for its whole tenure.
pub(crate) struct FileCoordinator {
    host: Arc<dyn LockHost>,
    path: PathBuf,
    files: LockFiles,
    opener_pid: u32,
    /// A page-service owner (it serves an endpoint); otherwise a non-serving owner — an exclusive
    /// opener or a file attachment.
    serving: bool,
    /// Leave the process-local path claim at drop: a page-service owner's claim belongs to its
    /// handle, which keeps the path through role changes (page-service.md §8).
    keep_path: bool,
    /// This coordinator wrote a non-serving owner record, removed at drop
    /// (spec/pageservice/README.md §5.1).
    record: bool,
}

impl FileCoordinator {
    /// Take ownership of an existing database before any database byte is read: `None` under
    /// `Locking::None`, otherwise a non-serving owner (the page-service open is the election,
    /// [`crate::pageservice::election`]). A caller resolves `Locking::Auto` first.
    pub(crate) fn open(path: &Path, mode: Locking, timeout_ms: u64) -> Result<Option<Self>> {
        let mode = resolve_mode(mode)?;
        if mode == Locking::None {
            return Ok(None);
        }
        let path = resolve_open_path(path)?;
        let mut coordinator = Self::acquire(Arc::new(OsLockHost), path, false, timeout_ms)?;
        coordinator.write_non_serving_record();
        Ok(Some(coordinator))
    }

    /// The registered participant of a page-service owner elected at open
    /// (spec/pageservice/README.md §6.1): presence EX held for its whole tenure. `files` are the
    /// bundle locks the election opened; the path is already claimed.
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
            opener_pid,
            serving: true,
            keep_path: true,
            record: false,
        }
    }

    /// Take ownership of an absent database path before the initial image is published. Under
    /// `Locking::Auto` (where the host serves) the creator becomes the serving owner — its record is
    /// written when it serves — and under `Locking::Exclusive` a non-serving owner.
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
        let serving = mode == Locking::Auto;
        let mut coordinator =
            Self::acquire(Arc::new(OsLockHost), parent.join(name), serving, timeout_ms)?;
        if !serving {
            coordinator.write_non_serving_record();
        }
        Ok(Some(coordinator))
    }

    /// Take presence EX for `path` through `host` with the exclusive acquisition loop, until the
    /// open timeout (`55006`, worded from the owner record when there is one). `serving` marks a
    /// page-service owner, which keeps the process-local path claim at drop.
    pub(crate) fn acquire(
        host: Arc<dyn LockHost>,
        path: PathBuf,
        serving: bool,
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
            let owned = h.validate_marker(&path).and_then(|()| {
                lock_until(h, &files.presence, true, deadline, &path, false).map_err(|e| {
                    match owner_reason(h, &path) {
                        Some(reason) => EngineError::new(
                            SqlState::ObjectInUse,
                            format!("database file is in use: {} ({reason})", path.display()),
                        ),
                        None => e,
                    }
                })
            });
            let unlocked = h.unlock(&files.arrival).map_err(|e| lock_error(e, &path));
            owned.and(unlocked)
        })();
        match result {
            Ok(()) => {
                let opener_pid = host.pid();
                Ok(FileCoordinator {
                    host,
                    path,
                    files,
                    opener_pid,
                    serving,
                    keep_path: serving,
                    record: false,
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

    /// Publish a non-serving owner record (flags 0, empty endpoint — README.md §5.1), so a
    /// page-service opener words its timeout as "owned by a non-serving process". Best effort: a
    /// missing record only loses that wording.
    fn write_non_serving_record(&mut self) {
        #[cfg(unix)]
        {
            use crate::pageservice::{codec::OwnerRecord, owner::new_incarnation};
            let written = (|| {
                let record = OwnerRecord {
                    serving: false,
                    uid: self.host.current_uid(&self.path)?,
                    incarnation: new_incarnation()?,
                    pid: std::process::id(),
                    endpoint: String::new(),
                };
                self.host.write_owner_record(&self.path, &record)
            })();
            self.record = written.is_ok();
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this is a page-service owner (it serves an endpoint).
    pub(crate) fn serving(&self) -> bool {
        self.serving
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
}

impl Drop for FileCoordinator {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.record {
            self.host.remove_owner_record(&self.path);
        }
        self.host.close_files(&self.files);
        if !self.keep_path {
            self.host.release_path(&self.path);
        }
    }
}

/// The owner record's wording for a presence EX this opener could not take (README.md §6.1).
#[cfg(unix)]
fn owner_reason(host: &dyn LockHost, path: &Path) -> Option<&'static str> {
    let record = host.read_owner_record(path)?;
    let uid = host.current_uid(path).ok()?;
    if record.uid != uid {
        Some("owned by another user's process")
    } else if !record.serving {
        Some("owned by a non-serving process")
    } else {
        None
    }
}

#[cfg(not(unix))]
fn owner_reason(_host: &dyn LockHost, _path: &Path) -> Option<&'static str> {
    None
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

/// Resolve a locking mode: `Auto` is the page service where the host serves (Unix —
/// page-service.md §14), otherwise a non-serving owner (`Exclusive`). Without OS file locks every
/// mode but `None` is `0A000`.
pub(crate) fn resolve_mode(mode: Locking) -> Result<Locking> {
    if mode == Locking::None {
        return Ok(mode);
    }
    if !cfg!(any(unix, windows)) {
        return Err(EngineError::new(
            SqlState::FeatureNotSupported,
            "file locking is unavailable on this host; use locking=none only with external coordination",
        ));
    }
    if mode == Locking::Auto && !cfg!(unix) {
        return Ok(Locking::Exclusive);
    }
    Ok(mode)
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
    for name in LOCK_NAMES {
        ensure_regular_file(&bundle.join(name))?;
    }
    Ok(LockFiles {
        presence: Box::new(open_regular(&bundle.join("presence"))?),
        arrival: Box::new(open_regular(&bundle.join("arrival"))?),
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
        // A legacy shared-mode marker (with its stale lock files) is tolerated.
        if name.starts_with("protocol-v")
            && name != PROTOCOL_MARKER
            && name != LEGACY_PROTOCOL_MARKER
        {
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
        let first = FileCoordinator::open(&path, Locking::Exclusive, 0)
            .unwrap()
            .unwrap();
        let err = match FileCoordinator::open(&path, Locking::Exclusive, 0) {
            Ok(_) => panic!("duplicate coordinated open unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(err.state, SqlState::ObjectInUse);
        drop(first);
        assert!(root.join("db.jed.lock/protocol-v2").is_file());
        assert!(!root.join("db.jed.lock/transition").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_marker_is_tolerated_and_unknown_markers_fail() {
        let root = std::env::temp_dir().join(format!("jed-lock-legacy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("db.jed.lock")).unwrap();
        let path = root.join("db.jed");
        fs::write(&path, b"x").unwrap();
        for stale in ["protocol-v1", "transition", "writer", "commit"] {
            fs::write(root.join("db.jed.lock").join(stale), b"").unwrap();
        }
        drop(
            FileCoordinator::open(&path, Locking::Exclusive, 0)
                .unwrap()
                .unwrap(),
        );
        fs::write(root.join("db.jed.lock/protocol-v3"), b"").unwrap();
        let err = match FileCoordinator::open(&path, Locking::Exclusive, 0) {
            Ok(_) => panic!("an unknown protocol marker unexpectedly opened"),
            Err(error) => error,
        };
        assert_eq!(err.state, SqlState::FeatureNotSupported);
        fs::remove_dir_all(root).unwrap();
    }
}
