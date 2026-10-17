//! Election under the page service (`Locking::Auto`; spec/pageservice/README.md §6.1, §6.5): the
//! opener that takes presence EX owns the database and serves it; every other opener connects to
//! the owner's endpoint as a page client. A client whose connection ends runs the same election
//! again (re-election). The OS lock is the only authority — the owner record only tells a client
//! where to connect and words its timeout error.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use crate::coordinator::{
    FileCoordinator, LockFiles, LockHost, check_deadline, deadline, lock_error, lock_until,
    try_lock,
};
use crate::error::{EngineError, Result, SqlState};
use crate::executor::Engine;
use crate::file::OpenOptions;
use crate::pageservice::client::{
    ClientLink, ClientPins, Connect, PsConnection, ServiceBlockStore, connect,
};

/// The outcome of an election: the owner's coordinator, or a connected client with its engine over
/// the connection.
pub(crate) enum Election {
    Owner(FileCoordinator),
    Client(Arc<PsConnection>, Box<Engine>),
}

/// One election's shape. `never` skips presence tries (`Ownership::Never`); `pause_until` skips them
/// until then (the handoff pause); `reelect` marks a re-election, which keeps the handle's existing
/// process-local path claim. `opts` open the client's engine.
pub(crate) struct ElectOptions<'a> {
    pub(crate) timeout_ms: u64,
    pub(crate) read_only: bool,
    pub(crate) never: bool,
    pub(crate) pause_until: Option<Instant>,
    pub(crate) reelect: bool,
    pub(crate) opts: &'a OpenOptions,
}

/// Run the election for the database at `path` (already resolved as a coordinated open resolves
/// it), retrying until the open timeout: presence may free up, a new owner may not have bound its
/// endpoint yet, and a draining owner closes the connection of a client that opened during the
/// drain.
pub(crate) fn elect(
    host: &Arc<dyn LockHost>,
    path: &Path,
    eo: &ElectOptions<'_>,
) -> Result<Election> {
    if !eo.reelect && !host.claim_path(path) {
        return Err(EngineError::new(
            SqlState::ObjectInUse,
            format!(
                "database is already open in this process: {} (share one Database handle)",
                path.display()
            ),
        ));
    }
    let release = || {
        if !eo.reelect {
            host.release_path(path);
        }
    };
    let files = match host.open_bundle(path) {
        Ok(files) => files,
        Err(e) => {
            release();
            return Err(e);
        }
    };
    match run(host, path, &files, eo) {
        Ok(None) => Ok(Election::Owner(FileCoordinator::elected_owner(
            Arc::clone(host),
            path.to_path_buf(),
            files,
        ))),
        Ok(Some((conn, engine))) => {
            // A client holds no bundle lock while connected (page-service.md §4.2), only the
            // handle's process-local path claim.
            host.close_files(&files);
            drop(files);
            Ok(Election::Client(conn, Box::new(engine)))
        }
        Err(e) => {
            host.close_files(&files);
            drop(files);
            release();
            Err(e)
        }
    }
}

/// The election loop under arrival SH: `None` when this opener took presence EX (the owner), the
/// opened client when it became one. Arrival is released on every return.
fn run(
    host_arc: &Arc<dyn LockHost>,
    path: &Path,
    files: &LockFiles,
    eo: &ElectOptions<'_>,
) -> Result<Option<(Arc<PsConnection>, Engine)>> {
    let host = host_arc.as_ref();
    let deadline = deadline(host, eo.timeout_ms);
    lock_until(host, &files.arrival, false, deadline, path, false)?;
    let result = (|| {
        host.validate_marker(path)?;
        loop {
            let paused = eo.pause_until.is_some_and(|until| host.now() < until);
            if !eo.never && !paused && try_lock(host, &files.presence, true)? {
                return Ok(None);
            }
            // The last record read words the timeout error (README.md §6.1 step 3).
            let reason = match host.read_owner_record(path) {
                None => "",
                Some(record) if record.uid != host.current_uid(path)? => {
                    "owned by another user's process"
                }
                Some(record) if !record.serving => "owned by a non-serving process",
                Some(record) => match connect(host_arc, path, &record.endpoint, eo.read_only) {
                    Connect::Connected(conn) => match open_client_engine(&conn, eo.opts) {
                        Ok(engine) => return Ok(Some((conn, engine))),
                        Err(e) => {
                            conn.close();
                            if e.state != SqlState::ConnectionFailure {
                                return Err(e);
                            }
                            ""
                        }
                    },
                    Connect::Retry => "",
                    // A re-electing client keeps trying past an owner's connection limit
                    // (README.md §6.5); an opener fails at once.
                    Connect::Refused(e)
                        if e.state == SqlState::TooManyConnections && eo.reelect =>
                    {
                        ""
                    }
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

/// Build the engine of a connected page client (README.md §6.3): one begin_read pins and adopts the
/// owner's committed snapshot, the ordinary loader reads the catalog skeleton through the service,
/// and the open's pin is released — no local reader follows the open; every read begins with its
/// own begin_read. The engine is read-only exactly when the connection is.
fn open_client_engine(conn: &Arc<PsConnection>, opts: &OpenOptions) -> Result<Engine> {
    let begun = conn.begin_read([0; 16], 0)?;
    let mut link = ClientLink {
        conn: Arc::clone(conn),
        pins: ClientPins::default(),
    };
    link.take(begun.txid);
    {
        let mut cache = conn.cache.lock().expect("client cache lock poisoned");
        cache.incarnation = begun.incarnation;
        cache.adopted = begun.txid;
    }
    conn.set_meta(begun.meta);
    let loaded = (|| {
        let mut pager =
            crate::pager::Pager::from_store(Box::new(ServiceBlockStore::new(Arc::clone(conn))))?;
        pager.set_trust_meta();
        let capacity = crate::paging::cache_leaves(opts.cache_bytes, pager.page_size());
        Engine::open_paged(pager, capacity, &opts.environment)
    })();
    link.release(begun.txid);
    let mut engine = loaded?;
    engine.path = Some(conn.db_path().to_path_buf());
    // The handle's spill host replaces it when the role is installed (`ServiceHandoff::attach_host`).
    engine.spill = Some(crate::spillhost::os_spill_host(std::env::temp_dir()));
    engine.read_only = conn.read_only();
    if opts.work_mem != 0 {
        engine.session.work_mem = opts.work_mem;
    }
    Ok(engine)
}
