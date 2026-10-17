//! The page-service endpoint and owner record on the host filesystem (spec/pageservice/README.md
//! §5): where an owner binds its socket, how a directory proves it is private to one user, and the
//! owner record clients read to find the endpoint. Unix only: Windows serving is not yet specified
//! (page-service.md §14), so `Locking::Auto` resolves to a non-serving owner there and none of this
//! is reached.

use std::path::{Path, PathBuf};

use crate::error::{EngineError, Result, SqlState};
use crate::pageservice::codec::{
    ENDPOINT_MAX_PATH_BYTES, Id16, OwnerRecord, decode_owner_record, encode_owner_record,
};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The coordination bundle beside a database (locking.md §3.1).
pub(crate) fn bundle_dir(db_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", db_path.display()))
}

/// The owner record's place in the bundle (README.md §5.1).
pub(crate) fn owner_record_path(db_path: &Path) -> PathBuf {
    bundle_dir(db_path).join("owner")
}

/// Publish `record` atomically: a temp file named by the incarnation, then a rename, so a reader
/// never sees a torn record.
pub(crate) fn write_owner_record(db_path: &Path, record: &OwnerRecord) -> Result<()> {
    let bytes = encode_owner_record(record)?;
    let target = owner_record_path(db_path);
    let tmp = bundle_dir(db_path).join(format!("owner.tmp-{}", hex(&record.incarnation)));
    std::fs::write(&tmp, &bytes)
        .map_err(|e| EngineError::new(SqlState::IoError, format!("write owner record: {e}")))?;
    std::fs::rename(&tmp, &target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        EngineError::new(SqlState::IoError, format!("publish owner record: {e}"))
    })
}

/// The current owner record, or `None` when it is absent or unparsable (README.md §5.1: such a
/// record is treated as absent).
pub(crate) fn read_owner_record(db_path: &Path) -> Option<OwnerRecord> {
    let bytes = std::fs::read(owner_record_path(db_path)).ok()?;
    decode_owner_record(&bytes).ok()
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};

    static UID: OnceLock<u32> = OnceLock::new();
    static PROBES: AtomicU64 = AtomicU64::new(0);

    /// This process's effective uid, read without libc as the owner of a directory it creates in
    /// `dir` (any directory it may write — the coordination bundle). Cached: it never changes.
    pub(crate) fn current_uid(dir: &Path) -> Result<u32> {
        if let Some(uid) = UID.get() {
            return Ok(*uid);
        }
        let probe = dir.join(format!(
            "uid-probe-{}-{}",
            std::process::id(),
            PROBES.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&probe)
            .map_err(|e| EngineError::new(SqlState::IoError, format!("probe user id: {e}")))?;
        let uid = std::fs::symlink_metadata(&probe).map(|m| m.uid());
        let _ = std::fs::remove_dir(&probe);
        let uid =
            uid.map_err(|e| EngineError::new(SqlState::IoError, format!("probe user id: {e}")))?;
        Ok(*UID.get_or_init(|| uid))
    }

    /// Whether `dir` is a real directory (not a symlink) owned by `uid` with mode exactly `0700` —
    /// the check every endpoint directory must pass (README.md §5.2).
    pub(crate) fn private_dir(dir: &Path, uid: u32) -> bool {
        match std::fs::symlink_metadata(dir) {
            Ok(meta) => {
                meta.file_type().is_dir()
                    && !meta.file_type().is_symlink()
                    && meta.permissions().mode() & 0o7777 == 0o700
                    && meta.uid() == uid
            }
            Err(_) => false,
        }
    }

    /// Create `dir` with mode `0700` when it is absent, then check it.
    pub(crate) fn ensure_private_dir(dir: &Path, uid: u32) -> bool {
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {
                // The umask may have narrowed the mode further; state it exactly.
                let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return false,
        }
        private_dir(dir, uid)
    }

    fn fits(path: &Path) -> bool {
        path.as_os_str().len() <= ENDPOINT_MAX_PATH_BYTES
    }

    /// The owner's socket path (README.md §5.2): the bundle's per-user directory when the path fits,
    /// otherwise a runtime directory. `None` means the owner cannot serve.
    pub(crate) fn endpoint_path(db_path: &Path, uid: u32, incarnation: &Id16) -> Option<PathBuf> {
        let user_dir = bundle_dir(db_path).join(format!("uid-{uid}"));
        let bundled = user_dir.join("endpoint");
        if fits(&bundled) && ensure_private_dir(&user_dir, uid) {
            return Some(bundled);
        }
        let name = format!("jed-{}.sock", hex(&incarnation[..8]));
        let mut candidates: Vec<PathBuf> = Vec::new();
        let runtime_var = if cfg!(target_os = "linux") {
            Some("XDG_RUNTIME_DIR")
        } else if cfg!(target_os = "macos") {
            Some("TMPDIR")
        } else {
            None
        };
        if let Some(var) = runtime_var
            && let Some(dir) = std::env::var_os(var)
            && !dir.is_empty()
        {
            candidates.push(PathBuf::from(dir).components().collect());
        }
        for dir in candidates {
            let path = dir.join(&name);
            if fits(&path) && private_dir(&dir, uid) {
                return Some(path);
            }
        }
        let dir = PathBuf::from(format!("/tmp/jed-{uid}"));
        let path = dir.join(&name);
        if fits(&path) && ensure_private_dir(&dir, uid) {
            return Some(path);
        }
        None
    }
}

#[cfg(unix)]
pub(crate) use unix::{current_uid, endpoint_path};

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jps-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn endpoint_prefers_the_private_bundle_directory() {
        let dir = scratch("ep");
        let db = dir.join("d.jed");
        std::fs::create_dir(bundle_dir(&db)).unwrap();
        let uid = current_uid(&bundle_dir(&db)).unwrap();
        let path = endpoint_path(&db, uid, &[7; 16]).unwrap();
        assert_eq!(
            path,
            bundle_dir(&db).join(format!("uid-{uid}")).join("endpoint")
        );
        assert!(unix::private_dir(path.parent().unwrap(), uid));
        // A directory another uid would own (or a wrong mode) fails the check.
        assert!(!unix::private_dir(
            path.parent().unwrap(),
            uid.wrapping_add(1)
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_long_bundle_path_falls_back_to_a_runtime_socket() {
        let dir = scratch("eplong");
        let db = dir.join("x".repeat(120));
        std::fs::create_dir(bundle_dir(&db)).unwrap();
        let uid = current_uid(&bundle_dir(&db)).unwrap();
        let incarnation = [0xab; 16];
        // The runtime directory may legitimately fail the private check on a test host (then the
        // owner does not serve); when one is chosen it is named by the incarnation and fits.
        if let Some(path) = endpoint_path(&db, uid, &incarnation) {
            assert!(path.as_os_str().len() <= ENDPOINT_MAX_PATH_BYTES);
            assert_eq!(path.file_name().unwrap(), "jed-abababababababab.sock");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn owner_record_round_trips_and_garbage_reads_as_absent() {
        let dir = scratch("rec");
        let db = dir.join("d.jed");
        std::fs::create_dir(bundle_dir(&db)).unwrap();
        assert!(read_owner_record(&db).is_none());
        let record = OwnerRecord {
            serving: true,
            uid: 1000,
            incarnation: [3; 16],
            pid: 42,
            endpoint: "/tmp/x.sock".into(),
        };
        write_owner_record(&db, &record).unwrap();
        assert_eq!(read_owner_record(&db), Some(record));
        std::fs::write(owner_record_path(&db), b"JEDOWNER garbage").unwrap();
        assert!(read_owner_record(&db).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
