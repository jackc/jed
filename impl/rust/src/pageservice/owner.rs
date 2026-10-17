//! The page-service owner (spec/design/page-service.md §4.1, §5; spec/pageservice/README.md §6.2,
//! §6.4): the presence-EX holder serves pages of its database to page clients over its endpoint and
//! commits their CommitDeltas. Each connection runs on its own thread, so a client request never
//! blocks the owner's own sessions. A connection's pins join the owner's reader registry, so the
//! reclamation watermark covers every process (transactions.md §8).
//!
//! The registry-side state — the published meta, the invalidation window, and the resolution window
//! ([`ServiceWindow`]) — lives in the shared core's reader registry, under the lock that orders reader
//! pins against publication, so a `begin_read` always sees a consistent (snapshot, meta, window)
//! triple.
//!
//! The writer gate (page-service.md §5.5) is [`WriterGate`], shared by the owner's own writers (after
//! the in-process writer gate), its compaction, and client write transactions (`begin_write`). A
//! client commit is published structurally — meta, page accounting, invalidation and resolution
//! windows — and the owner's own sessions adopt its catalog at their next transaction begin
//! (`Shared::adopt_foreign`), so nothing a client commits can fail after it is durable
//! (page-service.md §7).
//!
//! Handoff (README.md §6.5): an owner whose preference is `yield`, or whose own handle is read-only,
//! answers `request_ownership` by draining and releasing ownership (`shared/handoff.rs`); a graceful
//! close drains its clients first. While draining, client begins are parked unanswered and the
//! owner's own begins wait ([`PageServiceOwner::wait_admission`]).
//!
//! Lock order: the commit lock, then a connection's state, then the handoff state, then the reader
//! registry; a connection's state before the storage and the gate's own mutex. Nothing takes a
//! connection's state while holding the handoff state, the registry, the storage, or the gate's
//! mutex.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::coordinator::{FileCoordinator, LockHost};
use crate::error::{EngineError, Result, SqlState};
use crate::file::Ownership;
use crate::filehost::{FileHost, PageReader};
use crate::pageservice::codec::{
    BeginRead, BeginWrite, COMMIT_STATUS_COMMITTED, COMMIT_STATUS_NOT_COMMITTED,
    COMMIT_STATUS_UNKNOWN, CommitBegin, CommitPages, CommitStatusReply, Committed,
    ERROR_FLAG_FATAL, ErrorReply, HELLO_FLAG_READ_ONLY, HELLO_OK_FLAG_CONNECTION_READ_ONLY,
    HELLO_OK_FLAG_OWNER_READ_ONLY, HELLO_OK_FLAG_OWNER_YIELDS, Hello, HelloOk, INVALIDATE_ALL,
    INVALIDATE_EXACT, INVALIDATION_WINDOW_COMMITS, Id16, MAX_INVALIDATION_ENTRIES,
    MAX_READ_PAGES_CAP, Message, NOTICE_SHUTDOWN_BEGUN, NOTICE_YIELD_BEGUN, NOTICE_YIELD_CANCELLED,
    Notice, OWNERSHIP_REFUSED, OWNERSHIP_YIELDING, OwnerRecord, OwnershipReply, PROTOCOL_VERSION,
    Pages, encode_frame, max_read_pages, read_frame,
};
use crate::pageservice::delta::{Delta, DeltaBase, begin_delta};
use crate::pageservice::net::{PsListener, PsStream};
use crate::shared::Shared;

/// The resolution window keeps the newest commits, like the invalidation window (README.md §4).
const RESOLUTION_WINDOW_COMMITS: usize = INVALIDATION_WINDOW_COMMITS as usize;

/// The commit-acknowledgement test hooks (README.md §6.4–§6.5): acknowledge normally, drop the
/// connection, or exit the process instead of acknowledging the next client commit.
const ACK_NORMAL: u8 = 0;
const ACK_HANG_UP: u8 = 1;
const ACK_CRASH: u8 = 2;

/// An owner's limits (page-service.md §11.1), read from its own handle's options.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OwnerLimits {
    pub(crate) idle_in_transaction: Option<Duration>,
    pub(crate) idle_session: Option<Duration>,
    pub(crate) yield_grace: Duration,
    pub(crate) shutdown_grace: Duration,
    /// `0` is unlimited.
    pub(crate) max_connections: usize,
}

impl OwnerLimits {
    pub(crate) fn from_options(opts: &crate::file::OpenOptions) -> OwnerLimits {
        let positive = |ms: u64| (ms > 0).then(|| Duration::from_millis(ms));
        OwnerLimits {
            idle_in_transaction: positive(opts.idle_in_transaction_timeout_ms),
            idle_session: opts.idle_session_timeout_ms.and_then(positive),
            yield_grace: Duration::from_millis(
                opts.yield_grace_ms
                    .unwrap_or(crate::file::DEFAULT_YIELD_GRACE_MS),
            ),
            shutdown_grace: Duration::from_millis(
                opts.shutdown_grace_ms
                    .unwrap_or(crate::file::DEFAULT_SHUTDOWN_GRACE_MS),
            ),
            max_connections: opts.max_connections,
        }
    }
}

/// An owner's handoff state (README.md §6.5).
#[derive(Clone, Copy, PartialEq, Eq)]
enum OwnerState {
    Serving,
    Yielding,
    Closing,
}

/// The handoff state behind [`PageServiceOwner::handoff`].
struct HandoffState {
    state: OwnerState,
    /// The connection a yield answers.
    requester: Option<u64>,
    /// A yield is draining: the owner's own transaction begins wait ([`PageServiceOwner::wait_admission`]).
    draining: bool,
}

/// A fresh incarnation id: 16 random bytes, redrawn if all zero (README.md §4).
pub(crate) fn new_incarnation() -> Result<Id16> {
    let mut id = [0u8; 16];
    while id == [0u8; 16] {
        getrandom::fill(&mut id).map_err(|e| {
            EngineError::new(
                SqlState::IoError,
                format!("no OS entropy for a page-service incarnation: {e}"),
            )
        })?;
    }
    Ok(id)
}

/// Read the meta slot that published `txid` through `paging`. A commit writes slot `txid & 1`
/// unless a relaxed batch put it in the other slot (format.md, *Slot alternation*), so both are
/// tried; the parse check guards a slot a later commit already overwrote.
pub(crate) fn committed_meta(paging: &crate::paging::SharedPaging, txid: u64) -> Result<Vec<u8>> {
    let mut pager = paging.pager();
    for slot in [(txid & 1) as u32, ((txid & 1) ^ 1) as u32] {
        let block = pager.read_block(slot)?;
        if crate::format::meta_txid_and_page_count(&block).is_some_and(|(t, _)| t == txid) {
            return Ok(block);
        }
    }
    Err(EngineError::new(
        SqlState::DataCorrupted,
        "no meta slot carries the committed txid",
    ))
}

/// The meta in the slot other than `txid`'s when it holds the commit just before it — the second
/// seed of the resolution window (README.md §4).
pub(crate) fn previous_meta(paging: &crate::paging::SharedPaging, txid: u64) -> Option<Vec<u8>> {
    if txid == 0 {
        return None;
    }
    // A commit writes slot txid & 1 unless a relaxed batch put it in the other slot (format.md,
    // *Slot alternation*), so both are tried.
    let mut pager = paging.pager();
    for slot in 0..2 {
        let Ok(block) = pager.read_block(slot) else {
            continue;
        };
        if crate::format::meta_txid_and_page_count(&block).is_some_and(|(t, _)| t == txid - 1) {
            return Some(block);
        }
    }
    None
}

/// The owner's published meta, invalidation window, and resolution window (README.md §4). Lives in
/// the reader registry and changes only together with the published version.
pub(crate) struct ServiceWindow {
    incarnation: Id16,
    meta: Vec<u8>,
    /// The published version: what `begin_read` and `begin_write` pin. A client commit is published
    /// here before the owner's own roots adopt it.
    txid: u64,
    page_count: u32,
    /// The written page indices of the most recent commits, oldest first, each sorted.
    window: VecDeque<(u64, Vec<u32>)>,
    /// Every commit after this txid is in `window`.
    window_start: u64,
    /// The `(txid, digest)` pairs of the most recent published commits, oldest first.
    resolution: VecDeque<(u64, u64)>,
}

impl ServiceWindow {
    /// The window of an owner whose committed snapshot is `txid` with meta page `meta`.
    pub(crate) fn new(incarnation: Id16, txid: u64, meta: Vec<u8>) -> ServiceWindow {
        let page_count = crate::format::meta_txid_and_page_count(&meta).map_or(0, |(_, n)| n);
        let mut resolution = VecDeque::new();
        resolution.push_back((txid, crate::format::commit_crc64(0, &meta)));
        ServiceWindow {
            incarnation,
            meta,
            txid,
            page_count,
            window: VecDeque::new(),
            window_start: txid,
            resolution,
        }
    }

    /// Seed the resolution window with the commit before the recovered one (README.md §4).
    pub(crate) fn seed_previous(&mut self, txid: u64, meta: &[u8]) {
        self.resolution
            .push_front((txid, crate::format::commit_crc64(0, meta)));
    }

    pub(crate) fn incarnation(&self) -> Id16 {
        self.incarnation
    }

    pub(crate) fn meta(&self) -> &[u8] {
        &self.meta
    }

    /// The published version.
    pub(crate) fn txid(&self) -> u64 {
        self.txid
    }

    /// The current snapshot's page count. Page counts only grow between compactions and a pin
    /// blocks compaction, so it bounds every page of every pinned snapshot.
    pub(crate) fn page_count(&self) -> u32 {
        self.page_count
    }

    /// Record a main-database commit: its meta page, the indices it wrote (body, free-list, and
    /// manifest pages; never the meta slots), and its resolution pair.
    pub(crate) fn published(&mut self, txid: u64, meta: Vec<u8>, mut written: Vec<u32>) {
        self.page_count = crate::format::meta_txid_and_page_count(&meta).map_or(0, |(_, n)| n);
        self.record_resolution(txid, &meta);
        self.meta = meta;
        self.txid = txid;
        written.sort_unstable();
        written.dedup();
        self.window.push_back((txid, written));
        while self.window.len() > INVALIDATION_WINDOW_COMMITS as usize {
            let (dropped, _) = self.window.pop_front().expect("window is non-empty");
            self.window_start = dropped;
        }
    }

    fn record_resolution(&mut self, txid: u64, meta: &[u8]) {
        self.resolution
            .push_back((txid, crate::format::commit_crc64(0, meta)));
        while self.resolution.len() > RESOLUTION_WINDOW_COMMITS {
            self.resolution.pop_front();
        }
    }

    /// Restart the invalidation window at a compacted snapshot: every older cache is invalid. The
    /// compaction joins the resolution window.
    pub(crate) fn reset(&mut self, txid: u64, meta: Vec<u8>) {
        self.page_count = crate::format::meta_txid_and_page_count(&meta).map_or(0, |(_, n)| n);
        self.record_resolution(txid, &meta);
        self.meta = meta;
        self.txid = txid;
        self.window.clear();
        self.window_start = txid;
    }

    /// Answer `begin_read`'s cache question for a client whose cache is `(incarnation, last_txid)`
    /// and whose new pin is `txid` (README.md §3.2).
    pub(crate) fn invalidation(
        &self,
        incarnation: &Id16,
        last_txid: u64,
        txid: u64,
    ) -> (u8, Vec<u32>) {
        if *incarnation != self.incarnation
            || last_txid == 0
            || last_txid > txid
            || last_txid < self.window_start
        {
            return (INVALIDATE_ALL, Vec::new());
        }
        let mut indices = BTreeSet::new();
        for (commit, written) in &self.window {
            if *commit <= last_txid || *commit > txid {
                continue;
            }
            indices.extend(written.iter().copied());
            if indices.len() > MAX_INVALIDATION_ENTRIES as usize {
                return (INVALIDATE_ALL, Vec::new());
            }
        }
        (INVALIDATE_EXACT, indices.into_iter().collect())
    }

    /// Answer `commit_status` (page-service.md §8.4): committed when the window holds `txid` with
    /// this digest, not committed when it holds another digest or `txid` is above the published
    /// version, unknown otherwise.
    pub(crate) fn resolve(&self, txid: u64, digest: u64) -> u8 {
        if txid > self.txid {
            return COMMIT_STATUS_NOT_COMMITTED;
        }
        match self.resolution.iter().find(|(t, _)| *t == txid) {
            Some((_, d)) if *d == digest => COMMIT_STATUS_COMMITTED,
            Some(_) => COMMIT_STATUS_NOT_COMMITTED,
            None => COMMIT_STATUS_UNKNOWN,
        }
    }
}

/// The writer gate (page-service.md §5.5): a one-slot semaphore held by whoever is writing the
/// database — an owner write transaction, a compaction, or a client between `write_begun` and its
/// commit's publication.
pub(crate) struct WriterGate {
    state: Mutex<GateState>,
    freed: Condvar,
}

struct GateState {
    held: bool,
    /// The owner stopped serving: waiters give up.
    closing: bool,
}

impl WriterGate {
    fn new() -> WriterGate {
        WriterGate {
            state: Mutex::new(GateState {
                held: false,
                closing: false,
            }),
            freed: Condvar::new(),
        }
    }

    /// Take the gate, waiting at most `timeout_ms` (0: no deadline): `55P03` on expiry. A wait
    /// also ends (`08006`) when `cancelled` turns true or the owner stops serving; whoever sets
    /// either calls [`wake`](Self::wake).
    pub(crate) fn acquire(&self, timeout_ms: u64, cancelled: &dyn Fn() -> bool) -> Result<()> {
        let deadline = (timeout_ms > 0).then(|| Instant::now() + Duration::from_millis(timeout_ms));
        let mut state = self.state.lock().expect("writer gate lock poisoned");
        loop {
            if !state.held {
                state.held = true;
                return Ok(());
            }
            if state.closing {
                return Err(EngineError::new(
                    SqlState::ConnectionFailure,
                    "the database owner is closing",
                ));
            }
            if cancelled() {
                return Err(EngineError::new(
                    SqlState::ConnectionFailure,
                    "the connection closed while waiting for the writer gate",
                ));
            }
            match deadline {
                None => {
                    state = self.freed.wait(state).expect("writer gate lock poisoned");
                }
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(EngineError::new(
                            SqlState::LockNotAvailable,
                            "could not obtain the database writer lock",
                        ));
                    }
                    state = self
                        .freed
                        .wait_timeout(state, deadline - now)
                        .expect("writer gate lock poisoned")
                        .0;
                }
            }
        }
    }

    /// Whether the gate is held (a drain waits for its holder).
    pub(crate) fn is_held(&self) -> bool {
        self.state.lock().expect("writer gate lock poisoned").held
    }

    /// Take the gate only if it is free (compaction never waits).
    pub(crate) fn try_acquire(&self) -> bool {
        let mut state = self.state.lock().expect("writer gate lock poisoned");
        if state.held {
            return false;
        }
        state.held = true;
        true
    }

    pub(crate) fn release(&self) {
        self.state.lock().expect("writer gate lock poisoned").held = false;
        self.freed.notify_all();
    }

    /// Wake every waiter to re-check its cancellation.
    fn wake(&self) {
        let _state = self.state.lock().expect("writer gate lock poisoned");
        self.freed.notify_all();
    }

    fn close(&self) {
        self.state
            .lock()
            .expect("writer gate lock poisoned")
            .closing = true;
        self.freed.notify_all();
    }
}

/// One connection's pins, counted per txid, the write pin included; each is also one count in the
/// reader registry.
pub(crate) type ConnPins = BTreeMap<u64, usize>;

struct Conns {
    closed: bool,
    next_id: u64,
    /// The open connections, so close can shut them down and a notice can reach each.
    open: HashMap<u64, Arc<ServerConn>>,
    threads: Vec<JoinHandle<()>>,
}

/// A client write transaction: the connection holds the gate and a write pin on `base`.
struct WriteTxn {
    base: u64,
    check: DeltaBase,
    /// The CommitDelta being received (between `commit_begin` and `commit_end`).
    delta: Option<Delta>,
    /// A rejected delta: its remaining frames are discarded and `commit_end` reports it.
    failure: Option<EngineError>,
    /// Whether the owner's pager has begun this commit (a page was written).
    started: bool,
    /// Set at the point of no return: the commit completes whatever the connection does.
    finishing: bool,
}

/// A begin a draining owner left unanswered (README.md §6.5); a cancelled yield answers it.
enum Parked {
    Read(u32, BeginRead),
    Write(u32, BeginWrite),
}

/// One connection's mutable state, behind [`ServerConn::state`].
struct ConnState {
    read_only: bool,
    pins: ConnPins,
    write: Option<WriteTxn>,
    /// A `begin_write` is waiting for the gate.
    waiting: bool,
    /// The base of a write transaction the idle-in-transaction timeout revoked; the next
    /// write-path request reports `25P03`.
    revoked: Option<u64>,
    closed: bool,
    /// When the held write transaction is revoked unless a frame arrives first.
    idle_deadline: Option<Instant>,
    /// When the connection is closed for the idle-session timeout unless a frame arrives first
    /// (page-service.md §11.1).
    session_deadline: Option<Instant>,
    /// Begins a draining owner left unanswered.
    parked: Vec<Parked>,
}

/// One client connection on the owner side.
struct ServerConn {
    id: u64,
    /// The write half: replies come from the serve thread and from a `begin_write`'s gate waiter.
    sender: Mutex<Box<dyn PsStream>>,
    /// A second handle on the socket, so the owner can shut it down without the send lock.
    control: Box<dyn PsStream>,
    /// Set when the serve thread ends, so a gate waiter gives up.
    closed: AtomicBool,
    state: Mutex<ConnState>,
    /// Wakes the idle watchdog when the deadline moves or the connection closes.
    idle: Condvar,
    watchdog: Mutex<Option<JoinHandle<()>>>,
}

impl ServerConn {
    fn send(&self, request_id: u32, message: &Message) -> bool {
        match encode_frame(request_id, message) {
            Ok(frame) => self
                .sender
                .lock()
                .expect("page-service sender lock poisoned")
                .write_all(&frame)
                .is_ok(),
            Err(_) => false,
        }
    }

    /// Answer a request with a fatal error; the caller then closes the connection.
    fn fail(&self, request_id: u32, error: &EngineError) {
        self.send(request_id, &error_reply(error, ERROR_FLAG_FATAL));
    }

    /// Drop the connection (the commit-resolution test hook, the idle-session timeout, a release).
    fn hang_up(&self) {
        self.control.close_both();
    }
}

/// The serving state of an owner: its read handle, endpoint, connections, and writer gate. Shut down
/// explicitly ([`shutdown`](Self::shutdown)) before the owner releases presence.
pub(crate) struct PageServiceOwner {
    shared: Weak<Shared>,
    db_path: PathBuf,
    page_size: u32,
    read_only: bool,
    preference: Ownership,
    limits: OwnerLimits,
    incarnation: Id16,
    uid: u32,
    /// Presence EX for this ownership; releasing ownership drops it.
    coord: Mutex<Option<FileCoordinator>>,
    /// The handoff state; `admission` wakes the owner's own begins when a drain ends.
    handoff: Mutex<HandoffState>,
    admission: Condvar,
    /// Mirrors `draining` so every owner transaction begin checks it without a lock.
    draining_now: AtomicBool,
    /// The handle's lock host (the owner record, the endpoint, the drain's clock) and file host (the
    /// read handle): the OS, or the simulator's.
    host: Arc<dyn LockHost>,
    files: Arc<dyn FileHost>,
    /// The owner's own read handle on the database: page reads are positioned reads on it,
    /// independent of the pager's lock, so a client never waits on the owner's commit. Compaction
    /// replaces it.
    file: RwLock<Box<dyn PageReader>>,
    endpoint: Option<PathBuf>,
    conns: Mutex<Conns>,
    /// The bound endpoint, so a shutdown can wake the accept loop.
    listener: Mutex<Option<Arc<dyn PsListener>>>,
    accept: Mutex<Option<JoinHandle<()>>>,
    pub(crate) gate: WriterGate,
    /// Held from a client commit's point of no return through its publication, and by adoption,
    /// so neither observes a half-published commit and `commit_status` can wait for one.
    pub(crate) commit_lock: Mutex<()>,
    /// The txid of the newest client commit published here; the owner's roots adopt it at their
    /// next transaction begin (`Shared::adopt_foreign`).
    pub(crate) foreign: AtomicU64,
    /// The commit-acknowledgement test hook (`ACK_*`).
    next_ack: AtomicU8,
}

impl PageServiceOwner {
    /// Prepare the owner of the database at `db_path` for `shared` under `incarnation`: its read
    /// handle and endpoint path. `coord` holds presence EX for this ownership. [`serve`](Self::serve)
    /// starts serving once the role is installed.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        shared: Weak<Shared>,
        db_path: &Path,
        incarnation: Id16,
        page_size: u32,
        read_only: bool,
        preference: Ownership,
        limits: OwnerLimits,
        coord: Option<FileCoordinator>,
        host: Arc<dyn LockHost>,
        files: Arc<dyn FileHost>,
    ) -> Result<Arc<PageServiceOwner>> {
        let file = files.open_reader(db_path)?;
        let uid = host.current_uid(db_path)?;
        let endpoint_path = host.endpoint_path(db_path, uid, &incarnation);
        Ok(Arc::new(PageServiceOwner {
            shared,
            db_path: db_path.to_path_buf(),
            page_size,
            read_only,
            preference,
            limits,
            incarnation,
            uid,
            coord: Mutex::new(coord),
            handoff: Mutex::new(HandoffState {
                state: OwnerState::Serving,
                requester: None,
                draining: false,
            }),
            admission: Condvar::new(),
            draining_now: AtomicBool::new(false),
            host,
            files,
            file: RwLock::new(file),
            endpoint: endpoint_path,
            conns: Mutex::new(Conns {
                closed: false,
                next_id: 0,
                open: HashMap::new(),
                threads: Vec::new(),
            }),
            listener: Mutex::new(None),
            accept: Mutex::new(None),
            gate: WriterGate::new(),
            commit_lock: Mutex::new(()),
            foreign: AtomicU64::new(0),
            next_ack: AtomicU8::new(ACK_NORMAL),
        }))
    }

    /// Write the owner record, bind the endpoint, and start accepting. An owner that cannot bind (no
    /// private endpoint directory, or a failed bind) still owns and records that it does not serve.
    pub(crate) fn serve(self: &Arc<Self>) -> Result<()> {
        let record = OwnerRecord {
            serving: self.endpoint.is_some(),
            uid: self.uid,
            incarnation: self.incarnation,
            pid: std::process::id(),
            endpoint: self
                .endpoint
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        self.host.write_owner_record(&self.db_path, &record)?;
        let Some(path) = &self.endpoint else {
            return Ok(());
        };
        let listener: Arc<dyn PsListener> = match self.host.listen(path) {
            Ok(listener) => Arc::from(listener),
            Err(e) => {
                let _ = self.host.write_owner_record(
                    &self.db_path,
                    &OwnerRecord {
                        serving: false,
                        endpoint: String::new(),
                        ..record
                    },
                );
                return Err(EngineError::new(
                    SqlState::IoError,
                    format!("bind page-service endpoint: {e}"),
                ));
            }
        };
        *self.listener.lock().expect("listener lock poisoned") = Some(Arc::clone(&listener));
        let acceptor = Arc::clone(self);
        *self.accept.lock().expect("accept lock poisoned") =
            Some(thread::spawn(move || acceptor.accept_loop(listener)));
        Ok(())
    }

    /// Wake the blocking accept; it sees `closed` and returns.
    fn wake_accept(&self) {
        let listener = self
            .listener
            .lock()
            .expect("listener lock poisoned")
            .clone();
        if let Some(listener) = listener {
            listener.close();
        }
    }

    /// Accept until the owner closes, then drop the endpoint (the socket closes with the last handle
    /// on it, so a late dial is refused rather than left unanswered).
    fn accept_loop(self: Arc<Self>, listener: Arc<dyn PsListener>) {
        self.accept_connections(listener.as_ref());
        self.listener.lock().expect("listener lock poisoned").take();
    }

    fn accept_connections(self: &Arc<Self>, listener: &dyn PsListener) {
        loop {
            let stream = match listener.accept() {
                Ok(Some(stream)) => stream,
                Ok(None) => return,
                Err(_) => continue,
            };
            let mut conns = self.conns.lock().expect("connections lock poisoned");
            if conns.closed {
                stream.close_both();
                return;
            }
            let (Ok(control), Ok(sender)) = (stream.clone_stream(), stream.clone_stream()) else {
                stream.close_both();
                continue;
            };
            conns.next_id += 1;
            let id = conns.next_id;
            let conn = Arc::new(ServerConn {
                id,
                sender: Mutex::new(sender),
                control,
                closed: AtomicBool::new(false),
                state: Mutex::new(ConnState {
                    read_only: false,
                    pins: ConnPins::new(),
                    write: None,
                    waiting: false,
                    revoked: None,
                    closed: false,
                    idle_deadline: None,
                    session_deadline: self.limits.idle_session.map(|t| Instant::now() + t),
                    parked: Vec::new(),
                }),
                idle: Condvar::new(),
                watchdog: Mutex::new(None),
            });
            conns.open.insert(id, Arc::clone(&conn));
            conns.threads.retain(|t| !t.is_finished());
            let owner = Arc::clone(self);
            conns
                .threads
                .push(thread::spawn(move || owner.serve_connection(conn, stream)));
        }
    }

    /// Make the next client commit drop its connection instead of acknowledging (test hook).
    pub(crate) fn hang_up_next_ack(&self) {
        self.next_ack.store(ACK_HANG_UP, Ordering::Release);
    }

    /// Make the next client commit exit the process instead of acknowledging, after the commit is
    /// durable (test hook, README.md §6.5).
    pub(crate) fn crash_before_next_ack(&self) {
        self.next_ack.store(ACK_CRASH, Ordering::Release);
    }

    /// The open connections.
    fn connections(&self) -> Vec<Arc<ServerConn>> {
        self.conns
            .lock()
            .expect("connections lock poisoned")
            .open
            .values()
            .cloned()
            .collect()
    }

    /// Send a notice to every connection (README.md §3.7).
    fn broadcast(&self, kind: u8, connection_id: u64) {
        let notice = Message::Notice(Notice {
            kind,
            connection_id,
        });
        for conn in self.connections() {
            conn.send(0, &notice);
        }
    }

    /// Block an owner session's transaction begin while a yield drains (README.md §6.5). Returns
    /// whether it waited; the common case is one atomic load.
    pub(crate) fn wait_admission(&self) -> bool {
        if !self.draining_now.load(Ordering::Acquire) {
            return false;
        }
        let mut handoff = self.handoff.lock().expect("handoff lock poisoned");
        while handoff.draining {
            handoff = self.admission.wait(handoff).expect("handoff lock poisoned");
        }
        true
    }

    /// Whether client begins are answered rather than parked.
    fn admitting(&self) -> bool {
        self.handoff.lock().expect("handoff lock poisoned").state == OwnerState::Serving
    }

    /// Release the owner's own waiting begins.
    pub(crate) fn end_drain(&self) {
        let mut handoff = self.handoff.lock().expect("handoff lock poisoned");
        handoff.draining = false;
        self.draining_now.store(false, Ordering::Release);
        drop(handoff);
        self.admission.notify_all();
    }

    /// Whether no transaction a drain waits for is running: with `own`, the owner's own readers and
    /// writer as well as every connection's; otherwise only the connections'.
    fn drained(&self, shared: &Shared, own: bool) -> bool {
        if own && (shared.has_live_readers() || self.gate.is_held()) {
            return false;
        }
        self.connections().iter().all(|conn| {
            let state = conn.state.lock().expect("connection state lock poisoned");
            state.pins.is_empty() && state.write.is_none()
        })
    }

    /// Whether the connection `id` ended.
    fn connection_gone(&self, id: u64) -> bool {
        let conn = self
            .conns
            .lock()
            .expect("connections lock poisoned")
            .open
            .get(&id)
            .cloned();
        conn.is_none_or(|conn| conn.closed.load(Ordering::Acquire))
    }

    /// Wait for running transactions to finish, up to `grace` (README.md §6.5). `true` when the
    /// yield's `requester` disconnected first (the yield is cancelled).
    /// Polls through the lock host, whose clock the simulator advances.
    fn drain(&self, shared: &Shared, grace: Duration, own: bool, requester: Option<u64>) -> bool {
        let deadline = self.host.now() + grace;
        loop {
            if requester.is_some_and(|id| self.connection_gone(id)) {
                return true;
            }
            if self.drained(shared, own) || self.host.now() >= deadline {
                return false;
            }
            self.host.pause();
        }
    }

    /// Answer `request_ownership` (README.md §6.5): an owner whose preference is `yield`, or whose
    /// handle is read-only, yields to the first requester while it is serving.
    fn request_ownership(self: &Arc<Self>, conn: &ServerConn) -> Message {
        let reply = |status| Message::OwnershipReply(OwnershipReply { status });
        let mut handoff = self.handoff.lock().expect("handoff lock poisoned");
        if handoff.state == OwnerState::Yielding && handoff.requester == Some(conn.id) {
            return reply(OWNERSHIP_YIELDING);
        }
        if handoff.state != OwnerState::Serving
            || (self.preference != Ownership::Yield && !self.read_only)
        {
            return reply(OWNERSHIP_REFUSED);
        }
        handoff.state = OwnerState::Yielding;
        handoff.requester = Some(conn.id);
        handoff.draining = true;
        self.draining_now.store(true, Ordering::Release);
        drop(handoff);
        let owner = Arc::clone(self);
        let requester = conn.id;
        thread::spawn(move || owner.yield_to(requester));
        reply(OWNERSHIP_YIELDING)
    }

    /// Run an accepted ownership request: notice, drain for the yield grace, then release ownership
    /// and re-elect as a paused client — or, when the requester disconnected first, cancel
    /// (README.md §6.5).
    fn yield_to(self: Arc<Self>, requester: u64) {
        self.broadcast(NOTICE_YIELD_BEGUN, requester);
        let Some(shared) = self.shared.upgrade() else {
            self.end_drain();
            return;
        };
        if self.drain(&shared, self.limits.yield_grace, true, Some(requester)) {
            self.cancel_yield(&shared);
            return;
        }
        shared.yield_ownership(&self);
    }

    /// Resume serving after the requester disconnected: notice, admit again, and answer the parked
    /// begins.
    fn cancel_yield(self: &Arc<Self>, shared: &Shared) {
        self.broadcast(NOTICE_YIELD_CANCELLED, 0);
        {
            let mut handoff = self.handoff.lock().expect("handoff lock poisoned");
            if handoff.state == OwnerState::Yielding {
                handoff.state = OwnerState::Serving;
            }
            handoff.requester = None;
        }
        self.end_drain();
        for conn in self.connections() {
            let mut state = conn.state.lock().expect("connection state lock poisoned");
            for parked in std::mem::take(&mut state.parked) {
                match parked {
                    Parked::Read(request_id, m) => {
                        match shared.page_service_begin_read(
                            &mut state.pins,
                            &m.incarnation,
                            m.last_txid,
                        ) {
                            Some(begun) => {
                                conn.send(request_id, &Message::ReadBegun(begun));
                            }
                            None => conn.hang_up(),
                        }
                    }
                    Parked::Write(request_id, m) => {
                        let owner = Arc::clone(self);
                        let waiter = Arc::clone(&conn);
                        thread::spawn(move || owner.wait_for_gate(waiter, request_id, m));
                    }
                }
            }
        }
    }

    /// A serving owner's close (README.md §6.5): notice, then drain its clients' transactions for
    /// the shutdown grace. The caller then releases ownership.
    pub(crate) fn close_gracefully(&self, shared: &Shared) {
        self.handoff.lock().expect("handoff lock poisoned").state = OwnerState::Closing;
        self.end_drain();
        if self.accept.lock().expect("accept lock poisoned").is_none() {
            return;
        }
        self.broadcast(NOTICE_SHUTDOWN_BEGUN, 0);
        self.drain(shared, self.limits.shutdown_grace, false, None);
    }

    /// Release presence EX (the end of a release, after the endpoint and record are removed).
    pub(crate) fn release_presence(&self) {
        drop(self.coord.lock().expect("coordinator lock poisoned").take());
    }

    /// Reopen the read handle after compaction replaced the file. The caller holds the writer gate
    /// and no pin exists, so no page read is in flight.
    pub(crate) fn reopen_file(&self) -> Result<()> {
        let file = self.files.open_reader(&self.db_path)?;
        *self.file.write().expect("page-service file lock poisoned") = file;
        Ok(())
    }

    /// Stop serving: no new connections, and every client sees EOF (releasing its pins and the
    /// gate). A client commit past its point of no return finishes first (its serve thread is
    /// joined). The caller still holds presence EX; [`remove_serving`](Self::remove_serving) follows.
    pub(crate) fn shutdown(&self) {
        self.gate.close();
        let (conns, threads) = {
            let mut conns = self.conns.lock().expect("connections lock poisoned");
            conns.closed = true;
            (
                conns.open.values().cloned().collect::<Vec<_>>(),
                std::mem::take(&mut conns.threads),
            )
        };
        let accept = self.accept.lock().expect("accept lock poisoned").take();
        if let Some(accept) = accept {
            self.wake_accept();
            join_unless_current(accept);
        }
        for conn in conns {
            conn.hang_up();
        }
        for thread in threads {
            join_unless_current(thread);
        }
    }

    /// Remove the endpoint and the owner record, while presence EX is still held.
    pub(crate) fn remove_serving(&self) {
        if let Some(path) = &self.endpoint {
            self.host.remove_endpoint(path);
        }
        self.host.remove_owner_record(&self.db_path);
    }

    /// The owner's response to a write or sync failure in a client commit (page-service.md §5.6):
    /// its storage handle is poisoned, so it closes every connection and accepts no more. The
    /// owner's own handle then refuses commits until it is reopened.
    pub(crate) fn stop_serving(&self) {
        self.gate.close();
        {
            let mut conns = self.conns.lock().expect("connections lock poisoned");
            conns.closed = true;
            for conn in conns.open.values() {
                conn.hang_up();
            }
        }
        self.wake_accept();
    }

    /// Run one connection until EOF or a protocol violation, then abort its write transaction
    /// (unless past the point of no return) and release its pins.
    fn serve_connection(self: Arc<Self>, conn: Arc<ServerConn>, mut stream: Box<dyn PsStream>) {
        let id = conn.id;
        if self.limits.idle_in_transaction.is_some() || self.limits.idle_session.is_some() {
            let owner = Arc::clone(&self);
            let watched = Arc::clone(&conn);
            *conn.watchdog.lock().expect("watchdog lock poisoned") =
                Some(thread::spawn(move || owner.watch_idle(watched)));
        }
        self.serve_frames(&conn, &mut stream);
        self.end_connection(&conn);
        self.conns
            .lock()
            .expect("connections lock poisoned")
            .open
            .remove(&id);
        // Closed last, after the connection's state is released: the simulator treats a closed end
        // as settled.
        stream.close_both();
    }

    /// The end of a connection: abort its write transaction unless it is past the point of no
    /// return, release its pins, and stop its watchdog.
    fn end_connection(&self, conn: &Arc<ServerConn>) {
        conn.closed.store(true, Ordering::Release);
        self.gate.wake();
        // Declared before the state guard, so a final drop of the core never runs under it.
        let shared = self.shared.upgrade();
        {
            let mut state = conn.state.lock().expect("connection state lock poisoned");
            state.closed = true;
            state.parked.clear();
            if state.write.as_ref().is_some_and(|w| !w.finishing) {
                self.abort_write_locked(shared.as_deref(), &mut state);
            }
            if let Some(shared) = &shared {
                shared.page_service_release_pins(&mut state.pins);
            }
            conn.idle.notify_all();
        }
        let watchdog = conn.watchdog.lock().expect("watchdog lock poisoned").take();
        if let Some(watchdog) = watchdog {
            join_unless_current(watchdog);
        }
    }

    fn serve_frames(self: &Arc<Self>, conn: &Arc<ServerConn>, stream: &mut Box<dyn PsStream>) {
        let mut hello_done = false;
        loop {
            let (request_id, message) = match read_frame(stream, self.page_size) {
                Ok(Some(frame)) => frame,
                Ok(None) => return,
                Err(e) => {
                    if e.state != SqlState::ConnectionFailure {
                        conn.fail(0, &e);
                    }
                    return;
                }
            };
            self.touch(conn);
            if !hello_done {
                let Message::Hello(hello) = message else {
                    conn.fail(request_id, &violation("the first frame must be hello"));
                    return;
                };
                if !self.hello(conn, request_id, &hello) {
                    return;
                }
                hello_done = true;
                continue;
            }
            let Some(shared) = self.shared.upgrade() else {
                return; // the owner is closing
            };
            let reply = match message {
                Message::BeginRead(m) => {
                    let mut state = conn.state.lock().expect("connection state lock poisoned");
                    if !self.admitting() {
                        // Parked while the owner drains (README.md §6.5).
                        state.parked.push(Parked::Read(request_id, m));
                        drop(state);
                        drop(shared);
                        continue;
                    }
                    match shared.page_service_begin_read(
                        &mut state.pins,
                        &m.incarnation,
                        m.last_txid,
                    ) {
                        Some(begun) => Ok(Some(Message::ReadBegun(begun))),
                        None => return,
                    }
                }
                Message::ReadPages(m) => self.read_pages(&shared, conn, m.txid, &m.indices),
                Message::EndRead(m) => {
                    let mut state = conn.state.lock().expect("connection state lock poisoned");
                    if shared.page_service_end_read(&mut state.pins, m.txid) {
                        Ok(None)
                    } else {
                        Err(violation(
                            "end_read names a txid this connection has not pinned",
                        ))
                    }
                }
                Message::BeginWrite(m) => self.begin_write(conn, request_id, m),
                Message::CommitBegin(m) => self.commit_begin(&shared, conn, m),
                Message::CommitPages(m) => self.commit_pages(&shared, conn, m),
                Message::CommitEnd => self.commit_end(&shared, conn, request_id),
                Message::Abort => self.abort(&shared, conn),
                Message::RequestOwnership => Ok(Some(self.request_ownership(conn))),
                Message::CommitStatus(m) => {
                    // A commit past its point of no return holds the commit lock until published.
                    drop(self.commit_lock.lock().expect("commit lock poisoned"));
                    Ok(Some(Message::CommitStatusReply(CommitStatusReply {
                        status: shared.page_service_resolve(m.txid, m.digest),
                    })))
                }
                other => Err(violation(format!("unexpected {} message", other.name()))),
            };
            drop(shared);
            match reply {
                Ok(Some(reply)) => {
                    if !conn.send(request_id, &reply) {
                        return;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    conn.fail(request_id, &e);
                    return;
                }
            }
        }
    }

    /// Negotiate the protocol and the format (README.md §3.1). False closes the connection.
    fn hello(&self, conn: &ServerConn, request_id: u32, m: &Hello) -> bool {
        if m.protocol_min > PROTOCOL_VERSION || m.protocol_max < PROTOCOL_VERSION {
            conn.fail(
                request_id,
                &EngineError::new(
                    SqlState::FeatureNotSupported,
                    "unsupported page-service protocol; the owner speaks version 1",
                ),
            );
            return false;
        }
        if m.format_version != crate::format::FORMAT_VERSION {
            conn.fail(
                request_id,
                &EngineError::new(
                    SqlState::FeatureNotSupported,
                    format!(
                        "the client does not support this database's format_version; it requires format_version {}",
                        crate::format::FORMAT_VERSION
                    ),
                ),
            );
            return false;
        }
        // A connection is read-only exactly when the client asked for it.
        let mut flags = 0;
        if m.flags & HELLO_FLAG_READ_ONLY != 0 {
            conn.state
                .lock()
                .expect("connection state lock poisoned")
                .read_only = true;
            flags |= HELLO_OK_FLAG_CONNECTION_READ_ONLY;
        }
        if self.read_only {
            flags |= HELLO_OK_FLAG_OWNER_READ_ONLY;
        }
        if self.preference == Ownership::Yield {
            flags |= HELLO_OK_FLAG_OWNER_YIELDS;
        }
        let limit = self.limits.max_connections;
        if limit > 0
            && self
                .conns
                .lock()
                .expect("connections lock poisoned")
                .open
                .len()
                > limit
        {
            conn.fail(
                request_id,
                &EngineError::new(
                    SqlState::TooManyConnections,
                    format!(
                        "the database owner already serves its maximum number of connections ({limit})"
                    ),
                ),
            );
            return false;
        }
        conn.send(
            request_id,
            &Message::HelloOk(HelloOk {
                protocol: PROTOCOL_VERSION,
                format_version: crate::format::FORMAT_VERSION,
                page_size: self.page_size,
                connection_id: conn.id,
                flags,
                max_read_pages: max_read_pages(self.page_size),
            }),
        )
    }

    /// The requested pages of a snapshot this connection pins (README.md §3.3).
    fn read_pages(
        &self,
        shared: &Shared,
        conn: &ServerConn,
        txid: u64,
        indices: &[u32],
    ) -> Result<Option<Message>> {
        let page_count = {
            let mut state = conn.state.lock().expect("connection state lock poisoned");
            match shared.page_service_page_count(&state.pins, txid) {
                Some(page_count) => page_count,
                None => {
                    if state.revoked == Some(txid) {
                        // A read under a revoked write transaction's snapshot: report the revocation.
                        state.revoked = None;
                        return Ok(Some(idle_revoked()));
                    }
                    return Err(violation(
                        "read_pages names a txid this connection has not pinned",
                    ));
                }
            }
        };
        let limit = max_read_pages(self.page_size).min(MAX_READ_PAGES_CAP) as usize;
        if indices.is_empty() || indices.len() > limit {
            return Err(violation("read_pages names too few or too many pages"));
        }
        let file = self.file.read().expect("page-service file lock poisoned");
        let mut pages = Vec::with_capacity(indices.len());
        for &index in indices {
            if index < 2 || index >= page_count {
                return Err(violation(
                    "read_pages names a page outside the pinned snapshot",
                ));
            }
            let mut page = vec![0u8; self.page_size as usize];
            file.read_exact_at(&mut page, u64::from(index) * u64::from(self.page_size))?;
            pages.push(page);
        }
        Ok(Some(Message::Pages(Pages { pages })))
    }

    /// Start a client write transaction (README.md §3.5). The gate wait runs on its own thread, so
    /// the connection keeps serving reads meanwhile; `write_begun` (or the wait's error) is sent
    /// when the wait ends.
    fn begin_write(
        self: &Arc<Self>,
        conn: &Arc<ServerConn>,
        request_id: u32,
        m: BeginWrite,
    ) -> Result<Option<Message>> {
        let mut state = conn.state.lock().expect("connection state lock poisoned");
        if state.read_only {
            return Ok(Some(read_only_reply(
                "cannot write through a read-only page-service connection",
            )));
        }
        if self.read_only {
            return Ok(Some(read_only_reply(
                "the database owner opened the file read-only",
            )));
        }
        if state.write.is_some() || state.waiting {
            return Err(violation("begin_write while a write transaction is open"));
        }
        if state.revoked.take().is_some() {
            return Ok(Some(idle_revoked()));
        }
        state.waiting = true;
        if !self.admitting() {
            // Parked while the owner drains (README.md §6.5); a cancelled yield starts the wait.
            state.parked.push(Parked::Write(request_id, m));
            return Ok(None);
        }
        let owner = Arc::clone(self);
        let waiter = Arc::clone(conn);
        thread::spawn(move || owner.wait_for_gate(waiter, request_id, m));
        Ok(None)
    }

    /// Acquire the gate for a `begin_write`, then pin the published snapshot and answer.
    fn wait_for_gate(&self, conn: Arc<ServerConn>, request_id: u32, m: BeginWrite) {
        let result = self
            .gate
            .acquire(m.lock_timeout_ms, &|| conn.closed.load(Ordering::Acquire));
        // Declared before the state guard, so a final drop of the core never runs under it.
        let shared = self.shared.upgrade();
        // The owner's own relaxed commits are flushed before a client plans against the committed
        // meta: a client cannot name the owner's pending set in its manifest (README.md §3.5). A
        // failed flush poisons the owner, which stops serving.
        let mut flush_failed = false;
        let result = match (result, &shared) {
            (Ok(()), Some(core)) => match core.page_service_flush() {
                Ok(()) => Ok(()),
                Err(e) => {
                    self.gate.release();
                    flush_failed = true;
                    Err(e)
                }
            },
            (result, _) => result,
        };
        let mut state = conn.state.lock().expect("connection state lock poisoned");
        state.waiting = false;
        let shared = match (&shared, state.closed) {
            (Some(shared), false) => shared,
            _ => {
                if result.is_ok() {
                    self.gate.release();
                }
                return;
            }
        };
        if let Err(e) = result {
            conn.send(request_id, &error_reply(&e, 0));
            if flush_failed {
                drop(state);
                self.stop_serving();
            }
            return;
        }
        let Some((begun, check)) =
            shared.page_service_begin_write(&mut state.pins, &m.incarnation, m.last_txid)
        else {
            self.gate.release();
            return;
        };
        state.write = Some(WriteTxn {
            base: begun.txid,
            check,
            delta: None,
            failure: None,
            started: false,
            finishing: false,
        });
        self.arm_idle(&conn, &mut state);
        conn.send(request_id, &Message::WriteBegun(begun));
    }

    /// Restart the idle-in-transaction and idle-session timers on every received frame.
    fn touch(&self, conn: &ServerConn) {
        let mut state = conn.state.lock().expect("connection state lock poisoned");
        if state.write.as_ref().is_some_and(|w| !w.finishing) {
            self.arm_idle(conn, &mut state);
        }
        self.arm_session(conn, &mut state);
    }

    fn arm_idle(&self, conn: &ServerConn, state: &mut ConnState) {
        if let Some(timeout) = self.limits.idle_in_transaction {
            state.idle_deadline = Some(Instant::now() + timeout);
            conn.idle.notify_all();
        }
    }

    /// (Re)start the idle-session timer (page-service.md §11.1).
    fn arm_session(&self, conn: &ServerConn, state: &mut ConnState) {
        if let Some(timeout) = self.limits.idle_session {
            state.session_deadline = Some(Instant::now() + timeout);
            conn.idle.notify_all();
        }
    }

    /// The connection's idle watchdog: revoke a write transaction that received nothing for the
    /// idle-in-transaction timeout, and close a connection idle between transactions for the
    /// idle-session timeout, without an error frame (page-service.md §11.1).
    fn watch_idle(&self, conn: Arc<ServerConn>) {
        let mut state = conn.state.lock().expect("connection state lock poisoned");
        loop {
            if state.closed {
                return;
            }
            let now = Instant::now();
            if let Some(deadline) = state.session_deadline
                && now >= deadline
            {
                let busy = !state.pins.is_empty()
                    || state.write.is_some()
                    || state.waiting
                    || !state.parked.is_empty();
                if busy {
                    self.arm_session(&conn, &mut state);
                } else {
                    state.session_deadline = None;
                    conn.hang_up();
                }
                continue;
            }
            let next = match (state.idle_deadline, state.session_deadline) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let Some(deadline) = next else {
                state = conn
                    .idle
                    .wait(state)
                    .expect("connection state lock poisoned");
                continue;
            };
            if now < deadline {
                state = conn
                    .idle
                    .wait_timeout(state, deadline - now)
                    .expect("connection state lock poisoned")
                    .0;
                continue;
            }
            // Upgrade the core outside the state guard, so its final drop never runs under it.
            drop(state);
            let shared = self.shared.upgrade();
            state = conn.state.lock().expect("connection state lock poisoned");
            if state
                .idle_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                state.idle_deadline = None;
                self.revoke_locked(shared.as_deref(), &mut state);
            }
            drop(state);
            drop(shared);
            state = conn.state.lock().expect("connection state lock poisoned");
        }
    }

    /// End a write transaction for the idle-in-transaction timeout: the gate is released, and the
    /// next write-path request reports `25P03`.
    fn revoke_locked(&self, shared: Option<&Shared>, state: &mut ConnState) {
        let base = match &state.write {
            Some(w) if !w.finishing && !state.closed => w.base,
            _ => return,
        };
        self.abort_write_locked(shared, state);
        state.revoked = Some(base);
    }

    /// End the write transaction without committing: delta pages already written are unreachable
    /// orphans; the write pin and the gate are released.
    fn abort_write_locked(&self, shared: Option<&Shared>, state: &mut ConnState) {
        let Some(w) = state.write.take() else {
            return;
        };
        state.idle_deadline = None;
        if let Some(shared) = shared {
            if w.started {
                shared.page_service_abandon_commit();
            }
            shared.page_service_end_read(&mut state.pins, w.base);
        }
        self.gate.release();
    }

    /// End the write transaction at the client's request (README.md §3.5).
    fn abort(&self, shared: &Shared, conn: &ServerConn) -> Result<Option<Message>> {
        let mut state = conn.state.lock().expect("connection state lock poisoned");
        if state.write.is_some() {
            self.abort_write_locked(Some(shared), &mut state);
            return Ok(None);
        }
        if state.revoked.take().is_some() {
            return Ok(None);
        }
        Err(violation("abort without a write transaction"))
    }

    /// Check A, then write the manifest pages (README.md §3.5).
    fn commit_begin(
        &self,
        shared: &Shared,
        conn: &ServerConn,
        m: CommitBegin,
    ) -> Result<Option<Message>> {
        let mut state = conn.state.lock().expect("connection state lock poisoned");
        let revoked = state.revoked.is_some();
        let Some(w) = state.write.as_mut() else {
            if revoked {
                return Ok(None); // the revoked transaction's delta is discarded; commit_end reports 25P03
            }
            return Err(violation("commit_begin without a write transaction"));
        };
        if w.delta.is_some() || w.failure.is_some() {
            return Err(violation("commit_begin twice in one write transaction"));
        }
        let delta = match begin_delta(&w.check, m) {
            Ok(delta) => delta,
            Err(e) => {
                w.failure = Some(e);
                return Ok(None);
            }
        };
        let result =
            shared.page_service_write_delta(&mut w.started, delta.page_count, &delta.manifest);
        w.delta = Some(delta);
        if result.is_err() {
            self.stop_serving();
        }
        result.map(|()| None)
    }

    /// Check B on each page, then write them (README.md §3.5).
    fn commit_pages(
        &self,
        shared: &Shared,
        conn: &ServerConn,
        m: CommitPages,
    ) -> Result<Option<Message>> {
        let mut state = conn.state.lock().expect("connection state lock poisoned");
        let revoked = state.revoked.is_some();
        let Some(w) = state.write.as_mut() else {
            if revoked {
                return Ok(None);
            }
            return Err(violation("commit_pages without a write transaction"));
        };
        if w.failure.is_some() {
            return Ok(None);
        }
        let Some(delta) = w.delta.as_mut() else {
            return Err(violation("commit_pages before commit_begin"));
        };
        for page in &m.pages {
            if let Err(e) = delta.check_page(page) {
                w.failure = Some(e);
                return Ok(None);
            }
        }
        let result = shared.page_service_write_delta(&mut w.started, delta.page_count, &m.pages);
        if result.is_err() {
            self.stop_serving();
        }
        result.map(|()| None)
    }

    /// Check C; past it (the point of no return) the commit completes whatever the connection does
    /// (README.md §3.5).
    fn commit_end(
        &self,
        shared: &Shared,
        conn: &ServerConn,
        request_id: u32,
    ) -> Result<Option<Message>> {
        let mut state = conn.state.lock().expect("connection state lock poisoned");
        if state.write.is_none() {
            if state.revoked.take().is_some() {
                return Ok(Some(idle_revoked()));
            }
            return Err(violation("commit_end without a write transaction"));
        }
        let w = state.write.as_mut().expect("checked above");
        if w.failure.is_none() {
            match &w.delta {
                None => return Err(violation("commit_end before commit_begin")),
                Some(delta) => w.failure = delta.complete().err(),
            }
        }
        if let Some(failure) = w.failure.take() {
            self.abort_write_locked(Some(shared), &mut state);
            return Err(failure);
        }
        w.finishing = true;
        let base = w.base;
        let delta = w.delta.take().expect("checked above");
        state.idle_deadline = None;
        drop(state);
        self.finish_client_commit(shared, conn, request_id, base, delta)
    }

    /// Publish a validated delta (page-service.md §5.6): meta, the single durable barrier,
    /// structural adoption (page accounting, the invalidation and resolution windows), the
    /// acknowledgement, then — still holding the gate — the catalog adoption and a maintenance commit
    /// if one is due. A write or sync failure poisons the owner, which stops serving.
    fn finish_client_commit(
        &self,
        shared: &Shared,
        conn: &ServerConn,
        request_id: u32,
        base: u64,
        delta: Delta,
    ) -> Result<Option<Message>> {
        let commit = self.commit_lock.lock().expect("commit lock poisoned");
        if shared.page_service_commit_delta(&delta).is_err() {
            drop(commit);
            self.stop_serving();
            {
                let mut state = conn.state.lock().expect("connection state lock poisoned");
                state.write = None;
                shared.page_service_end_read(&mut state.pins, base);
            }
            self.gate.release();
            // The meta may be durable: the outcome is unknown, so the client sees a lost connection
            // and resolves it with commit_status (under the next owner) rather than an error frame.
            conn.hang_up();
            return Ok(None);
        }
        {
            let mut state = conn.state.lock().expect("connection state lock poisoned");
            shared.page_service_publish_commit(
                &mut state.pins,
                base,
                delta.txid,
                delta.meta,
                delta.written,
                &self.foreign,
            );
            state.write = None;
        }
        drop(commit);
        match self.next_ack.swap(ACK_NORMAL, Ordering::AcqRel) {
            ACK_HANG_UP => conn.hang_up(),
            ACK_CRASH => std::process::exit(70),
            _ => {
                conn.send(
                    request_id,
                    &Message::Committed(Committed { txid: delta.txid }),
                );
            }
        }
        if shared.adopt_foreign(self).is_ok() {
            shared.maintenance_commit(self);
        }
        self.gate.release();
        Ok(None)
    }
}

fn join_unless_current(handle: JoinHandle<()>) {
    if handle.thread().id() != thread::current().id() {
        let _ = handle.join();
    }
}

fn violation(message: impl Into<String>) -> EngineError {
    EngineError::new(SqlState::ProtocolViolation, message)
}

/// The `error` frame for `error`.
fn error_reply(error: &EngineError, flags: u8) -> Message {
    Message::Error(ErrorReply {
        sqlstate: error.code().to_string(),
        message: error.message.clone(),
        detail: String::new(),
        flags,
    })
}

fn read_only_reply(message: &str) -> Message {
    error_reply(
        &EngineError::new(SqlState::ReadOnlySqlTransaction, message),
        0,
    )
}

/// The non-fatal `25P03` a revoked write transaction's next write-path request reports.
fn idle_revoked() -> Message {
    error_reply(
        &EngineError::new(
            SqlState::IdleInTransactionSessionTimeout,
            "terminating the write transaction due to the idle-in-transaction timeout",
        ),
        0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(txid: u64) -> Vec<u8> {
        // Only the window's bookkeeping is under test; an unparsable meta reads as page count 0.
        txid.to_be_bytes().to_vec()
    }

    #[test]
    fn invalidation_lists_exactly_the_pages_written_since_the_cache() {
        let me = [1u8; 16];
        let mut w = ServiceWindow::new(me, 10, meta(10));
        w.published(11, meta(11), vec![9, 4, 4]);
        w.published(12, meta(12), vec![5, 4]);
        assert_eq!(
            w.invalidation(&me, 10, 12),
            (INVALIDATE_EXACT, vec![4, 5, 9])
        );
        assert_eq!(w.invalidation(&me, 11, 12), (INVALIDATE_EXACT, vec![4, 5]));
        assert_eq!(w.invalidation(&me, 12, 12), (INVALIDATE_EXACT, vec![]));
        // No cache, a foreign incarnation, a newer-than-pinned or pre-window txid: everything.
        assert_eq!(w.invalidation(&me, 0, 12).0, INVALIDATE_ALL);
        assert_eq!(w.invalidation(&[2; 16], 11, 12).0, INVALIDATE_ALL);
        assert_eq!(w.invalidation(&me, 13, 12).0, INVALIDATE_ALL);
        assert_eq!(w.invalidation(&me, 9, 12).0, INVALIDATE_ALL);
        // Compaction restarts the window: every older cache is invalid.
        w.reset(13, meta(13));
        assert_eq!(w.invalidation(&me, 12, 13).0, INVALIDATE_ALL);
        assert_eq!(w.invalidation(&me, 13, 13), (INVALIDATE_EXACT, vec![]));
    }

    #[test]
    fn the_window_keeps_the_last_256_commits() {
        let me = [1u8; 16];
        let mut w = ServiceWindow::new(me, 0, meta(0));
        for txid in 1..=300u64 {
            w.published(txid, meta(txid), vec![txid as u32]);
        }
        assert_eq!(w.invalidation(&me, 43, 300).0, INVALIDATE_ALL);
        let (flag, list) = w.invalidation(&me, 44, 300);
        assert_eq!(flag, INVALIDATE_EXACT);
        assert_eq!(list.len(), 256);
    }

    #[test]
    fn an_oversized_list_invalidates_everything() {
        let me = [1u8; 16];
        let mut w = ServiceWindow::new(me, 1, meta(1));
        w.published(2, meta(2), (0..=MAX_INVALIDATION_ENTRIES).collect());
        assert_eq!(w.invalidation(&me, 1, 2).0, INVALIDATE_ALL);
    }

    #[test]
    fn the_resolution_window_answers_commit_status() {
        let me = [1u8; 16];
        let mut w = ServiceWindow::new(me, 10, meta(10));
        w.published(11, meta(11), vec![]);
        w.published(12, meta(12), vec![]);
        let digest = |txid: u64| crate::format::commit_crc64(0, &meta(txid));
        assert_eq!(w.resolve(11, digest(11)), COMMIT_STATUS_COMMITTED);
        // Another commit took the txid.
        assert_eq!(w.resolve(11, digest(99)), COMMIT_STATUS_NOT_COMMITTED);
        // Above the published version.
        assert_eq!(w.resolve(13, digest(13)), COMMIT_STATUS_NOT_COMMITTED);
        // Below the window.
        assert_eq!(w.resolve(9, digest(9)), COMMIT_STATUS_UNKNOWN);
        // The window keeps the newest 256 commits.
        for txid in 13..13 + RESOLUTION_WINDOW_COMMITS as u64 {
            w.published(txid, meta(txid), vec![]);
        }
        assert_eq!(w.resolution.len(), RESOLUTION_WINDOW_COMMITS);
        assert_eq!(w.resolution.front().map(|r| r.0), Some(13));
        // A compaction joins it.
        w.reset(300, meta(300));
        assert_eq!(w.resolve(300, digest(300)), COMMIT_STATUS_COMMITTED);
    }
}
