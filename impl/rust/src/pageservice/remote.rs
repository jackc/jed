//! The stream transport (spec/design/page-service.md §2, §13; spec/pageservice/README.md §5.4): the
//! same protocol over a pair of pipes instead of a local socket, which is how a client on another
//! machine reaches an owner over SSH.
//!
//! - **The remote client** ([`RemoteLockHost`]) runs a command (for the CLI, `ssh host jed pages
//!   /path`) whose stdin and stdout carry the connection. It never owns: there is no bundle to lock
//!   here, so the election always finds a serving owner and dials it by running the command again.
//!   Re-election and `commit_status` therefore work exactly as for a local client.
//! - **The serving side** ([`serve_pages`], the `jed pages` command) relays the stream to the
//!   database's live owner when there is one, and otherwise becomes a yielding owner itself and
//!   serves the stream as one more connection, alongside its own endpoint.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::coordinator::{LockFiles, LockHandle, LockHost};
use crate::error::{EngineError, Result, SqlState};
use crate::file::{Locking, OpenOptions, Ownership};
use crate::pageservice::codec::{
    ERROR_FLAG_FATAL, ErrorReply, HELLO_FLAG_READ_ONLY, Id16, Message, OwnerRecord, encode_frame,
    read_frame,
};
use crate::pageservice::net::{PipeStream, PsListener, PsStream};

/// How long the remote client waits between transport attempts.
const REMOTE_RETRY_INTERVAL: Duration = Duration::from_millis(250);
/// How much of the transport command's standard error a failed open reports.
const STDERR_TAIL: usize = 2048;

/// The page-service host of a remote database: every dial runs `command`, and the database is
/// always owned elsewhere. Bundle locks are placeholders that are never contended.
pub(crate) struct RemoteLockHost {
    command: Vec<String>,
    /// Tests dial an in-process server over pipes instead of running a command.
    dialer: Option<Dialer>,
    /// The tail of the last transport's standard error, and how it ended, for a failed open's
    /// message.
    last_failure: Arc<Mutex<String>>,
}

/// An in-process transport: a fresh connection per dial.
pub(crate) type Dialer = Arc<dyn Fn() -> std::io::Result<Box<dyn PsStream>> + Send + Sync>;

static REMOTE_PATHS: std::sync::LazyLock<Mutex<std::collections::HashSet<PathBuf>>> =
    std::sync::LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));

impl RemoteLockHost {
    pub(crate) fn new(command: Vec<String>) -> RemoteLockHost {
        RemoteLockHost {
            command,
            dialer: None,
            last_failure: Arc::new(Mutex::new(String::new())),
        }
    }

    /// A host whose every dial is `dialer` (tests: an in-process `serve_pages` over pipes).
    pub(crate) fn with_dialer(dialer: Dialer) -> RemoteLockHost {
        RemoteLockHost {
            command: vec!["<in-process>".to_string()],
            dialer: Some(dialer),
            last_failure: Arc::new(Mutex::new(String::new())),
        }
    }

    pub(crate) fn command(&self) -> &[String] {
        &self.command
    }

    /// The last transport's standard error (trimmed), if it wrote any.
    pub(crate) fn last_failure(&self) -> String {
        self.last_failure
            .lock()
            .expect("remote failure lock poisoned")
            .trim()
            .to_string()
    }

    fn spawn(&self) -> std::io::Result<Box<dyn PsStream>> {
        let Some((program, args)) = self.command.split_first() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "empty transport command",
            ));
        };
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .inspect_err(|e| {
                *self
                    .last_failure
                    .lock()
                    .expect("remote failure lock poisoned") =
                    format!("could not run {program}: {e}");
            })?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let failure = Arc::clone(&self.last_failure);
        failure
            .lock()
            .expect("remote failure lock poisoned")
            .clear();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 1024];
            while let Ok(n) = stderr.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                let mut text = failure.lock().expect("remote failure lock poisoned");
                text.push_str(&String::from_utf8_lossy(&chunk[..n]));
                if text.len() > STDERR_TAIL {
                    let cut = text.len() - STDERR_TAIL;
                    let cut = (cut..text.len())
                        .find(|&i| text.is_char_boundary(i))
                        .unwrap_or(text.len());
                    text.drain(..cut);
                }
            }
        });
        let child = Arc::new(Mutex::new(child));
        let on_close: Box<dyn FnOnce() + Send> = Box::new(move || {
            // The pipe to the transport is closed first, so a well-behaved transport exits on its
            // own; one that does not is killed. Reaped off the closing thread.
            std::thread::spawn(move || reap(&child));
        });
        Ok(Box::new(PipeStream::new(
            Vec::new(),
            Box::new(stdout),
            Box::new(stdin),
            Some(on_close),
        )))
    }
}

/// Wait briefly for a transport process whose input was closed, then kill it.
fn reap(child: &Mutex<Child>) {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut child = child.lock().expect("transport child lock poisoned");
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

impl LockHost for RemoteLockHost {
    fn claim_path(&self, path: &Path) -> bool {
        REMOTE_PATHS
            .lock()
            .expect("remote path registry poisoned")
            .insert(path.to_path_buf())
    }

    fn release_path(&self, path: &Path) {
        REMOTE_PATHS
            .lock()
            .expect("remote path registry poisoned")
            .remove(path);
    }

    fn open_bundle(&self, _path: &Path) -> Result<LockFiles> {
        Ok(LockFiles {
            presence: Box::new(()) as LockHandle,
            arrival: Box::new(()) as LockHandle,
        })
    }

    fn validate_marker(&self, _path: &Path) -> Result<()> {
        Ok(())
    }

    fn close_files(&self, _files: &LockFiles) {}

    /// Arrival (shared) is always granted; presence (exclusive) never is: the database is owned on
    /// the remote side.
    fn try_lock(&self, _handle: &LockHandle, exclusive: bool) -> std::io::Result<bool> {
        Ok(!exclusive)
    }

    fn unlock(&self, _handle: &LockHandle) -> std::io::Result<()> {
        Ok(())
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn pause(&self) {
        std::thread::sleep(REMOTE_RETRY_INTERVAL);
    }

    fn pid(&self) -> u32 {
        std::process::id()
    }

    /// Always a serving owner of this user, reached by running the command.
    fn read_owner_record(&self, _db_path: &Path) -> Option<OwnerRecord> {
        Some(OwnerRecord {
            serving: true,
            uid: 0,
            incarnation: [0; 16],
            pid: 0,
            endpoint: "remote".into(),
        })
    }

    fn write_owner_record(&self, _db_path: &Path, _record: &OwnerRecord) -> Result<()> {
        Ok(())
    }

    fn remove_owner_record(&self, _db_path: &Path) {}

    fn current_uid(&self, _db_path: &Path) -> Result<u32> {
        Ok(0)
    }

    fn endpoint_path(&self, _db_path: &Path, _uid: u32, _incarnation: &Id16) -> Option<PathBuf> {
        None
    }

    fn listen(&self, _endpoint: &Path) -> std::io::Result<Box<dyn PsListener>> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    fn dial(&self, _endpoint: &str) -> std::io::Result<Box<dyn PsStream>> {
        match &self.dialer {
            Some(dialer) => dialer(),
            None => self.spawn(),
        }
    }

    fn remove_endpoint(&self, _endpoint: &Path) {}
}

/// The options a remote open runs with: a remote client never owns, so it never asks for
/// ownership either.
pub(crate) fn remote_options(mut opts: OpenOptions) -> OpenOptions {
    opts.ownership = Ownership::Never;
    opts.locking = Locking::Auto;
    opts
}

/// Word a failed remote open: an election that timed out without a connection reports the
/// transport's own complaint (`08006`).
pub(crate) fn remote_open_error(host: &RemoteLockHost, e: EngineError) -> EngineError {
    if e.state != SqlState::ObjectInUse && e.state != SqlState::ConnectionFailure {
        return e;
    }
    let detail = host.last_failure();
    let shown = host.command().join(" ");
    EngineError::new(
        SqlState::ConnectionFailure,
        if detail.is_empty() {
            format!("could not reach the database through `{shown}`")
        } else {
            format!("could not reach the database through `{shown}`: {detail}")
        },
    )
}

/// Write a fatal `error` frame answering `request_id`.
fn send_fatal(output: &mut dyn Write, request_id: u32, error: &EngineError) {
    let message = Message::Error(ErrorReply {
        sqlstate: error.code().to_string(),
        message: error.message.clone(),
        detail: String::new(),
        flags: ERROR_FLAG_FATAL,
    });
    if let Ok(frame) = encode_frame(request_id, &message) {
        let _ = output.write_all(&frame);
        let _ = output.flush();
    }
}

/// Serve one page-service connection arriving on `input`/`output` for the database at `path`: the
/// `jed pages` command (spec/pageservice/README.md §5.4). `read_only` makes the connection
/// read-only whatever the client asks. Returns when the connection ends.
///
/// The first frame must be `hello`. When the database has a live serving owner the stream is relayed
/// to its endpoint unchanged (with the read-only bit forced). Otherwise this process opens the
/// database as a `yield` owner, read-only when `read_only`, and serves the stream on it; a preferred
/// opener that later asks for ownership gets it, which ends this connection, and the client's
/// re-election reaches the new owner through a fresh `jed pages`. A failure to open is answered with
/// a fatal error frame.
pub(crate) fn serve_pages(
    path: &Path,
    opts: OpenOptions,
    read_only: bool,
    mut input: Box<dyn Read + Send>,
    mut output: Box<dyn Write + Send>,
) -> Result<()> {
    let (request_id, message) = match read_frame(&mut input, crate::DEFAULT_PAGE_SIZE) {
        Ok(Some(frame)) => frame,
        Ok(None) => return Ok(()),
        Err(e) => return Err(e),
    };
    let Message::Hello(mut hello) = message else {
        let e = EngineError::new(SqlState::ProtocolViolation, "the first frame must be hello");
        send_fatal(&mut *output, request_id, &e);
        return Err(e);
    };
    if read_only {
        hello.flags |= HELLO_FLAG_READ_ONLY;
    }
    let prefix = encode_frame(request_id, &Message::Hello(hello))?;
    let fail = |output: &mut dyn Write, e: EngineError| {
        send_fatal(output, request_id, &e);
        Err(e)
    };
    let real = match crate::coordinator::resolve_open_path(path) {
        Ok(real) => real,
        Err(e) => return fail(&mut *output, e),
    };
    let opts = OpenOptions {
        locking: Locking::Auto,
        ownership: Ownership::Yield,
        read_only,
        ..opts
    };
    let deadline = Instant::now() + Duration::from_millis(opts.file_lock_timeout_ms);
    loop {
        if let Some(socket) = live_owner(&real) {
            relay(&prefix, input, output, socket);
            return Ok(());
        }
        let db = match crate::shared::Database::open_with_options(&real, opts.clone()) {
            Ok(db) => db,
            Err(e) => return fail(&mut *output, e),
        };
        if let Some(owner) = db.page_service_owner() {
            let stream = PipeStream::new(prefix, input, output, None);
            let waiter = stream.clone_pipe();
            owner.add_connection(Box::new(stream));
            waiter.wait_closed();
            return db.close();
        }
        // Another process won ownership between the record check and the open: relay to it.
        drop(db);
        if Instant::now() >= deadline {
            return fail(
                &mut *output,
                EngineError::new(
                    SqlState::ObjectInUse,
                    format!(
                        "database file is in use: {} (no serving owner could be reached)",
                        real.display()
                    ),
                ),
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A connection to the live serving owner of the database at `path`, when its owner record names
/// one this user can reach.
fn live_owner(path: &Path) -> Option<UnixStream> {
    let record = crate::pageservice::endpoint::read_owner_record(path)?;
    if !record.serving || record.endpoint.is_empty() {
        return None;
    }
    let uid =
        crate::pageservice::endpoint::current_uid(&crate::pageservice::endpoint::bundle_dir(path))
            .ok()?;
    if record.uid != uid {
        return None;
    }
    UnixStream::connect(&record.endpoint).ok()
}

/// Relay the stream to the owner's socket in both directions, starting with the (rewritten) hello,
/// until either side ends.
fn relay(
    prefix: &[u8],
    mut input: Box<dyn Read + Send>,
    mut output: Box<dyn Write + Send>,
    socket: UnixStream,
) {
    let Ok(mut to_owner) = socket.try_clone() else {
        return;
    };
    let Ok(control) = socket.try_clone() else {
        return;
    };
    if to_owner.write_all(prefix).is_err() {
        return;
    }
    // Client → owner on its own thread; it may stay blocked on the input pipe after the owner side
    // ends, which a `jed pages` process leaves behind as it exits.
    std::thread::spawn(move || {
        let mut chunk = vec![0u8; 64 << 10];
        loop {
            match input.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to_owner.write_all(&chunk[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = to_owner.shutdown(std::net::Shutdown::Both);
    });
    let mut from_owner = socket;
    let mut chunk = vec![0u8; 64 << 10];
    loop {
        match from_owner.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if output.write_all(&chunk[..n]).is_err() || output.flush().is_err() {
                    break;
                }
            }
        }
    }
    let _ = control.shutdown(std::net::Shutdown::Both);
}

/// An in-process transport for tests: each dial starts [`serve_pages`] for `path` on a thread, over
/// a pair of OS pipes.
#[cfg(test)]
pub(crate) fn in_process_dialer(path: PathBuf, read_only: bool) -> Dialer {
    Arc::new(move || {
        let (from_client, to_server) = std::io::pipe()?;
        let (from_server, to_client) = std::io::pipe()?;
        let path = path.clone();
        std::thread::spawn(move || {
            let _ = serve_pages(
                &path,
                OpenOptions::default(),
                read_only,
                Box::new(from_client),
                Box::new(to_client),
            );
        });
        Ok(Box::new(PipeStream::new(
            Vec::new(),
            Box::new(from_server),
            Box::new(to_server),
            None,
        )) as Box<dyn PsStream>)
    })
}
