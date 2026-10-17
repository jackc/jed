//! The page client (spec/design/page-service.md §4.2, §5.3–§5.4; spec/pageservice/README.md §6.3):
//! a process that is not the owner runs its own engine over a pager whose byte store reads pages from
//! the owner's page service instead of the file. Phase 2: reads only — a client handle is read-only.
//!
//! Pins. The client holds exactly one owner pin on each version a local reader pins, and no other
//! ([`ClientPins`], kept in the shared core's reader registry). A read begin's pin is kept for the
//! reader that follows it; when a version's last local reader ends the client sends `end_read`. Page
//! reads name the newest held version, chosen under the connection lock, so an `end_read` for it can
//! never overtake the `read_pages` that names it.

use std::collections::BTreeSet;
use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::blockstore::BlockStore;
use crate::coordinator::LockHost;
use crate::error::{EngineError, Result, SqlState};
use crate::pageservice::codec::{
    BeginRead, ERROR_FLAG_FATAL, EndRead, HELLO_FLAG_READ_ONLY, Hello, Id16, Message,
    PROTOCOL_VERSION, ReadBegun, ReadPages, encode_frame, read_frame,
};

/// The process-local open-path claim a client keeps while connected (the duplicate-open guard,
/// locking.md §3.1); released when the client closes.
pub(crate) struct PathClaim {
    pub(crate) host: Arc<dyn LockHost>,
    pub(crate) path: PathBuf,
}

impl Drop for PathClaim {
    fn drop(&mut self) {
        self.host.release_path(&self.path);
    }
}

/// The request/reply half of a connection, behind [`PsConnection::io`].
struct ConnIo {
    stream: UnixStream,
    next_id: u32,
    /// The sticky failure after the connection is lost (`08006`) or poisoned by a fatal error.
    broken: Option<EngineError>,
}

/// The cache this process holds (README.md §3.2): the owner incarnation it came from and the
/// newest version it adopted. Its mutex serializes read begins and adoptions.
pub(crate) struct CacheId {
    pub(crate) incarnation: Id16,
    pub(crate) adopted: u64,
}

/// One page-service connection. `io` serializes request/reply pairs (phase 2 sends one request at a
/// time); it is the innermost lock — callers may hold the reader registry or the pager lock when they
/// take it, and nothing is taken while it is held.
pub(crate) struct PsConnection {
    db_path: PathBuf,
    io: Mutex<ConnIo>,
    /// A second handle on the socket, so [`close`](Self::close) can shut it down without waiting
    /// for a request in flight.
    control: UnixStream,
    page_size: u32,
    /// The newest version this connection pins at the owner (0 when none): what `read_pages` names.
    /// Updated under the reader-registry lock before any `end_read` is sent.
    read_txid: AtomicU64,
    /// The adopted version's meta page, served for page indices 0 and 1.
    meta: Mutex<Vec<u8>>,
    pub(crate) cache: Mutex<CacheId>,
    claim: Mutex<Option<PathClaim>>,
}

fn connection_lost(detail: &str) -> EngineError {
    EngineError::new(
        SqlState::ConnectionFailure,
        format!("lost the connection to the database owner: {detail}"),
    )
}

/// The engine error an `error` frame names. An unknown code (a newer owner) is a protocol
/// violation naming it.
fn error_from_frame(sqlstate: &str, message: &str) -> EngineError {
    match SqlState::from_code(sqlstate) {
        Some(state) => EngineError::new(state, message),
        None => EngineError::new(
            SqlState::ProtocolViolation,
            format!("the owner sent unknown SQLSTATE {sqlstate}: {message}"),
        ),
    }
}

/// The outcome of one connection attempt during election (README.md §6.1).
pub(crate) enum Connect {
    Connected(Arc<PsConnection>),
    /// A failed connect or a dropped connection: the opener retries.
    Retry,
    /// The owner refused the client (`0A000`, protocol or format mismatch): the open fails.
    Refused(EngineError),
}

/// Dial the owner's endpoint and run `hello`.
pub(crate) fn connect(db_path: &Path, endpoint: &str) -> Connect {
    let Ok(stream) = UnixStream::connect(endpoint) else {
        return Connect::Retry;
    };
    let Ok(control) = stream.try_clone() else {
        return Connect::Retry;
    };
    let mut conn = PsConnection {
        db_path: db_path.to_path_buf(),
        io: Mutex::new(ConnIo {
            stream,
            next_id: 0,
            broken: None,
        }),
        control,
        page_size: 0,
        read_txid: AtomicU64::new(0),
        meta: Mutex::new(Vec::new()),
        cache: Mutex::new(CacheId {
            incarnation: [0; 16],
            adopted: 0,
        }),
        claim: Mutex::new(None),
    };
    let hello = Message::Hello(Hello {
        protocol_min: PROTOCOL_VERSION,
        protocol_max: PROTOCOL_VERSION,
        format_version: crate::format::FORMAT_VERSION,
        flags: HELLO_FLAG_READ_ONLY,
        client_name: "jed-rust".into(),
    });
    match conn.round_trip(|| Ok(hello)) {
        Ok(Message::HelloOk(ok)) if crate::format::page_size_valid(ok.page_size as usize) => {
            conn.page_size = ok.page_size;
            Connect::Connected(Arc::new(conn))
        }
        Ok(_) => Connect::Refused(EngineError::new(
            SqlState::ProtocolViolation,
            "unexpected reply to hello",
        )),
        Err(e) if e.state == SqlState::ConnectionFailure => Connect::Retry,
        Err(e) => Connect::Refused(e),
    }
}

impl PsConnection {
    pub(crate) fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Hold `claim` until the client closes.
    pub(crate) fn keep_claim(&self, claim: PathClaim) {
        *self.claim.lock().expect("claim lock poisoned") = Some(claim);
    }

    /// Send the request `build` returns (built under the connection lock) and read its reply. An
    /// error reply comes back as the engine error it names; a lost connection is `08006` and breaks
    /// the connection for good.
    fn round_trip(&self, build: impl FnOnce() -> Result<Message>) -> Result<Message> {
        let mut io = self
            .io
            .lock()
            .expect("page-service connection lock poisoned");
        if let Some(broken) = &io.broken {
            return Err(broken.clone());
        }
        let request = build()?;
        let id = Self::send_locked(&mut io, &request)?;
        let reply = match read_frame(&mut io.stream, self.page_size) {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                let lost = connection_lost("the owner closed the connection");
                io.broken = Some(lost.clone());
                return Err(lost);
            }
            Err(e) => {
                let _ = io.stream.shutdown(Shutdown::Both);
                io.broken = Some(e.clone());
                return Err(e);
            }
        };
        match reply {
            (_, Message::Error(e)) => {
                let failure = error_from_frame(&e.sqlstate, &e.message);
                if e.flags & ERROR_FLAG_FATAL != 0 {
                    let _ = io.stream.shutdown(Shutdown::Both);
                    io.broken = Some(failure.clone());
                }
                Err(failure)
            }
            (got, message) if got == id => Ok(message),
            _ => {
                let failure =
                    EngineError::new(SqlState::ProtocolViolation, "reply out of request order");
                let _ = io.stream.shutdown(Shutdown::Both);
                io.broken = Some(failure.clone());
                Err(failure)
            }
        }
    }

    fn send_locked(io: &mut ConnIo, message: &Message) -> Result<u32> {
        io.next_id = io.next_id.wrapping_add(1);
        if io.next_id == 0 {
            io.next_id = 1;
        }
        let frame = encode_frame(io.next_id, message)?;
        if let Err(e) = io.stream.write_all(&frame) {
            let lost = connection_lost(&e.to_string());
            io.broken = Some(lost.clone());
            return Err(lost);
        }
        Ok(io.next_id)
    }

    /// `begin_read` for the cache `(incarnation, last_txid)`. The reply carries one new owner pin
    /// on its `txid`.
    pub(crate) fn begin_read(&self, incarnation: Id16, last_txid: u64) -> Result<ReadBegun> {
        let reply = self.round_trip(|| {
            Ok(Message::BeginRead(BeginRead {
                incarnation,
                last_txid,
            }))
        })?;
        let Message::ReadBegun(begun) = reply else {
            return Err(EngineError::new(
                SqlState::ProtocolViolation,
                "unexpected reply to begin_read",
            ));
        };
        if crate::format::meta_txid_and_page_count(&begun.meta).is_none() {
            return Err(EngineError::new(
                SqlState::DataCorrupted,
                "the owner sent an invalid meta page",
            ));
        }
        Ok(begun)
    }

    /// Send `end_read` for `txid` (no reply). A broken connection sends nothing: the owner already
    /// released every pin at EOF.
    pub(crate) fn end_read(&self, txid: u64) {
        let mut io = self
            .io
            .lock()
            .expect("page-service connection lock poisoned");
        if io.broken.is_none() {
            let _ = Self::send_locked(&mut io, &Message::EndRead(EndRead { txid }));
        }
    }

    /// Record the newest held version (the caller holds the reader-registry lock).
    pub(crate) fn set_read_txid(&self, txid: u64) {
        self.read_txid.store(txid, Ordering::Release);
    }

    /// Fetch one page under the newest pinned version.
    fn read_page(&self, index: u32) -> Result<Vec<u8>> {
        let reply = self.round_trip(|| {
            let txid = self.read_txid.load(Ordering::Acquire);
            if txid == 0 {
                return Err(EngineError::new(
                    SqlState::ObjectNotInPrerequisiteState,
                    "a page client read a page outside any read transaction",
                ));
            }
            Ok(Message::ReadPages(ReadPages {
                txid,
                indices: vec![index],
            }))
        })?;
        match reply {
            Message::Pages(mut pages) if pages.pages.len() == 1 => Ok(pages.pages.remove(0)),
            _ => Err(EngineError::new(
                SqlState::ProtocolViolation,
                "unexpected reply to read_pages",
            )),
        }
    }

    pub(crate) fn set_meta(&self, meta: Vec<u8>) {
        *self.meta.lock().expect("meta lock poisoned") = meta;
    }

    fn meta(&self) -> Vec<u8> {
        self.meta.lock().expect("meta lock poisoned").clone()
    }

    /// Close the connection (the owner releases every pin at EOF) and release the open-path claim.
    /// Idempotent.
    pub(crate) fn close(&self) {
        let _ = self.control.shutdown(Shutdown::Both);
        {
            let mut io = self
                .io
                .lock()
                .expect("page-service connection lock poisoned");
            if io.broken.is_none() {
                io.broken = Some(EngineError::new(
                    SqlState::ConnectionFailure,
                    "the page-service connection is closed",
                ));
            }
        }
        self.claim.lock().expect("claim lock poisoned").take();
    }
}

/// The versions a client pins at its owner (README.md §6.3): exactly the versions a local reader
/// pins, plus a read begin's pin until its reader registers. Kept in the reader registry, under its
/// lock; the methods say what to send, the caller sends it.
#[derive(Default)]
pub(crate) struct ClientPins {
    held: BTreeSet<u64>,
}

impl ClientPins {
    /// Record the pin a `read_begun` carries. `true` when it duplicates one already held: the caller
    /// sends `end_read` for the duplicate at once.
    pub(crate) fn take(&mut self, txid: u64) -> bool {
        !self.held.insert(txid)
    }

    /// Drop the pin on `txid`. `true` when it was held: the caller sends `end_read`.
    pub(crate) fn release(&mut self, txid: u64) -> bool {
        self.held.remove(&txid)
    }

    /// The newest held version (0 when none) — what `read_pages` names.
    pub(crate) fn newest(&self) -> u64 {
        self.held.last().copied().unwrap_or(0)
    }
}

/// A client's [`ClientPins`] with the connection they are held on.
pub(crate) struct ClientLink {
    pub(crate) conn: Arc<PsConnection>,
    pub(crate) pins: ClientPins,
}

impl ClientLink {
    /// Keep a `read_begun` pin, releasing it at once when it duplicates a held one.
    pub(crate) fn take(&mut self, txid: u64) {
        if self.pins.take(txid) {
            self.conn.end_read(txid);
        } else {
            self.conn.set_read_txid(self.pins.newest());
        }
    }

    /// Release the pin on `txid` if held: `read_pages` moves off it before `end_read` is sent.
    pub(crate) fn release(&mut self, txid: u64) {
        if self.pins.release(txid) {
            self.conn.set_read_txid(self.pins.newest());
            self.conn.end_read(txid);
        }
    }
}

/// The client pager's byte store: page indices 0 and 1 are the adopted meta page, every other page
/// comes from the owner. Read-only.
pub(crate) struct ServiceBlockStore {
    pub(crate) conn: Arc<PsConnection>,
}

fn client_read_only() -> EngineError {
    EngineError::new(
        SqlState::ReadOnlySqlTransaction,
        "a page client cannot write the database",
    )
}

impl BlockStore for ServiceBlockStore {
    fn read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let page_size = u64::from(self.conn.page_size);
        let index = offset / page_size;
        let within = (offset - index * page_size) as usize;
        if within + len > page_size as usize {
            return Err(EngineError::new(
                SqlState::IoError,
                "page-service read must stay within one page",
            ));
        }
        let page = if index < 2 {
            self.conn.meta()
        } else {
            self.conn.read_page(index as u32)?
        };
        if page.len() != page_size as usize {
            return Err(EngineError::new(
                SqlState::ProtocolViolation,
                "the owner sent a page of the wrong size",
            ));
        }
        Ok(page[within..within + len].to_vec())
    }

    fn write_at(&mut self, _offset: u64, _bytes: &[u8]) -> Result<()> {
        Err(client_read_only())
    }

    fn sync(&mut self) -> Result<()> {
        Err(client_read_only())
    }

    /// The adopted snapshot's page count: every page a reader of it may name lies below it.
    fn size(&mut self) -> Result<u64> {
        let (_, page_count) = crate::format::meta_txid_and_page_count(&self.conn.meta())
            .ok_or_else(|| EngineError::new(SqlState::DataCorrupted, "invalid meta page"))?;
        Ok(u64::from(page_count) * u64::from(self.conn.page_size))
    }

    fn set_size(&mut self, _bytes: u64) -> Result<()> {
        Err(client_read_only())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_track_local_readers_and_duplicates() {
        let mut pins = ClientPins::default();
        assert_eq!(pins.newest(), 0);
        assert!(!pins.take(5)); // a fresh pin is kept
        assert!(pins.take(5)); // a second read_begun on 5 duplicates it
        assert!(!pins.take(7));
        assert_eq!(pins.newest(), 7);
        assert!(pins.release(7));
        assert_eq!(pins.newest(), 5); // read_pages moves to the newest version still held
        assert!(!pins.release(7)); // releasing twice sends nothing
        assert!(pins.release(5));
        assert_eq!(pins.newest(), 0);
    }
}
