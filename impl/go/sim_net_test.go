package jed

// The deterministic-simulation network (spec/design/simulation-testing.md §14 S8): the page service's
// owner record, endpoints, and stream connections between simulated processes, standing in for the
// bundle files and Unix sockets of the OS host. Connections are in-memory byte queues. The engine's
// owner and client still run their connection goroutines, so after every step the runner waits until
// the network is quiescent — every byte written has been read, every reader is blocked waiting for
// more (or has closed its end after handling end of stream), and every pending connection has been
// accepted — and the next step starts from one state in every core. A process that dies loses its
// listener and connections at once, as the kernel closes a dead process's sockets.

import (
	"encoding/hex"
	"errors"
	"io"
	"net"
	"sync"
	"time"
)

var errSimRefused = errors.New("simulated connection refused")

// simNet is the network all simulated processes share.
type simNet struct {
	mu        sync.Mutex
	cond      *sync.Cond
	records   map[string]ownerRecord
	listeners map[string]*simListener
	ends      map[*simConnEnd]bool
	// frames counts the frames written during the step; at disconnectAt (0: none) the connection
	// carrying that frame ends instead of delivering it (the armed `disconnect` fault).
	frames       int
	disconnectAt int
	disconnected bool
}

func newSimNet() *simNet {
	n := &simNet{records: map[string]ownerRecord{}, listeners: map[string]*simListener{}, ends: map[*simConnEnd]bool{}}
	n.cond = sync.NewCond(&n.mu)
	return n
}

// beginStep resets the step's frame count and arms a disconnect at frame nth (0: none).
func (n *simNet) beginStep(nth int) {
	n.mu.Lock()
	n.frames, n.disconnectAt, n.disconnected = 0, nth, false
	n.mu.Unlock()
}

// quiescent reports whether nothing is in flight. Caller holds mu.
func (n *simNet) quiescent() bool {
	for _, l := range n.listeners {
		if len(l.pending) > 0 {
			return false
		}
	}
	for e := range n.ends {
		if !e.closed && !(e.waiting && len(e.inbox) == 0) {
			return false
		}
	}
	return true
}

// settle waits until the network is quiescent.
func (n *simNet) settle() {
	n.mu.Lock()
	for !n.quiescent() {
		n.cond.Wait()
	}
	// Forget ends both sides have closed.
	for e := range n.ends {
		if e.closed && e.peer.closed {
			delete(n.ends, e)
		}
	}
	n.mu.Unlock()
}

// kill closes every listener and connection of process pid.
func (n *simNet) kill(pid int) {
	n.mu.Lock()
	for _, l := range n.listeners {
		if l.pid == pid {
			l.closeLocked()
		}
	}
	for e := range n.ends {
		if e.pid == pid {
			e.closeLocked()
		}
	}
	n.cond.Broadcast()
	n.mu.Unlock()
}

// simListener is a simulated endpoint.
type simListener struct {
	net      *simNet
	endpoint string
	pid      int
	pending  []*simConnEnd
	closed   bool
}

func (l *simListener) Accept() (net.Conn, error) {
	n := l.net
	n.mu.Lock()
	defer n.mu.Unlock()
	for len(l.pending) == 0 && !l.closed {
		n.cond.Wait()
	}
	if l.closed {
		return nil, net.ErrClosed
	}
	e := l.pending[0]
	l.pending = l.pending[1:]
	n.cond.Broadcast()
	return e, nil
}

func (l *simListener) closeLocked() {
	if l.closed {
		return
	}
	l.closed = true
	for _, e := range l.pending {
		e.closeLocked()
	}
	l.pending = nil
	if l.net.listeners[l.endpoint] == l {
		delete(l.net.listeners, l.endpoint)
	}
}

func (l *simListener) Close() error {
	l.net.mu.Lock()
	l.closeLocked()
	l.net.cond.Broadcast()
	l.net.mu.Unlock()
	return nil
}

func (l *simListener) Addr() net.Addr { return simAddr(l.endpoint) }

// simConnEnd is one side of a simulated stream connection. waiting is set while its reader is
// blocked in Read with nothing to read; closed once this side closed its end.
type simConnEnd struct {
	net     *simNet
	peer    *simConnEnd
	pid     int
	inbox   []byte
	eof     bool
	closed  bool
	waiting bool
}

func (e *simConnEnd) Read(p []byte) (int, error) {
	n := e.net
	n.mu.Lock()
	defer n.mu.Unlock()
	for {
		if len(e.inbox) > 0 {
			k := copy(p, e.inbox)
			e.inbox = e.inbox[k:]
			e.waiting = false
			return k, nil
		}
		if e.eof || e.closed {
			e.waiting = false
			return 0, io.EOF
		}
		e.waiting = true
		n.cond.Broadcast()
		n.cond.Wait()
	}
}

func (e *simConnEnd) Write(p []byte) (int, error) {
	n := e.net
	n.mu.Lock()
	defer n.mu.Unlock()
	if e.closed || e.eof {
		return 0, net.ErrClosed
	}
	n.frames++
	if n.disconnectAt > 0 && n.frames == n.disconnectAt {
		n.disconnected = true
		e.dropLocked()
		n.cond.Broadcast()
		return 0, net.ErrClosed
	}
	e.peer.inbox = append(e.peer.inbox, p...)
	e.peer.waiting = false
	n.cond.Broadcast()
	// A write returns once the peer has read it and is waiting for more (or its end is gone), so every
	// frame of a step is handled before the writer goes on: the order of a connection's frames and of
	// their effects is the same in every run.
	for !(e.peer.waiting && len(e.peer.inbox) == 0) && !e.peer.closed && !e.closed {
		n.cond.Wait()
	}
	return len(p), nil
}

// dropLocked ends the connection without closing either end: both readers see end of stream and
// close their own ends after handling it, so settle waits for both (a lost connection).
func (e *simConnEnd) dropLocked() {
	for _, end := range []*simConnEnd{e, e.peer} {
		end.eof = true
		end.waiting = false
	}
}

// drop ends the connection as the `disconnect` step does.
func (e *simConnEnd) drop() {
	e.net.mu.Lock()
	e.dropLocked()
	e.net.cond.Broadcast()
	e.net.mu.Unlock()
}

// closeLocked ends this side: its peer reads end of stream once its inbox drains.
func (e *simConnEnd) closeLocked() {
	if !e.closed {
		e.closed = true
		e.peer.eof = true
		e.peer.waiting = false
	}
}

func (e *simConnEnd) Close() error {
	e.net.mu.Lock()
	e.closeLocked()
	e.net.cond.Broadcast()
	e.net.mu.Unlock()
	return nil
}

func (e *simConnEnd) LocalAddr() net.Addr              { return simAddr("sim") }
func (e *simConnEnd) RemoteAddr() net.Addr             { return simAddr("sim") }
func (e *simConnEnd) SetDeadline(time.Time) error      { return nil }
func (e *simConnEnd) SetReadDeadline(time.Time) error  { return nil }
func (e *simConnEnd) SetWriteDeadline(time.Time) error { return nil }

type simAddr string

func (a simAddr) Network() string { return "sim" }
func (a simAddr) String() string  { return string(a) }

// --- the page-service operations of simLockHost (coordinator_host.go) ----------------------------

func (h *simLockHost) readOwnerRecord(dbPath string) (ownerRecord, bool) {
	h.net.mu.Lock()
	defer h.net.mu.Unlock()
	r, ok := h.net.records[dbPath]
	return r, ok
}

func (h *simLockHost) writeOwnerRecord(dbPath string, r ownerRecord) error {
	h.net.mu.Lock()
	h.net.records[dbPath] = r
	h.net.mu.Unlock()
	return nil
}

func (h *simLockHost) removeOwnerRecord(dbPath string) {
	h.net.mu.Lock()
	delete(h.net.records, dbPath)
	h.net.mu.Unlock()
}

func (h *simLockHost) endpointPath(_ string, _ uint32, incarnation id16) string {
	return "sim-endpoint-" + hex.EncodeToString(incarnation[:8])
}

func (h *simLockHost) listen(endpoint string) (net.Listener, error) {
	n := h.net
	n.mu.Lock()
	defer n.mu.Unlock()
	if old := n.listeners[endpoint]; old != nil {
		old.closeLocked()
	}
	l := &simListener{net: n, endpoint: endpoint, pid: h.pidNum}
	n.listeners[endpoint] = l
	return l, nil
}

func (h *simLockHost) dial(endpoint string) (net.Conn, error) {
	n := h.net
	n.mu.Lock()
	defer n.mu.Unlock()
	l := n.listeners[endpoint]
	if l == nil || l.closed {
		return nil, errSimRefused
	}
	client := &simConnEnd{net: n, pid: h.pidNum}
	server := &simConnEnd{net: n, pid: l.pid}
	client.peer, server.peer = server, client
	n.ends[client], n.ends[server] = true, true
	l.pending = append(l.pending, server)
	n.cond.Broadcast()
	return client, nil
}

func (h *simLockHost) removeEndpoint(endpoint string) {
	n := h.net
	n.mu.Lock()
	if l := n.listeners[endpoint]; l != nil {
		l.closeLocked()
	}
	n.cond.Broadcast()
	n.mu.Unlock()
}
