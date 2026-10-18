//! The deterministic-simulation network (spec/design/simulation-testing.md §14 S8): the page
//! service's owner record, endpoints, and stream connections between simulated processes, standing
//! in for the bundle files and Unix sockets of the OS host. A port of the Go reference
//! (`impl/go/sim_net_test.go`).
//!
//! Connections are in-memory byte queues. The engine's owner and client still run their connection
//! threads, so after every step the runner waits until the network is quiescent — every byte written
//! has been read, every reader is blocked waiting for more (or has closed its end after handling end
//! of stream), and every pending connection has been accepted — and the next step starts from one
//! state in every core. A process that dies loses its listener and connections at once, as the
//! kernel closes a dead process's sockets.

use crate::pageservice::codec::OwnerRecord;
use crate::pageservice::net::{PsListener, PsStream};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How long a settle may wait in real time before the run is reported stuck (a simulator or engine
/// bug: some thread neither reads nor closes its end).
const SETTLE_LIMIT: Duration = Duration::from_secs(120);

/// One side of a simulated stream connection. `waiting` is set while its reader is blocked in a read
/// with nothing to read; `closed` once this side closed its end; `eof` once its peer did.
struct End {
    peer: usize,
    pid: u32,
    inbox: VecDeque<u8>,
    eof: bool,
    closed: bool,
    waiting: bool,
    /// Open handles on this end: the last one dropped closes it, as the last close of a socket does.
    handles: usize,
}

/// A simulated endpoint.
struct Listener {
    endpoint: PathBuf,
    pid: u32,
    pending: VecDeque<usize>,
    closed: bool,
}

struct NetState {
    records: HashMap<PathBuf, OwnerRecord>,
    /// Endpoint → the listener bound there.
    bound: HashMap<PathBuf, usize>,
    listeners: HashMap<usize, Listener>,
    ends: HashMap<usize, End>,
    next_id: usize,
    /// The frames written during the step; at `disconnect_at` (0: none) the connection carrying that
    /// frame ends instead of delivering it (the armed `disconnect` fault).
    frames: i64,
    disconnect_at: i64,
    disconnected: bool,
}

impl NetState {
    /// End `id`: its peer reads end of stream once its inbox drains.
    fn close_end(&mut self, id: usize) {
        let Some(e) = self.ends.get_mut(&id) else {
            return;
        };
        if e.closed {
            return;
        }
        e.closed = true;
        let peer = e.peer;
        if let Some(p) = self.ends.get_mut(&peer) {
            p.eof = true;
            p.waiting = false;
        }
    }

    /// End the connection of `id` without closing either end: both sides read end of stream once
    /// their inboxes drain, and each closes its own end after handling it — so a settle waits for
    /// both sides' handling, which closing an end on a side's behalf would not.
    fn sever(&mut self, id: usize) {
        let Some(e) = self.ends.get_mut(&id) else {
            return;
        };
        e.eof = true;
        e.waiting = false;
        let peer = e.peer;
        if let Some(p) = self.ends.get_mut(&peer) {
            p.eof = true;
            p.waiting = false;
        }
    }

    fn close_listener(&mut self, id: usize) {
        let Some(l) = self.listeners.get_mut(&id) else {
            return;
        };
        if l.closed {
            return;
        }
        l.closed = true;
        let pending: Vec<usize> = l.pending.drain(..).collect();
        let endpoint = l.endpoint.clone();
        for e in pending {
            self.close_end(e);
        }
        if self.bound.get(&endpoint) == Some(&id) {
            self.bound.remove(&endpoint);
        }
    }

    /// Whether nothing is in flight.
    fn quiescent(&self) -> bool {
        self.listeners.values().all(|l| l.pending.is_empty())
            && self
                .ends
                .values()
                .all(|e| e.closed || (e.waiting && e.inbox.is_empty()))
    }

    fn new_id(&mut self) -> usize {
        self.next_id += 1;
        self.next_id
    }
}

/// The network all simulated processes share.
pub(crate) struct SimNet {
    state: Mutex<NetState>,
    cond: Condvar,
}

impl SimNet {
    pub(crate) fn new() -> Arc<SimNet> {
        Arc::new(SimNet {
            state: Mutex::new(NetState {
                records: HashMap::new(),
                bound: HashMap::new(),
                listeners: HashMap::new(),
                ends: HashMap::new(),
                next_id: 0,
                frames: 0,
                disconnect_at: 0,
                disconnected: false,
            }),
            cond: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, NetState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reset the step's frame count and arm a disconnect at frame `nth` (0: none).
    pub(crate) fn begin_step(&self, nth: i64) {
        let mut st = self.lock();
        st.frames = 0;
        st.disconnect_at = nth;
        st.disconnected = false;
    }

    /// Whether the armed disconnect fired at its frame during the step.
    pub(crate) fn disconnected(&self) -> bool {
        self.lock().disconnected
    }

    /// Wait until the network is quiescent, then forget the ends both sides have closed.
    pub(crate) fn settle(&self) {
        let start = Instant::now();
        let mut st = self.lock();
        while !st.quiescent() {
            let (next, timeout) = self
                .cond
                .wait_timeout(st, Duration::from_millis(100))
                .unwrap_or_else(|e| e.into_inner());
            st = next;
            if timeout.timed_out() && start.elapsed() > SETTLE_LIMIT && !st.quiescent() {
                let busy: Vec<String> = st
                    .ends
                    .iter()
                    .filter(|(_, e)| !(e.closed || (e.waiting && e.inbox.is_empty())))
                    .map(|(id, e)| {
                        format!(
                            "end {id} (p{}, inbox {}, eof {}, waiting {})",
                            e.pid,
                            e.inbox.len(),
                            e.eof,
                            e.waiting
                        )
                    })
                    .collect();
                drop(st);
                panic!("simulated network never settled: {}", busy.join(", "));
            }
        }
        let gone: Vec<usize> = st
            .ends
            .iter()
            .filter(|(_, e)| e.closed && st.ends.get(&e.peer).is_none_or(|p| p.closed))
            .map(|(id, _)| *id)
            .collect();
        for id in gone {
            st.ends.remove(&id);
        }
    }

    /// Close every listener and connection of process `pid`.
    pub(crate) fn kill(&self, pid: u32) {
        let mut st = self.lock();
        let listeners: Vec<usize> = st
            .listeners
            .iter()
            .filter(|(_, l)| l.pid == pid)
            .map(|(id, _)| *id)
            .collect();
        for id in listeners {
            st.close_listener(id);
        }
        let ends: Vec<usize> = st
            .ends
            .iter()
            .filter(|(_, e)| e.pid == pid)
            .map(|(id, _)| *id)
            .collect();
        for id in ends {
            st.close_end(id);
        }
        self.cond.notify_all();
    }

    /// End every connection of process `pid` (the `disconnect` step): both ends see end of stream.
    pub(crate) fn disconnect(&self, pid: u32) {
        let mut st = self.lock();
        let ends: Vec<usize> = st
            .ends
            .iter()
            .filter(|(_, e)| e.pid == pid && !e.closed)
            .map(|(id, _)| *id)
            .collect();
        for id in ends {
            st.sever(id);
        }
        self.cond.notify_all();
    }

    // --- the page-service operations of the simulated lock host ---------------------------------

    pub(crate) fn read_owner_record(&self, db_path: &Path) -> Option<OwnerRecord> {
        self.lock().records.get(db_path).cloned()
    }

    pub(crate) fn write_owner_record(&self, db_path: &Path, record: &OwnerRecord) {
        self.lock()
            .records
            .insert(db_path.to_path_buf(), record.clone());
    }

    pub(crate) fn remove_owner_record(&self, db_path: &Path) {
        self.lock().records.remove(db_path);
    }

    pub(crate) fn listen(self: &Arc<Self>, endpoint: &Path, pid: u32) -> Box<dyn PsListener> {
        let mut st = self.lock();
        if let Some(old) = st.bound.get(endpoint).copied() {
            st.close_listener(old);
        }
        let id = st.new_id();
        st.listeners.insert(
            id,
            Listener {
                endpoint: endpoint.to_path_buf(),
                pid,
                pending: VecDeque::new(),
                closed: false,
            },
        );
        st.bound.insert(endpoint.to_path_buf(), id);
        self.cond.notify_all();
        Box::new(SimListener {
            net: Arc::clone(self),
            id,
        })
    }

    pub(crate) fn dial(
        self: &Arc<Self>,
        endpoint: &str,
        pid: u32,
    ) -> std::io::Result<Box<dyn PsStream>> {
        let mut st = self.lock();
        let lid = match st.bound.get(Path::new(endpoint)) {
            Some(id) if !st.listeners[id].closed => *id,
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionRefused,
                    "simulated connection refused",
                ));
            }
        };
        let server_pid = st.listeners[&lid].pid;
        let client = st.new_id();
        let server = st.new_id();
        let end = |peer, pid, handles| End {
            peer,
            pid,
            inbox: VecDeque::new(),
            eof: false,
            closed: false,
            waiting: false,
            handles,
        };
        st.ends.insert(client, end(server, pid, 1));
        st.ends.insert(server, end(client, server_pid, 0));
        st.listeners
            .get_mut(&lid)
            .expect("checked above")
            .pending
            .push_back(server);
        self.cond.notify_all();
        Ok(Box::new(SimStream {
            net: Arc::clone(self),
            id: client,
        }))
    }

    pub(crate) fn remove_endpoint(&self, endpoint: &Path) {
        let mut st = self.lock();
        if let Some(id) = st.bound.get(endpoint).copied() {
            st.close_listener(id);
        }
        self.cond.notify_all();
    }
}

/// A handle on a simulated endpoint.
struct SimListener {
    net: Arc<SimNet>,
    id: usize,
}

impl PsListener for SimListener {
    fn accept(&self) -> std::io::Result<Option<Box<dyn PsStream>>> {
        let net = &self.net;
        let mut st = net.lock();
        loop {
            let l = st
                .listeners
                .get_mut(&self.id)
                .expect("a listener is never forgotten");
            if l.closed {
                return Ok(None);
            }
            if let Some(id) = l.pending.pop_front() {
                if let Some(e) = st.ends.get_mut(&id) {
                    e.handles += 1;
                }
                net.cond.notify_all();
                return Ok(Some(Box::new(SimStream {
                    net: Arc::clone(net),
                    id,
                })));
            }
            st = net.cond.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn close(&self) {
        let mut st = self.net.lock();
        st.close_listener(self.id);
        self.net.cond.notify_all();
    }
}

/// Dropping the handle closes the endpoint, as the last close of a listening socket does: a later
/// dial is refused rather than queued where no accept will ever take it (and its writer would wait
/// for a reader forever).
impl Drop for SimListener {
    fn drop(&mut self) {
        let mut st = self.net.lock();
        st.close_listener(self.id);
        self.net.cond.notify_all();
    }
}

/// A handle on one end of a simulated connection.
struct SimStream {
    net: Arc<SimNet>,
    id: usize,
}

impl std::io::Read for SimStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let net = &self.net;
        let mut st = net.lock();
        loop {
            let Some(e) = st.ends.get_mut(&self.id) else {
                return Ok(0);
            };
            if !e.inbox.is_empty() {
                let k = buf.len().min(e.inbox.len());
                for (slot, byte) in buf.iter_mut().zip(e.inbox.drain(..k)) {
                    *slot = byte;
                }
                e.waiting = false;
                return Ok(k);
            }
            if e.eof || e.closed {
                e.waiting = false;
                return Ok(0);
            }
            e.waiting = true;
            net.cond.notify_all();
            st = net.cond.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }
}

fn closed_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "simulated connection closed",
    )
}

impl std::io::Write for SimStream {
    /// One write is one frame: it is delivered whole, or — at the armed disconnect's frame — ends the
    /// connection instead (not delivered; both ends read end of stream; the write fails).
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let net = &self.net;
        let mut st = net.lock();
        let peer = match st.ends.get(&self.id) {
            Some(e) if !e.closed && !e.eof => e.peer,
            _ => return Err(closed_error()),
        };
        st.frames += 1;
        if st.disconnect_at > 0 && st.frames == st.disconnect_at {
            st.disconnected = true;
            st.sever(self.id);
            net.cond.notify_all();
            return Err(closed_error());
        }
        if let Some(p) = st.ends.get_mut(&peer) {
            p.inbox.extend(buf.iter().copied());
            p.waiting = false;
        }
        net.cond.notify_all();
        // A write returns once the peer has read it and is waiting for more (or either end is
        // closed), so every frame of a step is handled before the writer goes on: the order of a
        // connection's frames and of their effects is the same in every run.
        loop {
            let peer_done = st
                .ends
                .get(&peer)
                .is_none_or(|p| p.closed || (p.waiting && p.inbox.is_empty()));
            let own_closed = st.ends.get(&self.id).is_none_or(|e| e.closed);
            if peer_done || own_closed {
                return Ok(buf.len());
            }
            st = net.cond.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl PsStream for SimStream {
    fn clone_stream(&self) -> std::io::Result<Box<dyn PsStream>> {
        let mut st = self.net.lock();
        if let Some(e) = st.ends.get_mut(&self.id) {
            e.handles += 1;
        }
        Ok(Box::new(SimStream {
            net: Arc::clone(&self.net),
            id: self.id,
        }))
    }

    fn close_both(&self) {
        let mut st = self.net.lock();
        st.close_end(self.id);
        self.net.cond.notify_all();
    }
}

impl Drop for SimStream {
    fn drop(&mut self) {
        let mut st = self.net.lock();
        let last = match st.ends.get_mut(&self.id) {
            Some(e) => {
                e.handles = e.handles.saturating_sub(1);
                e.handles == 0
            }
            None => false,
        };
        if last {
            st.close_end(self.id);
            self.net.cond.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The last handle on an endpoint closes it: a later dial is refused, not queued on a listener
    /// no accept will ever drain (where a hello's write would wait for a reader forever).
    #[test]
    fn a_dropped_listener_refuses_dials() {
        let net = SimNet::new();
        let listener = net.listen(Path::new("sim-endpoint-test"), 0);
        let queued = net
            .dial("sim-endpoint-test", 1)
            .expect("a bound endpoint accepts dials");
        drop(listener);
        let refused = net.dial("sim-endpoint-test", 1).err().expect("refused");
        assert_eq!(refused.kind(), std::io::ErrorKind::ConnectionRefused);
        // The connection queued before the close ended with it.
        drop(queued);
        net.settle();
    }
}
