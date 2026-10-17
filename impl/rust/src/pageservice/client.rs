//! The page client (spec/design/page-service.md §4.2, §5.3–§5.6; spec/pageservice/README.md §3, §6):
//! a process that is not the owner runs its own engine over a pager whose byte store reads pages from,
//! and commits whole CommitDeltas through, the owner's page service instead of the file.
//!
//! Pins. The client holds exactly one owner pin on each version a local reader pins, and no other
//! ([`ClientPins`], kept in the shared core's reader registry). A read begin's pin is kept for the
//! reader that follows it; when a version's last local reader ends the client sends `end_read`. A
//! write transaction holds the owner's write pin on its base instead. Page reads name the newest
//! pinned version, chosen under the send lock, so an `end_read`, `abort`, or `commit_end` releasing
//! it can never overtake the `read_pages` that names it.
//!
//! The connection is multiplexed (README.md §2): one reader thread reads every reply and hands it to
//! the request it answers, so a `begin_write` waiting for the owner's gate never stalls this
//! process's page reads.
//!
//! A [`PsConnection`] is one connection. When it ends, the handle re-elects (`shared/handoff.rs`,
//! README.md §6.5) and installs a new role with a new connection; this one stays broken (`08006`)
//! for the transactions begun on it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::blockstore::BlockStore;
use crate::coordinator::LockHost;
use crate::error::{EngineError, Result, SqlState};
use crate::pageservice::codec::{
    BeginRead, BeginWrite, CommitBegin, CommitPages, CommitStatus, ERROR_FLAG_FATAL, EndRead,
    HELLO_FLAG_READ_ONLY, HELLO_OK_FLAG_CONNECTION_READ_ONLY, HELLO_OK_FLAG_OWNER_READ_ONLY,
    HELLO_OK_FLAG_OWNER_YIELDS, Hello, Id16, IndexedPage, MAX_FRAME_LENGTH, Message,
    NOTICE_YIELD_BEGUN, NOTICE_YIELD_CANCELLED, PROTOCOL_VERSION, ReadBegun, ReadPages, WriteBegun,
    encode_frame, read_frame,
};
use crate::pageservice::net::PsStream;

/// How long a client that saw another connection's yield notice skips its presence tries after
/// its connection ends, so the requester takes ownership (README.md §6.5).
pub(crate) const HANDOFF_PAUSE: Duration = Duration::from_millis(2000);

#[cfg(test)]
thread_local! {
    /// `read_pages` requests this thread sent (the read-ahead tests count round trips).
    pub(crate) static READ_PAGES_SENT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Turns read-ahead off for connections made afterwards (the benchmark's baseline lane,
/// `Database::set_page_client_read_ahead_for_testing`).
pub(crate) static READ_AHEAD_DISABLED: AtomicBool = AtomicBool::new(false);

/// The re-election trigger a handle installs on its connection ([`PsConnection::set_on_break`]).
pub(crate) type OnBreak = Arc<dyn Fn() + Send + Sync>;

/// The send half of a connection, behind [`PsConnection::outbox`]: the send lock.
struct Outbox {
    stream: Box<dyn PsStream>,
    next_id: u32,
}

/// A request's reply, or the connection's failure.
type Reply = Result<Message>;

/// The requests waiting for replies, behind [`PsConnection::waiters`].
struct Waiters {
    pending: HashMap<u32, mpsc::Sender<Reply>>,
    /// The sticky failure after the connection is lost (`08006`) or poisoned by a fatal error, and
    /// when it was set (the handoff pause counts from it).
    broken: Option<EngineError>,
    broken_at: Option<Instant>,
    /// Called once when the connection breaks, to start the handle's re-election.
    on_break: Option<OnBreak>,
}

/// The cache this process holds (README.md §3.2): the owner incarnation it came from and the
/// newest version it adopted. Its mutex serializes read begins and adoptions.
pub(crate) struct CacheId {
    pub(crate) incarnation: Id16,
    pub(crate) adopted: u64,
}

/// One page-service connection. The send lock (`outbox`) serializes frames and request numbering;
/// a request's contents may depend on state that changes only under it (the version `read_pages`
/// names). It is the innermost lock — callers may hold the reader registry, the cache, the storage,
/// or the pager lock when they take it.
pub(crate) struct PsConnection {
    db_path: PathBuf,
    outbox: Mutex<Outbox>,
    waiters: Mutex<Waiters>,
    /// Signalled (with `waiters`) when the connection breaks.
    broke: Condvar,
    /// A second handle on the socket, so [`close`](Self::close) can shut it down while the reader
    /// thread is blocked on it.
    control: Box<dyn PsStream>,
    /// The lock host, whose clock times the handoff pause (the simulator's in a simulation).
    host: Arc<dyn LockHost>,
    page_size: AtomicU32,
    /// The most pages one `read_pages` may name (`hello_ok`).
    max_read_pages: AtomicU32,
    /// Scans over this connection read ahead (the stream transport, [`LockHost::reads_ahead`]).
    read_ahead: bool,
    /// `hello_ok` said the connection is read-only.
    read_only: AtomicBool,
    /// The owner's `hello_ok` flags (README.md §3.1, §6.5): its own handle is read-only; its
    /// ownership preference is `yield`.
    owner_read_only: AtomicBool,
    owner_yields: AtomicBool,
    /// This connection's id (`hello_ok`), which a yield notice names.
    connection_id: AtomicU64,
    /// Set by a yield-begun notice naming another connection and cleared by a yield-cancelled one
    /// (README.md §6.5).
    handoff_paused: AtomicBool,
    /// A commit whose connection was lost after `commit_end` was sent: `(txid, digest)`, for the
    /// handle's re-election to resolve (README.md §6.4–§6.5).
    lost_commit: Mutex<Option<(u64, u64)>>,
    /// The newest version a local reader pins (0 when none). With `write_txid`, what `read_pages`
    /// names; both change under the send lock before any message releasing a version is sent.
    held_max: AtomicU64,
    /// The write transaction's base while this process holds the owner's gate (0 otherwise).
    write_txid: AtomicU64,
    /// The write transaction holds the owner's gate, and whether it may reuse free pages. Touched
    /// only by the local writer, which holds the in-process writer gate.
    gate_held: AtomicBool,
    reuse_grant: AtomicBool,
    /// The last commit failed and the owner certainly did not commit it (the pager may continue).
    uncommitted: AtomicBool,
    /// The adopted version's meta page, served for page indices 0 and 1.
    meta: Mutex<Vec<u8>>,
    pub(crate) cache: Mutex<CacheId>,
}

fn connection_lost(detail: &str) -> EngineError {
    EngineError::new(
        SqlState::ConnectionFailure,
        format!("lost the connection to the database owner: {detail}"),
    )
}

fn violation(message: &str) -> EngineError {
    EngineError::new(SqlState::ProtocolViolation, message)
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
    /// The owner refused the client (`0A000`, protocol or format mismatch; `53300`, its connection
    /// limit): the election decides.
    Refused(EngineError),
}

/// Dial the owner's endpoint through `host`, start the reader thread, and run `hello`, asking for a
/// read-only connection when `read_only`.
pub(crate) fn connect(
    host: &Arc<dyn LockHost>,
    db_path: &Path,
    endpoint: &str,
    read_only: bool,
) -> Connect {
    let Ok(stream) = host.dial(endpoint) else {
        return Connect::Retry;
    };
    let (Ok(control), Ok(reader)) = (stream.clone_stream(), stream.clone_stream()) else {
        stream.close_both();
        return Connect::Retry;
    };
    let conn = Arc::new(PsConnection {
        db_path: db_path.to_path_buf(),
        outbox: Mutex::new(Outbox { stream, next_id: 0 }),
        waiters: Mutex::new(Waiters {
            pending: HashMap::new(),
            broken: None,
            broken_at: None,
            on_break: None,
        }),
        broke: Condvar::new(),
        control,
        host: Arc::clone(host),
        page_size: AtomicU32::new(0),
        max_read_pages: AtomicU32::new(1),
        read_ahead: host.reads_ahead() && !READ_AHEAD_DISABLED.load(Ordering::Relaxed),
        read_only: AtomicBool::new(false),
        owner_read_only: AtomicBool::new(false),
        owner_yields: AtomicBool::new(false),
        connection_id: AtomicU64::new(0),
        handoff_paused: AtomicBool::new(false),
        lost_commit: Mutex::new(None),
        held_max: AtomicU64::new(0),
        write_txid: AtomicU64::new(0),
        gate_held: AtomicBool::new(false),
        reuse_grant: AtomicBool::new(false),
        uncommitted: AtomicBool::new(false),
        meta: Mutex::new(Vec::new()),
        cache: Mutex::new(CacheId {
            incarnation: [0; 16],
            adopted: 0,
        }),
    });
    let reading = Arc::clone(&conn);
    std::thread::spawn(move || reading.read_loop(reader));
    let hello = Message::Hello(Hello {
        protocol_min: PROTOCOL_VERSION,
        protocol_max: PROTOCOL_VERSION,
        format_version: crate::format::FORMAT_VERSION,
        flags: if read_only { HELLO_FLAG_READ_ONLY } else { 0 },
        client_name: "jed-rust".into(),
    });
    match conn.round_trip(|| Ok(hello)) {
        Ok(Message::HelloOk(ok)) if crate::format::page_size_valid(ok.page_size as usize) => {
            conn.page_size.store(ok.page_size, Ordering::Release);
            conn.max_read_pages
                .store(ok.max_read_pages.max(1), Ordering::Release);
            conn.read_only.store(
                ok.flags & HELLO_OK_FLAG_CONNECTION_READ_ONLY != 0,
                Ordering::Release,
            );
            conn.owner_read_only.store(
                ok.flags & HELLO_OK_FLAG_OWNER_READ_ONLY != 0,
                Ordering::Release,
            );
            conn.owner_yields.store(
                ok.flags & HELLO_OK_FLAG_OWNER_YIELDS != 0,
                Ordering::Release,
            );
            conn.connection_id
                .store(ok.connection_id, Ordering::Release);
            Connect::Connected(conn)
        }
        Ok(_) => {
            conn.close();
            Connect::Refused(violation("unexpected reply to hello"))
        }
        Err(e) if e.state == SqlState::ConnectionFailure => {
            conn.close();
            Connect::Retry
        }
        Err(e) => {
            conn.close();
            Connect::Refused(e)
        }
    }
}

impl PsConnection {
    pub(crate) fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Whether the connection (and so the client's handle) is read-only.
    pub(crate) fn read_only(&self) -> bool {
        self.read_only.load(Ordering::Acquire)
    }

    /// Whether the owner's own handle is read-only (`hello_ok` bit 1).
    pub(crate) fn owner_read_only(&self) -> bool {
        self.owner_read_only.load(Ordering::Acquire)
    }

    /// Whether the owner's ownership preference is `yield` (`hello_ok` bit 2).
    pub(crate) fn owner_yields(&self) -> bool {
        self.owner_yields.load(Ordering::Acquire)
    }

    /// Whether the connection ended (or was closed).
    pub(crate) fn is_broken(&self) -> bool {
        self.waiters
            .lock()
            .expect("page-service waiters lock poisoned")
            .broken
            .is_some()
    }

    /// Install the re-election trigger, called once when the connection breaks. `false` when it
    /// is broken already: the caller starts the re-election itself.
    pub(crate) fn set_on_break(&self, on_break: OnBreak) -> bool {
        let mut waiters = self
            .waiters
            .lock()
            .expect("page-service waiters lock poisoned");
        if waiters.broken.is_some() {
            return false;
        }
        waiters.on_break = Some(on_break);
        true
    }

    /// Wait until the connection breaks, at most until `deadline` (`None`: no deadline). `true`
    /// when it broke.
    pub(crate) fn wait_broken(&self, deadline: Option<Instant>) -> bool {
        let mut waiters = self
            .waiters
            .lock()
            .expect("page-service waiters lock poisoned");
        loop {
            if waiters.broken.is_some() {
                return true;
            }
            match deadline {
                None => {
                    waiters = self
                        .broke
                        .wait(waiters)
                        .expect("page-service waiters lock poisoned");
                }
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return false;
                    }
                    waiters = self
                        .broke
                        .wait_timeout(waiters, deadline - now)
                        .expect("page-service waiters lock poisoned")
                        .0;
                }
            }
        }
    }

    /// When this client's handoff pause ends: `HANDOFF_PAUSE` after the connection ended, when a
    /// yield notice named another connection (README.md §6.5); `None` when not paused.
    pub(crate) fn handoff_pause_until(&self) -> Option<Instant> {
        if !self.handoff_paused.load(Ordering::Acquire) {
            return None;
        }
        let waiters = self
            .waiters
            .lock()
            .expect("page-service waiters lock poisoned");
        Some(waiters.broken_at.unwrap_or_else(|| self.host.now()) + HANDOFF_PAUSE)
    }

    /// Act on an owner notice (README.md §3.7, §6.5): another connection's yield pauses this
    /// client's presence tries after the connection ends; a cancelled yield lifts the pause. Every
    /// other notice is ignored.
    fn notice(&self, kind: u8, connection_id: u64) {
        match kind {
            NOTICE_YIELD_BEGUN => self.handoff_paused.store(
                connection_id != self.connection_id.load(Ordering::Acquire),
                Ordering::Release,
            ),
            NOTICE_YIELD_CANCELLED => self.handoff_paused.store(false, Ordering::Release),
            _ => {}
        }
    }

    /// Read every reply frame and hand it to the request it answers. On a lost connection or a
    /// frame no request is waiting for, break the connection and fail every waiting request.
    fn read_loop(self: Arc<Self>, mut stream: Box<dyn PsStream>) {
        loop {
            let (id, message) =
                match read_frame(&mut stream, self.page_size.load(Ordering::Acquire)) {
                    Ok(Some(frame)) => frame,
                    Ok(None) => {
                        self.break_with(connection_lost("the owner closed the connection"));
                        return;
                    }
                    Err(e) => {
                        self.break_with(e);
                        return;
                    }
                };
            if let Message::Notice(notice) = &message
                && id == 0
            {
                self.notice(notice.kind, notice.connection_id);
                continue;
            }
            if let Message::HelloOk(ok) = &message
                && crate::format::page_size_valid(ok.page_size as usize)
            {
                // Later frames carry pages of this size: set it before reading the next one.
                self.page_size.store(ok.page_size, Ordering::Release);
            }
            let waiter = self
                .waiters
                .lock()
                .expect("page-service waiters lock poisoned")
                .pending
                .remove(&id);
            match waiter {
                Some(waiter) => {
                    let _ = waiter.send(Ok(message));
                }
                None => {
                    self.break_with(violation(
                        "the owner sent a reply no request is waiting for",
                    ));
                    return;
                }
            }
        }
    }

    /// Make `error` the connection's sticky failure, fail every waiting request with it, (the first
    /// time) start the handle's re-election, and shut the socket down — last: the simulator treats a
    /// closed end as settled.
    fn break_with(&self, error: EngineError) {
        let (pending, broken, on_break) = {
            let mut waiters = self
                .waiters
                .lock()
                .expect("page-service waiters lock poisoned");
            let mut on_break = None;
            if waiters.broken.is_none() {
                waiters.broken = Some(error);
                waiters.broken_at = Some(self.host.now());
                on_break = waiters.on_break.take();
                self.broke.notify_all();
            }
            (
                std::mem::take(&mut waiters.pending),
                waiters.broken.clone().expect("set above"),
                on_break,
            )
        };
        for waiter in pending.into_values() {
            let _ = waiter.send(Err(broken.clone()));
        }
        if let Some(on_break) = on_break {
            on_break();
        }
        self.control.close_both();
    }

    fn broken(&self) -> EngineError {
        self.waiters
            .lock()
            .expect("page-service waiters lock poisoned")
            .broken
            .clone()
            .unwrap_or_else(|| connection_lost("the reply was lost"))
    }

    /// Number and write one frame; with `reply` set, register the request's reply channel first.
    /// The caller holds the send lock.
    fn send_locked(
        &self,
        outbox: &mut Outbox,
        message: &Message,
        reply: bool,
    ) -> Result<Option<mpsc::Receiver<Reply>>> {
        outbox.next_id = outbox.next_id.wrapping_add(1);
        if outbox.next_id == 0 {
            outbox.next_id = 1;
        }
        let frame = encode_frame(outbox.next_id, message)?;
        let receiver = {
            let mut waiters = self
                .waiters
                .lock()
                .expect("page-service waiters lock poisoned");
            if let Some(broken) = &waiters.broken {
                return Err(broken.clone());
            }
            if reply {
                let (sender, receiver) = mpsc::channel();
                waiters.pending.insert(outbox.next_id, sender);
                Some(receiver)
            } else {
                None
            }
        };
        if let Err(e) = outbox.stream.write_all(&frame) {
            let lost = connection_lost(&e.to_string());
            self.break_with(lost.clone());
            return Err(lost);
        }
        Ok(receiver)
    }

    /// Wait for a request's reply. An error reply comes back as the engine error it names; a fatal
    /// one breaks the connection.
    fn await_reply(&self, receiver: mpsc::Receiver<Reply>) -> Result<Message> {
        let reply = receiver.recv().unwrap_or_else(|_| Err(self.broken()))?;
        match reply {
            Message::Error(e) => {
                let failure = error_from_frame(&e.sqlstate, &e.message);
                if e.flags & ERROR_FLAG_FATAL != 0 {
                    self.break_with(failure.clone());
                }
                Err(failure)
            }
            message => Ok(message),
        }
    }

    /// Send the request `build` returns (built under the send lock) and wait for its reply. A lost
    /// connection is `08006` and breaks the connection for good.
    fn round_trip(&self, build: impl FnOnce() -> Result<Message>) -> Result<Message> {
        let receiver = {
            let mut outbox = self.outbox.lock().expect("page-service send lock poisoned");
            let request = build()?;
            self.send_locked(&mut outbox, &request, true)?
                .expect("a request with a reply registers a channel")
        };
        self.await_reply(receiver)
    }

    /// Send a message that has no reply. A broken connection sends nothing: the owner already
    /// released every pin and the gate at EOF.
    fn send(&self, message: &Message) {
        let mut outbox = self.outbox.lock().expect("page-service send lock poisoned");
        let _ = self.send_locked(&mut outbox, message, false);
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
            return Err(violation("unexpected reply to begin_read"));
        };
        if crate::format::meta_txid_and_page_count(&begun.meta).is_none() {
            return Err(EngineError::new(
                SqlState::DataCorrupted,
                "the owner sent an invalid meta page",
            ));
        }
        Ok(begun)
    }

    /// `begin_write` with the session's lock timeout (README.md §3.5). The wait for the owner's
    /// gate does not hold the send lock, so this process's readers keep reading.
    pub(crate) fn begin_write(
        &self,
        incarnation: Id16,
        last_txid: u64,
        lock_timeout_ms: u64,
    ) -> Result<WriteBegun> {
        let reply = self.round_trip(|| {
            Ok(Message::BeginWrite(BeginWrite {
                incarnation,
                last_txid,
                lock_timeout_ms,
            }))
        })?;
        let Message::WriteBegun(begun) = reply else {
            return Err(violation("unexpected reply to begin_write"));
        };
        if crate::format::meta_txid_and_page_count(&begun.meta).is_none() {
            self.abort_write();
            return Err(EngineError::new(
                SqlState::DataCorrupted,
                "the owner sent an invalid meta page",
            ));
        }
        Ok(begun)
    }

    /// Record a granted write transaction on `txid`: it holds the gate, and page reads may name its
    /// write pin.
    pub(crate) fn start_write(&self, txid: u64, reuse_grant: bool) {
        self.gate_held.store(true, Ordering::Release);
        self.reuse_grant.store(reuse_grant, Ordering::Release);
        let _outbox = self.outbox.lock().expect("page-service send lock poisoned");
        self.write_txid.store(txid, Ordering::Release);
    }

    /// Whether the write transaction may reuse the committed free list (page-service.md §6).
    pub(crate) fn reuse_grant(&self) -> bool {
        self.reuse_grant.load(Ordering::Acquire)
    }

    /// End a write transaction that did not commit: send `abort` if it still holds the gate.
    pub(crate) fn end_write(&self) {
        if self.gate_held.swap(false, Ordering::AcqRel) {
            self.abort_write();
        }
    }

    /// Clear the write pin's part in `read_pages` and send `abort`, both under the send lock, so
    /// no later `read_pages` names the released version.
    fn abort_write(&self) {
        let mut outbox = self.outbox.lock().expect("page-service send lock poisoned");
        self.write_txid.store(0, Ordering::Release);
        let _ = self.send_locked(&mut outbox, &Message::Abort, false);
    }

    /// `request_ownership` (README.md §3.6): the owner's `ownership_reply` status.
    pub(crate) fn request_ownership(&self) -> Result<u8> {
        match self.round_trip(|| Ok(Message::RequestOwnership))? {
            Message::OwnershipReply(reply) => Ok(reply.status),
            _ => Err(violation("unexpected reply to request_ownership")),
        }
    }

    /// `commit_status` (README.md §3.5): the owner's answer for the commit `(txid, digest)`.
    pub(crate) fn commit_status(&self, txid: u64, digest: u64) -> Result<u8> {
        match self.round_trip(|| Ok(Message::CommitStatus(CommitStatus { txid, digest })))? {
            Message::CommitStatusReply(reply) => Ok(reply.status),
            _ => Err(violation("unexpected reply to commit_status")),
        }
    }

    /// Send `end_read` for a duplicate pin a `read_begun` carried.
    pub(crate) fn end_read(&self, txid: u64) {
        self.send(&Message::EndRead(EndRead { txid }));
    }

    /// Record that the newest version a local reader pins is now `newest`.
    pub(crate) fn set_held_max(&self, newest: u64) {
        let _outbox = self.outbox.lock().expect("page-service send lock poisoned");
        self.held_max.store(newest, Ordering::Release);
    }

    /// Release the pin on `txid`: `read_pages` moves to `newest` before `end_read` is sent.
    pub(crate) fn release_pin(&self, txid: u64, newest: u64) {
        let mut outbox = self.outbox.lock().expect("page-service send lock poisoned");
        self.held_max.store(newest, Ordering::Release);
        let _ = self.send_locked(&mut outbox, &Message::EndRead(EndRead { txid }), false);
    }

    /// Fetch pages under the newest pinned version, in as few `read_pages` requests as the owner's
    /// `max_read_pages` allows (read-ahead batches them, page-service.md §5.3; a backup's copy,
    /// README.md §6.6).
    pub(crate) fn read_pages(&self, indices: &[u32]) -> Result<Vec<Vec<u8>>> {
        let limit = self.max_read_pages.load(Ordering::Acquire).max(1) as usize;
        let mut pages = Vec::with_capacity(indices.len());
        for chunk in indices.chunks(limit) {
            #[cfg(test)]
            READ_PAGES_SENT.with(|sent| sent.set(sent.get() + 1));
            let reply = self.round_trip(|| {
                let txid = self
                    .held_max
                    .load(Ordering::Acquire)
                    .max(self.write_txid.load(Ordering::Acquire));
                if txid == 0 {
                    return Err(EngineError::new(
                        SqlState::ObjectNotInPrerequisiteState,
                        "a page client read a page outside any transaction",
                    ));
                }
                Ok(Message::ReadPages(ReadPages {
                    txid,
                    indices: chunk.to_vec(),
                }))
            })?;
            match reply {
                Message::Pages(reply) if reply.pages.len() == chunk.len() => {
                    pages.extend(reply.pages)
                }
                _ => return Err(violation("unexpected reply to read_pages")),
            }
        }
        Ok(pages)
    }

    /// The most indices one `read_pages` may name on this connection (README.md §3.1).
    pub(crate) fn max_read_pages(&self) -> usize {
        crate::pageservice::codec::max_read_pages(self.page_size.load(Ordering::Acquire)) as usize
    }

    pub(crate) fn set_meta(&self, meta: Vec<u8>) {
        *self.meta.lock().expect("meta lock poisoned") = meta;
    }

    fn meta(&self) -> Vec<u8> {
        self.meta.lock().expect("meta lock poisoned").clone()
    }

    /// The commit whose acknowledgement the last commit lost, `(txid, digest)`; clears it.
    pub(crate) fn take_lost_commit(&self) -> Option<(u64, u64)> {
        self.lost_commit
            .lock()
            .expect("lost-commit lock poisoned")
            .take()
    }

    /// Whether the last commit failed with the owner certainly not committing it; clears the mark.
    pub(crate) fn take_uncommitted(&self) -> bool {
        self.uncommitted.swap(false, Ordering::AcqRel)
    }

    /// Mark `error` as a commit failure after which the owner certainly did not commit.
    fn not_committed(&self, error: EngineError) -> EngineError {
        self.uncommitted.store(true, Ordering::Release);
        error
    }

    /// Send a CommitDelta (README.md §3.5): `commit_begin` with the meta and the manifest chain,
    /// the body pages in ascending index order in frame-sized chunks, then `commit_end`, and wait
    /// for the owner. A connection lost after `commit_end` was sent leaves the outcome unknown: the
    /// commit is recorded for [`take_lost_commit`](Self::take_lost_commit), and the handle's
    /// re-election resolves it.
    fn commit(&self, meta: Vec<u8>, mut pages: BTreeMap<u32, Vec<u8>>) -> Result<()> {
        self.uncommitted.store(false, Ordering::Release);
        let Some(m) = crate::format::meta_fields(&meta) else {
            return Err(self.not_committed(EngineError::new(
                SqlState::DataCorrupted,
                "a page-client commit built an invalid meta page",
            )));
        };
        let ps = self.page_size.load(Ordering::Acquire) as usize;
        let mut manifest = Vec::new();
        let mut id = m.manifest_head;
        for _ in 0..m.manifest_pages {
            let Some(page) = pages.remove(&id) else {
                return Err(self.not_committed(EngineError::new(
                    SqlState::DataCorrupted,
                    "a page-client commit lost a manifest page",
                )));
            };
            let next = crate::format::page_header(&page)
                .map_err(|e| self.not_committed(e))?
                .next_page;
            manifest.push(IndexedPage { index: id, page });
            id = next;
        }
        let max_frame = MAX_FRAME_LENGTH as usize;
        // A frame holds the length's 5 bytes of code and request id, the list count, and the
        // (index, page) items.
        let per_frame = (max_frame - 5 - 4) / (4 + ps);
        if 5 + 8 + ps + 4 + manifest.len() * (4 + ps) > max_frame {
            return Err(self.not_committed(EngineError::new(
                SqlState::ProgramLimitExceeded,
                "the transaction is too large to commit through the page service",
            )));
        }
        let digest = crate::format::commit_crc64(0, &meta);
        let body: Vec<IndexedPage> = pages
            .into_iter()
            .map(|(index, page)| IndexedPage { index, page })
            .collect();
        let receiver = {
            let mut outbox = self.outbox.lock().expect("page-service send lock poisoned");
            let begin = Message::CommitBegin(CommitBegin {
                txid: m.txid,
                meta,
                manifest_pages: manifest,
            });
            self.send_locked(&mut outbox, &begin, false)
                .map_err(|e| self.not_committed(e))?;
            let mut body = body.into_iter().peekable();
            while body.peek().is_some() {
                let chunk: Vec<IndexedPage> = body.by_ref().take(per_frame).collect();
                self.send_locked(
                    &mut outbox,
                    &Message::CommitPages(CommitPages { pages: chunk }),
                    false,
                )
                .map_err(|e| self.not_committed(e))?;
            }
            let receiver = self.send_locked(&mut outbox, &Message::CommitEnd, true);
            // The owner releases the write pin and the gate when it processes commit_end, whatever
            // its outcome, so no later request may name the write base.
            self.write_txid.store(0, Ordering::Release);
            self.gate_held.store(false, Ordering::Release);
            receiver
                .map_err(|e| self.not_committed(e))?
                .expect("a request with a reply registers a channel")
        };
        match self.await_reply(receiver) {
            Ok(Message::Committed(done)) if done.txid == m.txid => Ok(()),
            Ok(_) => Err(violation("unexpected reply to commit_end")),
            Err(e) if e.state == SqlState::ConnectionFailure => {
                *self.lost_commit.lock().expect("lost-commit lock poisoned") =
                    Some((m.txid, digest));
                Err(e)
            }
            Err(e) => Err(self.not_committed(e)),
        }
    }

    /// Close the connection (the owner releases every pin and the gate at EOF). Idempotent.
    pub(crate) fn close(&self) {
        self.break_with(EngineError::new(
            SqlState::ConnectionFailure,
            "the page-service connection is closed",
        ));
    }
}

/// The versions a client pins at its owner for local readers (README.md §6.3): exactly the
/// versions a local reader pins, plus a read begin's pin until its reader registers. Kept in the
/// reader registry, under its lock; the methods say what to send, the caller sends it.
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

    /// The newest held version (0 when none) — what `read_pages` names outside a write.
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
            self.conn.set_held_max(self.pins.newest());
        }
    }

    /// Release the pin on `txid` if held: `read_pages` moves off it before `end_read` is sent.
    pub(crate) fn release(&mut self, txid: u64) {
        if self.pins.release(txid) {
            self.conn.release_pin(txid, self.pins.newest());
        }
    }
}

/// The client pager's byte store: page indices 0 and 1 are the adopted meta page, every other page
/// comes from the owner. A commit's writes collect here and its sync sends them as one CommitDelta
/// (page-service.md §5.6).
pub(crate) struct ServiceBlockStore {
    conn: Arc<PsConnection>,
    /// The commit in progress: its body pages by index and its meta. The pager reads a pending page
    /// back from here (a commit re-reads what it wrote while planning).
    pending: BTreeMap<u32, Vec<u8>>,
    pending_meta: Option<Vec<u8>>,
    /// The size a commit in progress reserved.
    reserved: u64,
}

impl ServiceBlockStore {
    pub(crate) fn new(conn: Arc<PsConnection>) -> ServiceBlockStore {
        ServiceBlockStore {
            conn,
            pending: BTreeMap::new(),
            pending_meta: None,
            reserved: 0,
        }
    }

    fn page_size(&self) -> u64 {
        u64::from(self.conn.page_size.load(Ordering::Acquire))
    }
}

fn client_read_only() -> EngineError {
    EngineError::new(
        SqlState::ReadOnlySqlTransaction,
        "cannot write through a read-only page-service connection",
    )
}

impl BlockStore for ServiceBlockStore {
    fn read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let page_size = self.page_size();
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
        } else if let Some(page) = self.pending.get(&(index as u32)) {
            page.clone()
        } else {
            self.conn.read_pages(&[index as u32])?.remove(0)
        };
        if page.len() != page_size as usize {
            return Err(EngineError::new(
                SqlState::ProtocolViolation,
                "the owner sent a page of the wrong size",
            ));
        }
        Ok(page[within..within + len].to_vec())
    }

    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        if self.conn.read_only() {
            return Err(client_read_only());
        }
        let page_size = self.page_size();
        if !offset.is_multiple_of(page_size) || bytes.len() as u64 != page_size {
            return Err(EngineError::new(
                SqlState::IoError,
                "page-service write must be one whole page",
            ));
        }
        let index = (offset / page_size) as u32;
        if index < 2 {
            self.pending_meta = Some(bytes.to_vec());
        } else {
            self.pending.insert(index, bytes.to_vec());
        }
        Ok(())
    }

    /// Send the pending commit (its meta written last) and wait for the owner's answer.
    fn sync(&mut self) -> Result<()> {
        let Some(meta) = self.pending_meta.take() else {
            return Ok(());
        };
        let pages = std::mem::take(&mut self.pending);
        self.conn.commit(meta, pages)
    }

    /// The adopted snapshot's page count (every page a reader of it may name lies below it), or the
    /// space a commit in progress reserved.
    fn size(&mut self) -> Result<u64> {
        let (_, page_count) = crate::format::meta_txid_and_page_count(&self.conn.meta())
            .ok_or_else(|| EngineError::new(SqlState::DataCorrupted, "invalid meta page"))?;
        Ok((u64::from(page_count) * self.page_size()).max(self.reserved))
    }

    fn set_size(&mut self, bytes: u64) -> Result<()> {
        if self.conn.read_only() {
            return Err(client_read_only());
        }
        self.reserved = self.reserved.max(bytes);
        Ok(())
    }

    fn discard_pending(&mut self) {
        self.pending.clear();
        self.pending_meta = None;
    }

    fn batches_reads(&self) -> bool {
        self.conn.read_ahead
    }

    /// The meta slots and a commit's pending pages are local; every other page comes from the owner
    /// in one batched fetch.
    fn read_pages(&mut self, indices: &[u32], page_size: u32) -> Result<Vec<Vec<u8>>> {
        let remote: Vec<u32> = indices
            .iter()
            .copied()
            .filter(|index| *index >= 2 && !self.pending.contains_key(index))
            .collect();
        let mut fetched = if remote.is_empty() {
            Vec::new()
        } else {
            self.conn.read_pages(&remote)?
        }
        .into_iter();
        let mut pages = Vec::with_capacity(indices.len());
        for &index in indices {
            let page = if index < 2 {
                self.conn.meta()
            } else if let Some(page) = self.pending.get(&index) {
                page.clone()
            } else {
                fetched.next().expect("one fetched page per remote index")
            };
            if page.len() != page_size as usize {
                return Err(EngineError::new(
                    SqlState::ProtocolViolation,
                    "the owner sent a page of the wrong size",
                ));
            }
            pages.push(page);
        }
        Ok(pages)
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
