//! The page-service owner (spec/design/page-service.md §4.1, §5; spec/pageservice/README.md §6.2):
//! the presence-EX holder serves pages of its database to page clients over its endpoint. Each
//! connection runs on its own thread, so a client request never blocks the owner's own sessions. A
//! connection's pins join the owner's reader registry, so the reclamation watermark covers every
//! process (transactions.md §8). Phase 2: read path only — every connection is read-only.
//!
//! The registry-side state — the published meta and the invalidation window ([`ServiceWindow`]) —
//! lives in the shared core's reader registry, under the lock that orders reader pins against
//! publication, so a `begin_read` always sees a consistent (snapshot, meta, window) triple.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::File;
use std::io::Write;
use std::net::Shutdown;
use std::os::unix::fs::FileExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::thread::{self, JoinHandle};

use crate::error::{EngineError, Result, SqlState};
use crate::pageservice::codec::{
    ERROR_FLAG_FATAL, ErrorReply, HELLO_OK_FLAG_CONNECTION_READ_ONLY,
    HELLO_OK_FLAG_OWNER_READ_ONLY, Hello, HelloOk, INVALIDATE_ALL, INVALIDATE_EXACT,
    INVALIDATION_WINDOW_COMMITS, Id16, MAX_INVALIDATION_ENTRIES, MAX_READ_PAGES_CAP, Message,
    OwnerRecord, PROTOCOL_VERSION, Pages, encode_frame, max_read_pages, read_frame,
};
use crate::pageservice::endpoint;
use crate::shared::Shared;

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

/// Read the meta slot that published `txid` through `paging`. Every commit writes slot `txid & 1`,
/// so it is read first; the parse check guards a slot a later commit already overwrote.
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

/// The owner's published meta and invalidation window (README.md §4). Lives in the reader registry
/// and changes only together with the published root.
pub(crate) struct ServiceWindow {
    incarnation: Id16,
    meta: Vec<u8>,
    page_count: u32,
    /// The written page indices of the most recent commits, oldest first, each sorted.
    window: VecDeque<(u64, Vec<u32>)>,
    /// Every commit after this txid is in `window`.
    window_start: u64,
}

impl ServiceWindow {
    /// The window of an owner whose committed snapshot is `txid` with meta page `meta`.
    pub(crate) fn new(incarnation: Id16, txid: u64, meta: Vec<u8>) -> ServiceWindow {
        let page_count = crate::format::meta_txid_and_page_count(&meta).map_or(0, |(_, n)| n);
        ServiceWindow {
            incarnation,
            meta,
            page_count,
            window: VecDeque::new(),
            window_start: txid,
        }
    }

    pub(crate) fn incarnation(&self) -> Id16 {
        self.incarnation
    }

    pub(crate) fn meta(&self) -> &[u8] {
        &self.meta
    }

    /// The current snapshot's page count. Page counts only grow between compactions and a pin
    /// blocks compaction, so it bounds every page of every pinned snapshot.
    pub(crate) fn page_count(&self) -> u32 {
        self.page_count
    }

    /// Record a main-database commit: its meta page and the indices it wrote (body, free-list, and
    /// manifest pages; never the meta slots).
    pub(crate) fn published(&mut self, txid: u64, meta: Vec<u8>, mut written: Vec<u32>) {
        self.page_count = crate::format::meta_txid_and_page_count(&meta).map_or(0, |(_, n)| n);
        self.meta = meta;
        written.sort_unstable();
        written.dedup();
        self.window.push_back((txid, written));
        while self.window.len() > INVALIDATION_WINDOW_COMMITS as usize {
            let (dropped, _) = self.window.pop_front().expect("window is non-empty");
            self.window_start = dropped;
        }
    }

    /// Restart the window at a compacted snapshot: every older cache is invalid.
    pub(crate) fn reset(&mut self, txid: u64, meta: Vec<u8>) {
        *self = ServiceWindow::new(self.incarnation, txid, meta);
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
}

/// One connection's pins, counted per txid; each is also one count in the reader registry.
pub(crate) type ConnPins = BTreeMap<u64, usize>;

struct Conns {
    closed: bool,
    next_id: u64,
    /// A second handle on each open connection, so close can shut it down.
    open: HashMap<u64, UnixStream>,
    threads: Vec<JoinHandle<()>>,
}

/// The serving state of an owner: its read handle, endpoint, and connections. Shut down explicitly
/// ([`shutdown`](Self::shutdown)) before the owner releases presence.
pub(crate) struct PageServiceOwner {
    shared: Weak<Shared>,
    db_path: PathBuf,
    page_size: u32,
    read_only: bool,
    /// The owner's own read handle on the database: page reads are positioned reads on it,
    /// independent of the pager's lock, so a client never waits on the owner's commit. Compaction
    /// replaces it.
    file: RwLock<File>,
    endpoint: Option<PathBuf>,
    conns: Mutex<Conns>,
    accept: Mutex<Option<JoinHandle<()>>>,
}

impl PageServiceOwner {
    /// Serve the database at `db_path` for `shared`, whose registry already holds the window for
    /// `incarnation`: write the owner record, bind the endpoint, and start accepting. An owner that
    /// cannot bind (no private endpoint directory) still owns and records `flags = 0`.
    pub(crate) fn start(
        shared: &Arc<Shared>,
        db_path: &Path,
        incarnation: Id16,
        page_size: u32,
        read_only: bool,
    ) -> Result<Arc<PageServiceOwner>> {
        let file = File::open(db_path).map_err(|e| {
            EngineError::new(
                SqlState::IoError,
                format!("open database for page service: {e}"),
            )
        })?;
        let uid = endpoint::current_uid(&endpoint::bundle_dir(db_path))?;
        let endpoint_path = endpoint::endpoint_path(db_path, uid, &incarnation);
        let record = OwnerRecord {
            serving: endpoint_path.is_some(),
            uid,
            incarnation,
            pid: std::process::id(),
            endpoint: endpoint_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        endpoint::write_owner_record(db_path, &record)?;
        let owner = Arc::new(PageServiceOwner {
            shared: Arc::downgrade(shared),
            db_path: db_path.to_path_buf(),
            page_size,
            read_only,
            file: RwLock::new(file),
            endpoint: endpoint_path.clone(),
            conns: Mutex::new(Conns {
                closed: false,
                next_id: 0,
                open: HashMap::new(),
                threads: Vec::new(),
            }),
            accept: Mutex::new(None),
        });
        let Some(path) = endpoint_path else {
            return Ok(owner);
        };
        // Only a presence-EX holder reaches here, so a file already at the path is a dead owner's.
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).map_err(|e| {
            let _ = std::fs::remove_file(endpoint::owner_record_path(db_path));
            EngineError::new(
                SqlState::IoError,
                format!("bind page-service endpoint: {e}"),
            )
        })?;
        let acceptor = Arc::clone(&owner);
        *owner.accept.lock().expect("accept lock poisoned") =
            Some(thread::spawn(move || acceptor.accept_loop(listener)));
        Ok(owner)
    }

    fn accept_loop(self: Arc<Self>, listener: UnixListener) {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                continue;
            };
            let mut conns = self.conns.lock().expect("connections lock poisoned");
            if conns.closed {
                return;
            }
            let Ok(control) = stream.try_clone() else {
                continue;
            };
            conns.next_id += 1;
            let id = conns.next_id;
            conns.open.insert(id, control);
            conns.threads.retain(|t| !t.is_finished());
            let owner = Arc::clone(&self);
            conns
                .threads
                .push(thread::spawn(move || owner.serve(id, stream)));
        }
    }

    /// Reopen the read handle after compaction replaced the file. The caller holds the writer gate
    /// and no pin exists, so no page read is in flight.
    pub(crate) fn reopen_file(&self) -> Result<()> {
        let file = File::open(&self.db_path).map_err(|e| {
            EngineError::new(
                SqlState::IoError,
                format!("reopen database for page service: {e}"),
            )
        })?;
        *self.file.write().expect("page-service file lock poisoned") = file;
        Ok(())
    }

    /// Stop serving: no new connections, every client sees EOF (releasing its pins), and the
    /// endpoint and owner record are removed. The caller still holds presence EX.
    pub(crate) fn shutdown(&self) {
        let (streams, threads) = {
            let mut conns = self.conns.lock().expect("connections lock poisoned");
            conns.closed = true;
            (
                conns.open.drain().map(|(_, s)| s).collect::<Vec<_>>(),
                std::mem::take(&mut conns.threads),
            )
        };
        let accept = self.accept.lock().expect("accept lock poisoned").take();
        if let (Some(accept), Some(path)) = (accept, &self.endpoint) {
            // Wake the blocking accept; it sees `closed` and returns.
            let _ = UnixStream::connect(path);
            join_unless_current(accept);
        }
        for stream in streams {
            let _ = stream.shutdown(Shutdown::Both);
        }
        for thread in threads {
            join_unless_current(thread);
        }
        if let Some(path) = &self.endpoint {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_file(endpoint::owner_record_path(&self.db_path));
    }

    /// Run one connection until EOF or a protocol violation, then release its pins.
    fn serve(self: Arc<Self>, id: u64, mut stream: UnixStream) {
        let mut pins = ConnPins::new();
        self.serve_frames(id, &mut stream, &mut pins);
        let _ = stream.shutdown(Shutdown::Both);
        if let Some(shared) = self.shared.upgrade() {
            shared.page_service_release_pins(&mut pins);
        }
        self.conns
            .lock()
            .expect("connections lock poisoned")
            .open
            .remove(&id);
    }

    fn serve_frames(&self, id: u64, stream: &mut UnixStream, pins: &mut ConnPins) {
        let mut hello_done = false;
        loop {
            let (request_id, message) = match read_frame(stream, self.page_size) {
                Ok(Some(frame)) => frame,
                Ok(None) => return,
                Err(e) => {
                    if e.state != SqlState::ConnectionFailure {
                        fail(stream, 0, &e);
                    }
                    return;
                }
            };
            if !hello_done {
                let Message::Hello(hello) = message else {
                    fail(
                        stream,
                        request_id,
                        &violation("the first frame must be hello"),
                    );
                    return;
                };
                if !self.hello(stream, request_id, id, &hello) {
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
                    match shared.page_service_begin_read(pins, &m.incarnation, m.last_txid) {
                        Some(begun) => Ok(Some(Message::ReadBegun(begun))),
                        None => return,
                    }
                }
                Message::ReadPages(m) => self.read_pages(&shared, pins, m.txid, &m.indices),
                Message::EndRead(m) => {
                    if shared.page_service_end_read(pins, m.txid) {
                        Ok(None)
                    } else {
                        Err(violation(
                            "end_read names a txid this connection has not pinned",
                        ))
                    }
                }
                Message::BeginWrite(_) => Ok(Some(Message::Error(ErrorReply {
                    sqlstate: "25006".into(),
                    message: "cannot write through a read-only page-service connection".into(),
                    detail: String::new(),
                    flags: 0,
                }))),
                other => Err(violation(format!("unexpected {} message", other.name()))),
            };
            drop(shared);
            match reply {
                Ok(Some(reply)) => {
                    if !send(stream, request_id, &reply) {
                        return;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    fail(stream, request_id, &e);
                    return;
                }
            }
        }
    }

    /// Negotiate the protocol and the format (README.md §3.1). False closes the connection.
    fn hello(&self, stream: &mut UnixStream, request_id: u32, id: u64, m: &Hello) -> bool {
        if m.protocol_min > PROTOCOL_VERSION || m.protocol_max < PROTOCOL_VERSION {
            fail(
                stream,
                request_id,
                &EngineError::new(
                    SqlState::FeatureNotSupported,
                    "unsupported page-service protocol; the owner speaks version 1",
                ),
            );
            return false;
        }
        if m.format_version != crate::format::FORMAT_VERSION {
            fail(
                stream,
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
        // Phase 2: every connection is read-only.
        let mut flags = HELLO_OK_FLAG_CONNECTION_READ_ONLY;
        if self.read_only {
            flags |= HELLO_OK_FLAG_OWNER_READ_ONLY;
        }
        send(
            stream,
            request_id,
            &Message::HelloOk(HelloOk {
                protocol: PROTOCOL_VERSION,
                format_version: crate::format::FORMAT_VERSION,
                page_size: self.page_size,
                connection_id: id,
                flags,
                max_read_pages: max_read_pages(self.page_size),
            }),
        )
    }

    /// The requested pages of a snapshot this connection pins (README.md §3.3).
    fn read_pages(
        &self,
        shared: &Shared,
        pins: &ConnPins,
        txid: u64,
        indices: &[u32],
    ) -> Result<Option<Message>> {
        let Some(page_count) = shared.page_service_page_count(pins, txid) else {
            return Err(violation(
                "read_pages names a txid this connection has not pinned",
            ));
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
            file.read_exact_at(&mut page, u64::from(index) * u64::from(self.page_size))
                .map_err(|e| {
                    EngineError::new(SqlState::IoError, format!("page service read: {e}"))
                })?;
            pages.push(page);
        }
        Ok(Some(Message::Pages(Pages { pages })))
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

fn send(stream: &mut UnixStream, request_id: u32, message: &Message) -> bool {
    match encode_frame(request_id, message) {
        Ok(frame) => stream.write_all(&frame).is_ok(),
        Err(_) => false,
    }
}

/// Answer a request with a fatal error; the caller then closes the connection.
fn fail(stream: &mut UnixStream, request_id: u32, error: &EngineError) {
    send(
        stream,
        request_id,
        &Message::Error(ErrorReply {
            sqlstate: error.code().to_string(),
            message: error.message.clone(),
            detail: String::new(),
            flags: ERROR_FLAG_FATAL,
        }),
    );
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
}
