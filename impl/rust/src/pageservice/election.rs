//! Election at open under the test-only `Locking::Service` (spec/pageservice/README.md §6.1): the
//! opener that takes presence EX owns the database and serves it; every other opener connects to
//! the owner's endpoint as a read-only page client (phase 2). The OS lock is the only authority — the
//! owner record only tells a client where to connect and words its timeout error.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::coordinator::{
    FileCoordinator, LockFiles, LockHost, OsLockHost, check_deadline, deadline, lock_error,
    lock_until, try_lock,
};
use crate::error::{EngineError, Result, SqlState};
use crate::pageservice::client::{Connect, PathClaim, PsConnection, connect};
use crate::pageservice::endpoint;

/// The outcome of an election: the owner's coordinator, or a connected client.
pub(crate) enum Election {
    Owner(FileCoordinator),
    Client(Arc<PsConnection>),
}

/// Run the election for the database at `path` (already resolved as a coordinated open resolves
/// it), retrying until `timeout_ms`: presence may free up, and a new owner may not have bound its
/// endpoint yet.
pub(crate) fn elect(path: PathBuf, timeout_ms: u64) -> Result<Election> {
    let host: Arc<dyn LockHost> = Arc::new(OsLockHost);
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
    match run(host.as_ref(), &path, &files, timeout_ms) {
        Ok(None) => Ok(Election::Owner(FileCoordinator::elected_owner(
            host, path, files,
        ))),
        Ok(Some(conn)) => {
            // A client holds no bundle lock while connected (page-service.md §4.2), only the
            // process-local path claim.
            host.close_files(&files);
            drop(files);
            conn.keep_claim(PathClaim { host, path });
            Ok(Election::Client(conn))
        }
        Err(e) => {
            host.close_files(&files);
            drop(files);
            host.release_path(&path);
            Err(e)
        }
    }
}

/// The election loop under arrival SH: `None` when this opener took presence EX (the owner), the
/// connection when it became a client. Arrival is released on every return.
fn run(
    host: &dyn LockHost,
    path: &Path,
    files: &LockFiles,
    timeout_ms: u64,
) -> Result<Option<Arc<PsConnection>>> {
    let deadline = deadline(host, timeout_ms);
    lock_until(host, &files.arrival, false, deadline, path, false)?;
    let result = (|| {
        host.validate_marker(path)?;
        loop {
            if try_lock(host, &files.presence, true)? {
                return Ok(None);
            }
            // The last record read words the timeout error (README.md §6.1 step 3).
            let reason = match endpoint::read_owner_record(path) {
                None => "",
                Some(record)
                    if record.uid != endpoint::current_uid(&endpoint::bundle_dir(path))? =>
                {
                    "owned by another user's process"
                }
                Some(record) if !record.serving => "owned by a non-serving process",
                Some(record) => match connect(path, &record.endpoint) {
                    Connect::Connected(conn) => return Ok(Some(conn)),
                    Connect::Retry => "",
                    Connect::Refused(e) => return Err(e),
                },
            };
            if let Err(e) = check_deadline(host, deadline, path, false) {
                if reason.is_empty() {
                    return Err(e);
                }
                return Err(EngineError::new(
                    SqlState::ObjectInUse,
                    format!("database file is in use: {} ({reason})", path.display()),
                ));
            }
            host.pause();
        }
    })();
    host.unlock(&files.arrival)
        .map_err(|e| lock_error(e, path))?;
    result
}
