package jed

// The page-service owner (spec/design/page-service.md §4.1, §5; spec/pageservice/README.md): the
// presence-EX holder serves pages of its database to page clients over its endpoint and commits their
// CommitDeltas. Each connection runs on its own goroutine, so a client request never blocks the owner's
// own sessions. A client's pins join the owner's reader registry (core.live), so the reclamation
// watermark covers every process (transactions.md §8).
//
// The writer gate (page-service.md §5.5) is gate, shared by the owner's own writers (acquireWriter,
// after the in-process writeMu) and client write transactions (begin_write). A client commit is
// published structurally — meta, page accounting, invalidation window, resolution window — and the
// owner's own sessions adopt its catalog at their next transaction begin (adoptForeign), so nothing a
// client commits can fail after it is durable (page-service.md §7).
//
// Handoff (README.md §6.5): an owner whose preference is yield, or whose own handle is read-only,
// answers request_ownership by draining and releasing ownership (pageservice_handoff.go); a graceful
// close drains its clients first. While draining, client begins are parked unanswered and the owner's
// own begins wait (waitAdmission).

import (
	"crypto/rand"
	"errors"
	"io"
	"net"
	"os"
	"sort"
	"strconv"
	"sync"
	"sync/atomic"
	"time"
)

// psWindowEntry is one commit's written page indices (README.md §4).
type psWindowEntry struct {
	txid    uint64
	indices []uint32
}

// psResolution is one published commit's (txid, digest) pair, for commit_status (README.md §4).
type psResolution struct {
	txid   uint64
	digest uint64
}

// psResolutionWindowCommits bounds the resolution window, like the invalidation window.
const psResolutionWindowCommits = psInvalidationWindowCommits

// defaultIdleInTransactionTimeout is the owner's idle-in-transaction timeout for client write
// transactions (page-service.md §11.1).
const defaultIdleInTransactionTimeoutMs = 60000

// ownerState is an owner's handoff state (README.md §6.5).
type ownerState uint8

const (
	ownerServing ownerState = iota
	ownerYielding
	ownerClosing
)

// Commit-acknowledgement test hooks (README.md §6.4): drop the connection, or exit the process, instead
// of acknowledging the next client commit after it is durable.
const (
	psAckNormal int32 = iota
	psAckHangUp
	psAckCrash
)

// pageServiceOwner is the serving state of an owner. meta, metaTxid, window, windowStart, and
// resolution change only together with the published version, under core.liveMu, so begin_read
// observes a consistent (snapshot, meta, invalidation window) triple.
type pageServiceOwner struct {
	core        *sharedCore
	dbPath      string
	incarnation id16
	pageSize    uint32
	readOnly    bool
	preference  Ownership
	limits      ownerLimits
	// coord holds presence EX for this ownership; releasing ownership closes it.
	coord *fileCoordinator

	// Guarded by core.liveMu.
	meta        []byte
	metaTxid    uint64
	window      []psWindowEntry
	windowStart uint64
	resolution  []psResolution

	// gate is the writer gate: a one-slot semaphore, held by whoever is writing the database (an owner
	// write transaction, a compaction, or a client between write_begun and its commit's publication).
	gate chan struct{}
	// commitMu is held from a client commit's point of no return through its publication, and by
	// adoption, so neither observes a half-published commit and commit_status can wait for one.
	commitMu sync.Mutex
	// foreign is the txid of the newest client commit published here; the owner's committed roots
	// adopt it at their next transaction begin (adoptForeign).
	foreign atomic.Uint64
	// nextAck is the commit-resolution test hook (psAck*).
	nextAck atomic.Int32

	// stateMu guards state and requester. drainDone (set under stateMu, read lock-free by every owner
	// transaction begin) is closed when a yield's drain ends, which
	// releases the owner's own waiting begins (waitAdmission).
	stateMu   sync.Mutex
	state     ownerState
	requester *psServerConn
	drainDone atomic.Pointer[chan struct{}]

	// fileMu guards file, the owner's own read handle on the database: page reads use positioned reads
	// on it, independent of the pager's lock, so a client never waits on the owner's commit. Compaction
	// replaces it.
	fileMu sync.RWMutex
	file   blockStore
	// host and files are the handle's lock host and file host (the OS, or the simulator's).
	host  lockHost
	files fileHost

	listener net.Listener
	endpoint string

	connMu   sync.Mutex
	conns    map[*psServerConn]struct{}
	nextConn uint64
	closed   bool
	closing  chan struct{}
	wg       sync.WaitGroup
}

// psWriteTxn is a client write transaction: the connection holds the gate and a write pin on base.
type psWriteTxn struct {
	base  uint64
	grant bool
	// relaxed is a begin_write with the relaxed bit: the commit joins the owner's batch (README.md §3.5).
	relaxed bool
	check   psDeltaBase
	// delta is the CommitDelta being received (between commit_begin and commit_end); failure is a
	// rejected delta, whose remaining frames are discarded and which commit_end reports.
	delta   *psDelta
	failure error
	// started is whether the owner's pager has begun this commit (a page was written).
	started bool
	// finishing is set at the point of no return: the commit completes whatever the connection does.
	finishing bool
}

// psServerConn is one client connection on the owner side.
type psServerConn struct {
	owner    *pageServiceOwner
	conn     net.Conn
	id       uint64
	readOnly bool
	// pins counts this connection's pins per txid, the write pin included; each is also one count in
	// core.live. Guarded by core.liveMu.
	pins map[uint64]int

	// sendMu serializes frames onto the connection: a begin_write's reply is sent by its gate waiter.
	sendMu sync.Mutex

	// mu guards the write-transaction state below.
	mu      sync.Mutex
	write   *psWriteTxn
	waiting bool // a begin_write is waiting for the gate
	// revoked is set when the idle-in-transaction timeout revoked the write transaction on
	// revokedBase; the next write-path request reports 25P03.
	revoked     bool
	revokedBase uint64
	closed      bool
	done        chan struct{}
	idle        *time.Timer
	// parked holds begins a draining owner left unanswered; a cancelled yield answers them.
	parked []func()
	// session closes the connection after the idle-session timeout (page-service.md §11.1).
	session *time.Timer
}

func newIncarnation() id16 {
	var id id16
	for id == (id16{}) {
		if _, err := rand.Read(id[:]); err != nil {
			panic("page service: no OS entropy for an incarnation id: " + err.Error())
		}
	}
	return id
}

// newPageServiceOwner prepares the owner of the database whose committed snapshot txid is read through
// paging: its incarnation, committed meta, and windows. The caller holds presence EX (coord); start
// begins serving.
func newPageServiceOwner(c *sharedCore, paging *sharedPaging, txid uint64, h *serviceHandoff, coord *fileCoordinator) (*pageServiceOwner, error) {
	o := &pageServiceOwner{
		core:        c,
		dbPath:      h.path,
		incarnation: newIncarnation(),
		pageSize:    c.storage.pageSize,
		readOnly:    h.opts.ReadOnly,
		preference:  h.ownership,
		limits:      h.limits,
		coord:       coord,
		host:        h.host,
		files:       h.fileHost(),
		conns:       map[*psServerConn]struct{}{},
		gate:        make(chan struct{}, 1),
		closing:     make(chan struct{}),
	}
	meta, err := readCommittedMeta(paging, txid)
	if err != nil {
		return nil, err
	}
	o.meta, o.metaTxid, o.windowStart = meta, txid, txid
	o.seedResolution(paging, txid, meta)
	if o.file, err = o.files.openStore(h.path, true, true); err != nil {
		return nil, err
	}
	return o, nil
}

// start writes the owner record, binds the endpoint, and starts accepting. An owner that cannot bind (no
// private endpoint directory, or a failed bind) still owns and records that it does not serve.
func (o *pageServiceOwner) start() error {
	uid := currentUID()
	o.endpoint = o.host.endpointPath(o.dbPath, uid, o.incarnation)
	record := ownerRecord{serving: o.endpoint != "", uid: uid, incarnation: o.incarnation, pid: uint32(os.Getpid()), endpoint: o.endpoint}
	if err := o.host.writeOwnerRecord(o.dbPath, record); err != nil {
		return err
	}
	if o.endpoint == "" {
		return nil
	}
	listener, err := o.host.listen(o.endpoint)
	if err != nil {
		o.endpoint = ""
		_ = o.host.writeOwnerRecord(o.dbPath, ownerRecord{uid: uid, incarnation: o.incarnation, pid: uint32(os.Getpid())})
		return newError(IoError, "bind page-service endpoint: "+err.Error())
	}
	o.listener = listener
	o.wg.Add(1)
	go o.acceptLoop()
	return nil
}

// readCommittedMeta reads the meta slot that published txid. A commit writes slot txid & 1 unless a
// relaxed batch put it in the other slot (format.md, *Slot alternation*), so both are tried; the parse
// check guards a slot a later commit already overwrote.
func readCommittedMeta(paging *sharedPaging, txid uint64) ([]byte, error) {
	var meta []byte
	err := paging.withPager(func(p *pager) error {
		for _, slot := range []uint32{uint32(txid & 1), uint32(txid&1) ^ 1} {
			block, err := p.readBlock(slot)
			if err != nil {
				return err
			}
			if m, ok := parseMeta(block); ok && m.txid == txid {
				meta = block
				return nil
			}
		}
		return newError(DataCorrupted, "no meta slot carries the committed txid")
	})
	return meta, err
}

// resolutionDigest is a commit's resolution digest (spec/pageservice/README.md §3.5): the CRC-64 of
// its meta page, with the CRC field and the reserve zeroed in the encrypted variant, so the digest of
// the meta a client sent equals that of the meta the owner resealed for its slot
// (spec/fileformat/encrypted.md §14).
func resolutionDigest(meta []byte) uint64 {
	if len(meta) < 7 || meta[6] == 0 {
		return commitCRC64(0, meta)
	}
	m := append([]byte(nil), meta...)
	clear(m[32:36])
	clear(m[len(m)-cryptoReserve:])
	return commitCRC64(0, m)
}

// seedResolution starts the resolution window with the recovered meta slots (README.md §4): the
// committed one and, when the other slot holds the commit just before it, that one too.
func (o *pageServiceOwner) seedResolution(paging *sharedPaging, txid uint64, meta []byte) {
	if txid > 0 {
		_ = paging.withPager(func(p *pager) error {
			for slot := uint32(0); slot < 2; slot++ {
				block, err := p.readBlock(slot)
				if err != nil {
					continue
				}
				if m, ok := parseMeta(block); ok && m.txid == txid-1 {
					o.resolution = append(o.resolution, psResolution{txid: m.txid, digest: resolutionDigest(block)})
					break
				}
			}
			return nil
		})
	}
	o.resolution = append(o.resolution, psResolution{txid: txid, digest: resolutionDigest(meta)})
}

// published records a main-domain commit: its meta, the indices it wrote, and its resolution pair.
// Called by the publisher under core.liveMu, together with the root swap.
func (o *pageServiceOwner) published(txid uint64, meta []byte, written []uint32) {
	o.meta = append([]byte(nil), meta...)
	o.metaTxid = txid
	sorted := append([]uint32(nil), written...)
	sort.Slice(sorted, func(i, j int) bool { return sorted[i] < sorted[j] })
	o.window = append(o.window, psWindowEntry{txid: txid, indices: sorted})
	if len(o.window) > psInvalidationWindowCommits {
		drop := len(o.window) - psInvalidationWindowCommits
		o.windowStart = o.window[drop-1].txid
		o.window = append([]psWindowEntry(nil), o.window[drop:]...)
	}
	o.recordResolution(txid, meta)
}

func (o *pageServiceOwner) recordResolution(txid uint64, meta []byte) {
	o.resolution = append(o.resolution, psResolution{txid: txid, digest: resolutionDigest(meta)})
	if len(o.resolution) > psResolutionWindowCommits {
		o.resolution = append([]psResolution(nil), o.resolution[len(o.resolution)-psResolutionWindowCommits:]...)
	}
}

// compacted resets the window after compaction replaced the file: every older cache is invalid.
// Called under core.liveMu with the writer gate held and no pins, so no page read is in flight.
func (o *pageServiceOwner) compacted(txid uint64) error {
	meta, err := readCommittedMeta(o.core.storage.paging, txid)
	if err != nil {
		return err
	}
	file, err := o.files.openStore(o.dbPath, true, true)
	if err != nil {
		return err
	}
	o.fileMu.Lock()
	old := o.file
	o.file = file
	o.fileMu.Unlock()
	_ = old.close()
	o.meta, o.metaTxid = meta, txid
	o.window, o.windowStart = nil, txid
	o.recordResolution(txid, meta)
	return nil
}

// invalidation answers begin_read's cache question for a client whose cache is from (incarnation,
// lastTxid) (README.md §3.2). Called under core.liveMu.
func (o *pageServiceOwner) invalidation(incarnation id16, lastTxid, txid uint64) (uint8, []uint32) {
	if incarnation != o.incarnation || lastTxid == 0 || lastTxid > txid || lastTxid < o.windowStart {
		return psInvalidateAll, nil
	}
	seen := map[uint32]bool{}
	var out []uint32
	for _, e := range o.window {
		if e.txid <= lastTxid {
			continue
		}
		for _, i := range e.indices {
			if !seen[i] {
				seen[i] = true
				out = append(out, i)
			}
		}
		if len(out) > psMaxInvalidationEntries {
			return psInvalidateAll, nil
		}
	}
	sort.Slice(out, func(i, j int) bool { return out[i] < out[j] })
	return psInvalidateExact, out
}

// resolve answers commit_status (page-service.md §8.4). Called under core.liveMu.
func (o *pageServiceOwner) resolve(txid, digest uint64) uint8 {
	if txid > o.metaTxid {
		return psCommitStatusNotCommitted
	}
	for _, r := range o.resolution {
		if r.txid == txid {
			if r.digest == digest {
				return psCommitStatusCommitted
			}
			return psCommitStatusNotCommitted
		}
	}
	return psCommitStatusUnknown
}

// acquireGate takes the writer gate, waiting at most timeoutMs (0: no deadline): 55P03 on expiry.
// done cancels the wait (a closed connection).
func (o *pageServiceOwner) acquireGate(timeoutMs uint64, done <-chan struct{}) error {
	select {
	case o.gate <- struct{}{}:
		return nil
	default:
	}
	var expired <-chan time.Time
	if timeoutMs > 0 {
		timer := time.NewTimer(time.Duration(timeoutMs) * time.Millisecond)
		defer timer.Stop()
		expired = timer.C
	}
	select {
	case o.gate <- struct{}{}:
		return nil
	case <-expired:
		return newError(LockNotAvailable, "could not obtain the database writer lock")
	case <-done:
		return newError(ConnectionFailure, "the connection closed while waiting for the writer gate")
	case <-o.closing:
		return newError(ConnectionFailure, "the database owner is closing")
	}
}

// tryGate takes the writer gate only if it is free (compaction never waits).
func (o *pageServiceOwner) tryGate() bool {
	select {
	case o.gate <- struct{}{}:
		return true
	default:
		return false
	}
}

func (o *pageServiceOwner) releaseGate() { <-o.gate }

// stopServing stops accepting and closes every connection (clients see EOF, releasing their pins and
// the gate). A client commit past its point of no return still finishes on its connection's goroutine;
// wg.Wait waits for it. Also the response to a write or sync failure in a client commit (page-service.md
// §5.6): the storage handle is poisoned, so no connection may stay.
func (o *pageServiceOwner) stopServing() {
	o.connMu.Lock()
	if !o.closed {
		o.closed = true
		close(o.closing)
	}
	conns := o.connList()
	o.connMu.Unlock()
	if o.listener != nil {
		_ = o.listener.Close()
	}
	for _, sc := range conns {
		_ = sc.conn.Close()
	}
}

// connList snapshots the connections. Caller holds connMu.
func (o *pageServiceOwner) connList() []*psServerConn {
	conns := make([]*psServerConn, 0, len(o.conns))
	for sc := range o.conns {
		conns = append(conns, sc)
	}
	return conns
}

func (o *pageServiceOwner) connections() []*psServerConn {
	o.connMu.Lock()
	defer o.connMu.Unlock()
	return o.connList()
}

// removeServing removes the endpoint and the owner record and closes the read handle, while presence EX
// is still held.
func (o *pageServiceOwner) removeServing() {
	if o.endpoint != "" {
		o.host.removeEndpoint(o.endpoint)
	}
	o.host.removeOwnerRecord(o.dbPath)
	o.fileMu.Lock()
	if o.file != nil {
		_ = o.file.close()
		o.file = nil
	}
	o.fileMu.Unlock()
}

// broadcast sends a notice to every connection.
func (o *pageServiceOwner) broadcast(n psNotice) {
	for _, sc := range o.connections() {
		sc.send(0, n)
	}
}

// waitAdmission blocks an owner session's transaction begin while a yield drains (README.md §6.5).
func (o *pageServiceOwner) waitAdmission() {
	if done := o.drainDone.Load(); done != nil {
		<-*done
	}
}

// admitting reports whether client begins are answered rather than parked.
func (o *pageServiceOwner) admitting() bool {
	o.stateMu.Lock()
	defer o.stateMu.Unlock()
	return o.state == ownerServing
}

// drained reports whether no transaction a drain waits for is running: with own set, the owner's own
// readers and writer as well as every connection's; otherwise only the connections'.
func (o *pageServiceOwner) drained(own bool) bool {
	c := o.core
	if own {
		c.liveMu.Lock()
		readers := len(c.live)
		c.liveMu.Unlock()
		if readers > 0 || len(o.gate) > 0 {
			return false
		}
	}
	for _, sc := range o.connections() {
		c.liveMu.Lock()
		pinned := len(sc.pins) > 0
		c.liveMu.Unlock()
		sc.mu.Lock()
		writing := sc.write != nil
		sc.mu.Unlock()
		if pinned || writing {
			return false
		}
	}
	return true
}

// drain waits for running transactions to finish, up to grace (README.md §6.5). It reports true when
// the yield's requester disconnected first (the yield is cancelled).
func (o *pageServiceOwner) drain(grace time.Duration, own bool, requester *psServerConn) bool {
	deadline := o.host.now().Add(grace)
	for {
		if requester != nil {
			requester.mu.Lock()
			gone := requester.closed
			requester.mu.Unlock()
			if gone {
				return true
			}
		}
		if o.drained(own) || !o.host.now().Before(deadline) {
			return false
		}
		o.host.pause()
	}
}

// requestOwnership answers request_ownership (README.md §6.5): an owner whose preference is yield, or
// whose handle is read-only, yields to the first requester while it is serving.
func (sc *psServerConn) requestOwnership() psMessage {
	o := sc.owner
	o.stateMu.Lock()
	defer o.stateMu.Unlock()
	switch {
	case o.state == ownerYielding && o.requester == sc:
		return psOwnershipReply{status: psOwnershipYielding}
	case o.state != ownerServing, o.preference != OwnershipYield && !o.readOnly:
		return psOwnershipReply{status: psOwnershipRefused}
	}
	done := make(chan struct{})
	o.state, o.requester = ownerYielding, sc
	o.drainDone.Store(&done)
	go o.yield(sc)
	return psOwnershipReply{status: psOwnershipYielding}
}

// yield runs an accepted ownership request: notice, drain for the yield grace, then release ownership and
// re-elect as a paused client — or, when the requester disconnected first, cancel (README.md §6.5).
func (o *pageServiceOwner) yield(requester *psServerConn) {
	c := o.core
	o.broadcast(psNotice{kind: psNoticeYieldBegun, connectionID: requester.id})
	if o.drain(o.limits.yieldGrace, true, requester) {
		o.cancelYield()
		return
	}
	h := c.handoff
	h.mu.Lock()
	r := c.role.Load()
	if r.owner != o || h.closed.Load() {
		h.mu.Unlock()
		o.endDrain()
		return
	}
	next := &serviceRole{gen: r.gen + 1, pauseUntil: h.host.now().Add(psHandoffPause)}
	c.releaseOwnership(r, next)
	h.mu.Unlock()
	o.endDrain()
	_ = c.reelect(next)
}

// cancelYield resumes serving after the requester disconnected: notice, admit again, and answer the
// parked begins.
func (o *pageServiceOwner) cancelYield() {
	o.stateMu.Lock()
	if o.state != ownerYielding {
		// A close began during the drain: it owns the outcome.
		o.stateMu.Unlock()
		return
	}
	o.state, o.requester = ownerServing, nil
	o.stateMu.Unlock()
	o.broadcast(psNotice{kind: psNoticeYieldCancelled})
	o.endDrain()
	for _, sc := range o.connections() {
		sc.mu.Lock()
		parked := sc.parked
		sc.parked = nil
		sc.mu.Unlock()
		for _, run := range parked {
			run()
		}
	}
}

// endDrain releases the owner's own waiting begins.
func (o *pageServiceOwner) endDrain() {
	o.stateMu.Lock()
	done := o.drainDone.Swap(nil)
	o.stateMu.Unlock()
	if done != nil {
		close(*done)
	}
}

// closeGracefully is a serving owner's close (README.md §6.5): notice, then drain its clients'
// transactions for the shutdown grace. The caller then releases ownership.
func (o *pageServiceOwner) closeGracefully() {
	o.stateMu.Lock()
	o.state = ownerClosing
	o.stateMu.Unlock()
	o.endDrain()
	if o.listener == nil {
		return
	}
	o.broadcast(psNotice{kind: psNoticeShutdownBegun})
	o.drain(o.limits.shutdownGrace, false, nil)
}

func (o *pageServiceOwner) acceptLoop() {
	defer o.wg.Done()
	for {
		conn, err := o.listener.Accept()
		if err != nil {
			return
		}
		o.connMu.Lock()
		if o.closed {
			o.connMu.Unlock()
			_ = conn.Close()
			return
		}
		o.nextConn++
		sc := &psServerConn{owner: o, conn: conn, id: o.nextConn, pins: map[uint64]int{}, done: make(chan struct{})}
		sc.armSessionLocked()
		o.conns[sc] = struct{}{}
		o.wg.Add(1)
		o.connMu.Unlock()
		go sc.serve()
	}
}

// serve runs one connection until EOF or a protocol violation, then aborts its write transaction (unless
// past the point of no return) and releases its pins.
func (sc *psServerConn) serve() {
	o := sc.owner
	defer func() {
		sc.mu.Lock()
		sc.closed = true
		sc.parked = nil
		if sc.session != nil {
			sc.session.Stop()
		}
		if sc.write != nil && !sc.write.finishing {
			sc.abortWriteLocked()
		}
		sc.mu.Unlock()
		close(sc.done)
		sc.releasePins()
		o.connMu.Lock()
		delete(o.conns, sc)
		o.connMu.Unlock()
		// Closed last, after the connection's state is released: the simulator treats a closed end as
		// settled.
		_ = sc.conn.Close()
		o.wg.Done()
	}()
	helloDone := false
	for {
		requestID, msg, err := readPSFrame(sc.conn, o.pageSize)
		if err != nil {
			if !errors.Is(err, io.EOF) {
				sc.fail(0, err)
			}
			return
		}
		sc.touch()
		if !helloDone {
			hello, ok := msg.(psHello)
			if !ok {
				sc.fail(requestID, newError(ProtocolViolation, "the first frame must be hello"))
				return
			}
			if !sc.hello(requestID, hello) {
				return
			}
			helloDone = true
			continue
		}
		var reply psMessage
		switch m := msg.(type) {
		case psBeginRead:
			reply = sc.beginReadOrPark(requestID, m)
		case psReadPages:
			reply, err = sc.readPages(m)
		case psEndRead:
			err = sc.endRead(m)
		case psBeginWrite:
			reply, err = sc.beginWrite(requestID, m)
		case psCommitBegin:
			err = sc.commitBegin(m)
		case psCommitPages:
			err = sc.commitPages(m)
		case psCommitEnd:
			reply, err = sc.commitEnd(requestID)
		case psAbort:
			err = sc.abort()
		case psCommitStatus:
			reply = sc.commitStatus(m)
		case psRequestOwnership:
			reply = sc.requestOwnership()
		default:
			err = newError(ProtocolViolation, "unexpected message")
		}
		if err != nil {
			sc.fail(requestID, err)
			return
		}
		if reply != nil && !sc.send(requestID, reply) {
			return
		}
	}
}

// hello negotiates the protocol and the format (README.md §3.1). False closes the connection.
func (sc *psServerConn) hello(requestID uint32, m psHello) bool {
	o := sc.owner
	// Only a stream transport's serving end takes a credential, and it removes it before the hello
	// reaches an owner (spec/pageservice/README.md §3.1).
	if m.credential != nil {
		sc.fail(requestID, newError(ProtocolViolation, "an owner's endpoint does not take a credential in hello"))
		return false
	}
	if m.protocolMin > psProtocolVersion || m.protocolMax < psProtocolVersion {
		sc.fail(requestID, newError(FeatureNotSupported, "unsupported page-service protocol; the owner speaks version 1"))
		return false
	}
	if m.formatVersion != formatVersion {
		sc.fail(requestID, newError(FeatureNotSupported, "the client does not support this database's format_version; it requires format_version "+strconv.Itoa(int(formatVersion))))
		return false
	}
	var flags uint8
	if m.flags&psHelloFlagReadOnly != 0 {
		sc.readOnly = true
		flags |= psHelloOKFlagReadOnly
	}
	if o.readOnly {
		flags |= psHelloOKFlagOwnerReadOnly
	}
	if o.preference == OwnershipYield {
		flags |= psHelloOKFlagOwnerYields
	}
	if limit := o.limits.maxConnections; limit > 0 {
		o.connMu.Lock()
		count := len(o.conns)
		o.connMu.Unlock()
		if count > limit {
			sc.fail(requestID, newError(TooManyConnections, "the database owner already serves its maximum number of connections ("+strconv.Itoa(limit)+")"))
			return false
		}
	}
	maxRead := uint32((psMaxFrameLength - psHeaderLength) / int(o.pageSize))
	if maxRead > psMaxReadPagesCap {
		maxRead = psMaxReadPagesCap
	}
	return sc.send(requestID, psHelloOK{protocol: psProtocolVersion, formatVersion: formatVersion, pageSize: o.pageSize, connectionID: sc.id, flags: flags, maxReadPages: maxRead})
}

// beginReadOrPark answers begin_read, or parks it while the owner drains (README.md §6.5).
func (sc *psServerConn) beginReadOrPark(requestID uint32, m psBeginRead) psMessage {
	if !sc.owner.admitting() {
		sc.mu.Lock()
		sc.parked = append(sc.parked, func() { sc.send(requestID, sc.beginRead(m)) })
		sc.mu.Unlock()
		return nil
	}
	return sc.beginRead(m)
}

// beginRead pins the committed snapshot for this connection (README.md §3.2).
func (sc *psServerConn) beginRead(m psBeginRead) psMessage {
	o := sc.owner
	c := o.core
	c.liveMu.Lock()
	defer c.liveMu.Unlock()
	txid := o.metaTxid
	c.live[txid]++
	sc.pins[txid]++
	invalidate, indices := o.invalidation(m.incarnation, m.lastTxid, txid)
	return psReadBegun{incarnation: o.incarnation, txid: txid, invalidate: invalidate, indices: indices, meta: o.meta}
}

// pinnedPageCount returns the page count of a snapshot this connection pins, or ok=false.
func (sc *psServerConn) pinnedPageCount(txid uint64) (uint32, bool) {
	c := sc.owner.core
	c.liveMu.Lock()
	defer c.liveMu.Unlock()
	if sc.pins[txid] == 0 {
		return 0, false
	}
	// The page count only grows between compactions, and a pin blocks compaction, so the current
	// count bounds every page of the pinned snapshot.
	m, _ := parseMeta(sc.owner.meta)
	return m.pageCount, true
}

// readPages returns the requested pages (README.md §3.3).
func (sc *psServerConn) readPages(m psReadPages) (psMessage, error) {
	o := sc.owner
	pageCount, ok := sc.pinnedPageCount(m.txid)
	if !ok {
		sc.mu.Lock()
		defer sc.mu.Unlock()
		if sc.revoked && m.txid == sc.revokedBase {
			// A read under a revoked write transaction's snapshot: report the revocation.
			sc.revoked = false
			return errIdleRevoked(), nil
		}
		return nil, newError(ProtocolViolation, "read_pages names a txid this connection has not pinned")
	}
	maxRead := (psMaxFrameLength - psHeaderLength) / int(o.pageSize)
	if len(m.indices) == 0 || len(m.indices) > min(maxRead, psMaxReadPagesCap) {
		return nil, newError(ProtocolViolation, "read_pages names too few or too many pages")
	}
	pages := make([][]byte, len(m.indices))
	o.fileMu.RLock()
	defer o.fileMu.RUnlock()
	for i, index := range m.indices {
		if index < 2 || index >= pageCount {
			return nil, newError(ProtocolViolation, "read_pages names a page outside the pinned snapshot")
		}
		page, err := o.file.readAt(int64(index)*int64(o.pageSize), int(o.pageSize))
		if err != nil {
			return nil, err
		}
		pages[i] = page
	}
	return psPages{pages: pages}, nil
}

// endRead releases one pin (README.md §3.4).
func (sc *psServerConn) endRead(m psEndRead) error {
	c := sc.owner.core
	c.liveMu.Lock()
	defer c.liveMu.Unlock()
	if sc.pins[m.txid] == 0 {
		return newError(ProtocolViolation, "end_read names a txid this connection has not pinned")
	}
	sc.unpinLocked(m.txid)
	return nil
}

// unpinLocked drops one of this connection's pins on txid. Caller holds core.liveMu.
func (sc *psServerConn) unpinLocked(txid uint64) {
	c := sc.owner.core
	if sc.pins[txid]--; sc.pins[txid] <= 0 {
		delete(sc.pins, txid)
	}
	if c.live[txid]--; c.live[txid] <= 0 {
		delete(c.live, txid)
	}
}

// releasePins drops every pin of a closed connection.
func (sc *psServerConn) releasePins() {
	c := sc.owner.core
	c.liveMu.Lock()
	defer c.liveMu.Unlock()
	for txid, n := range sc.pins {
		if c.live[txid] -= n; c.live[txid] <= 0 {
			delete(c.live, txid)
		}
	}
	sc.pins = map[uint64]int{}
}

func errIdleRevoked() psError {
	return psError{sqlstate: "25P03", message: "terminating the write transaction due to the idle-in-transaction timeout"}
}

// beginWrite starts a client write transaction (README.md §3.5). The gate wait runs on its own
// goroutine, so the connection keeps serving reads meanwhile; write_begun (or the wait's error) is
// sent when the wait ends.
func (sc *psServerConn) beginWrite(requestID uint32, m psBeginWrite) (psMessage, error) {
	o := sc.owner
	if sc.readOnly {
		return psError{sqlstate: "25006", message: "cannot write through a read-only page-service connection"}, nil
	}
	if o.readOnly {
		return psError{sqlstate: "25006", message: "the database owner opened the file read-only"}, nil
	}
	sc.mu.Lock()
	defer sc.mu.Unlock()
	if sc.write != nil || sc.waiting {
		return nil, newError(ProtocolViolation, "begin_write while a write transaction is open")
	}
	if sc.revoked {
		sc.revoked = false
		return errIdleRevoked(), nil
	}
	sc.waiting = true
	if !o.admitting() {
		// Parked while the owner drains (README.md §6.5); a cancelled yield starts the wait.
		sc.parked = append(sc.parked, func() { go sc.waitForGate(requestID, m) })
		return nil, nil
	}
	go sc.waitForGate(requestID, m)
	return nil, nil
}

// waitForGate acquires the gate for a begin_write, then pins the committed snapshot and answers.
func (sc *psServerConn) waitForGate(requestID uint32, m psBeginWrite) {
	o := sc.owner
	c := o.core
	relaxed := m.flags&psBeginWriteRelaxed != 0
	err := o.acquireGate(m.lockTimeoutMs, sc.done)
	if err == nil {
		// The owner's batch (README.md §3.5): a synchronous client plans against a durable floor, so the
		// pending set is flushed first; a relaxed one is told the pending set instead, once any cap flush
		// in progress has finished. A failed flush poisons the owner, which stops serving.
		st := c.storage
		st.mu.Lock()
		var ferr error
		if relaxed {
			// A batch past its deadline is flushed first (validated-cow.md §4.1 rule 4).
			ferr = st.beginClientBatch()
		} else {
			ferr = st.flush(false)
		}
		st.mu.Unlock()
		if ferr != nil {
			o.releaseGate()
			err = ferr
			go o.stopServing()
		}
	}
	sc.mu.Lock()
	defer sc.mu.Unlock()
	sc.waiting = false
	if sc.closed {
		if err == nil {
			o.releaseGate()
		}
		return
	}
	if err != nil {
		code, message := "55P03", err.Error()
		var engine *EngineError
		if errors.As(err, &engine) {
			code, message = engine.Code(), engine.Message
		}
		sc.send(requestID, psError{sqlstate: code, message: message})
		return
	}
	st := c.storage
	c.liveMu.Lock()
	txid := o.metaTxid
	c.live[txid]++
	sc.pins[txid]++
	invalidate, indices := o.invalidation(m.incarnation, m.lastTxid, txid)
	// The reuse grant (page-service.md §6): every pin, this write pin included, is at or after the
	// free list's generation. Later pins are on this snapshot or newer, so they cannot void it.
	oldest := txid
	for v := range c.live {
		oldest = min(oldest, v)
	}
	grant := oldest >= st.freeGenTxid
	meta := o.meta
	c.liveMu.Unlock()
	codec := st.paging.codec()
	check := psDeltaBase{committedTxid: txid, pageSize: o.pageSize, pageCount: st.pageCount, encrypted: codec != nil, codec: codec}
	var pending []manifestEntry
	var reuse []uint32
	st.mu.Lock()
	if relaxed {
		_ = st.paging.withPager(func(p *pager) error { pending = p.pendingEntries(); return nil })
	}
	switch {
	case len(pending) > 0:
		// Inside a batch the grant narrows to the committed free pages that were free at the floor
		// (validated-cow.md §4 rule 3), the lowest psMaxReuse of them.
		check.pending = make(map[uint32]uint64, len(pending))
		for _, e := range pending {
			check.pending[e.index] = e.digest
		}
		check.granted = map[uint32]bool{}
		if grant {
			floor := st.floorFreeFor(true)
			for _, p := range st.freePages {
				if floor[p] {
					reuse = append(reuse, p)
				}
			}
			sortUint32s(reuse)
			reuse = reuse[:min(len(reuse), psMaxReuse)]
			for _, p := range reuse {
				check.granted[p] = true
			}
		}
	case grant:
		check.granted = make(map[uint32]bool, len(st.freePages))
		for _, p := range st.freePages {
			check.granted[p] = true
		}
	}
	if relaxed && len(pending) == 0 {
		// The client's commit may start a batch: the committed free list is the floor's (rule 3).
		st.recordFloorFree()
	}
	st.mu.Unlock()
	sc.write = &psWriteTxn{base: txid, grant: grant, relaxed: relaxed, check: check}
	sc.armIdleLocked()
	sc.send(requestID, psWriteBegun{incarnation: o.incarnation, txid: txid, invalidate: invalidate, indices: indices, reuseGrant: grant, meta: meta, pending: pending, reuse: reuse})
}

// touch restarts the idle-in-transaction and idle-session timers on every received frame.
func (sc *psServerConn) touch() {
	sc.mu.Lock()
	defer sc.mu.Unlock()
	if sc.write != nil && !sc.write.finishing {
		sc.armIdleLocked()
	}
	sc.armSessionLocked()
}

func (sc *psServerConn) armIdleLocked() {
	timeout := sc.owner.limits.idleInTransaction
	if timeout <= 0 {
		return
	}
	if sc.idle == nil {
		sc.idle = time.AfterFunc(timeout, sc.revoke)
	} else {
		sc.idle.Reset(timeout)
	}
}

// armSessionLocked (re)starts the idle-session timer (page-service.md §11.1).
func (sc *psServerConn) armSessionLocked() {
	timeout := sc.owner.limits.idleSession
	if timeout <= 0 {
		return
	}
	if sc.session == nil {
		sc.session = time.AfterFunc(timeout, sc.idleSessionExpired)
	} else {
		sc.session.Reset(timeout)
	}
}

// idleSessionExpired closes a connection idle between transactions for the idle-session timeout,
// without an error frame; a busy connection's timer starts again.
func (sc *psServerConn) idleSessionExpired() {
	c := sc.owner.core
	c.liveMu.Lock()
	pinned := len(sc.pins) > 0
	c.liveMu.Unlock()
	sc.mu.Lock()
	defer sc.mu.Unlock()
	if sc.closed {
		return
	}
	if pinned || sc.write != nil || sc.waiting || len(sc.parked) > 0 {
		sc.armSessionLocked()
		return
	}
	_ = sc.conn.Close()
}

func (sc *psServerConn) stopIdleLocked() {
	if sc.idle != nil {
		sc.idle.Stop()
	}
}

// revoke ends a write transaction that received nothing for the idle-in-transaction timeout
// (page-service.md §11.1): the gate is released, and the next write-path request reports 25P03.
func (sc *psServerConn) revoke() {
	sc.mu.Lock()
	defer sc.mu.Unlock()
	if sc.write == nil || sc.write.finishing || sc.closed {
		return
	}
	sc.revokedBase = sc.write.base
	sc.abortWriteLocked()
	sc.revoked = true
}

// abortWriteLocked ends the write transaction without committing: delta pages already written are
// unreachable orphans, the write pin and the gate are released. Caller holds sc.mu.
func (sc *psServerConn) abortWriteLocked() {
	o := sc.owner
	c := o.core
	w := sc.write
	sc.write = nil
	sc.stopIdleLocked()
	if w.started {
		_ = c.storage.paging.withPager(func(p *pager) error {
			p.abandonValidatedCommit()
			return nil
		})
	}
	c.liveMu.Lock()
	sc.unpinLocked(w.base)
	c.liveMu.Unlock()
	o.releaseGate()
}

// abort ends the write transaction at the client's request (README.md §3.5).
func (sc *psServerConn) abort() error {
	sc.mu.Lock()
	defer sc.mu.Unlock()
	if sc.write != nil {
		sc.abortWriteLocked()
		return nil
	}
	if sc.revoked {
		sc.revoked = false
		return nil
	}
	return newError(ProtocolViolation, "abort without a write transaction")
}

// writeDeltaPages writes validated delta pages through the owner's pager, beginning the commit at the
// first write. A write failure poisons the owner (stopServing).
func (sc *psServerConn) writeDeltaPages(w *psWriteTxn, pages []psIndexedPage) error {
	st := sc.owner.core.storage
	st.mu.Lock()
	defer st.mu.Unlock()
	err := st.paging.withPager(func(p *pager) error {
		if !w.started {
			if err := p.beginValidatedCommit(); err != nil {
				return err
			}
			w.started = true
			if err := p.reserve(w.delta.m.pageCount); err != nil {
				return err
			}
		}
		for _, pg := range pages {
			if err := p.writeBlock(pg.index, pg.page); err != nil {
				return err
			}
			st.paging.pool.invalidate(pg.index)
		}
		return nil
	})
	if err != nil {
		sc.owner.stopServing()
	}
	return err
}

// commitBegin runs check A and writes the manifest pages (README.md §3.5).
func (sc *psServerConn) commitBegin(m psCommitBegin) error {
	sc.mu.Lock()
	defer sc.mu.Unlock()
	w := sc.write
	if w == nil {
		if sc.revoked {
			return nil // the revoked transaction's delta is discarded; commit_end reports 25P03
		}
		return newError(ProtocolViolation, "commit_begin without a write transaction")
	}
	if w.delta != nil || w.failure != nil {
		return newError(ProtocolViolation, "commit_begin twice in one write transaction")
	}
	d, err := beginDelta(w.check, m)
	if err == nil {
		err = d.checkMeta(w.check.codec)
	}
	if err != nil {
		w.failure = err
		return nil
	}
	w.delta = d
	return sc.writeDeltaPages(w, d.manifest)
}

// commitPages runs check B on each page and writes it (README.md §3.5).
func (sc *psServerConn) commitPages(m psCommitPages) error {
	sc.mu.Lock()
	defer sc.mu.Unlock()
	w := sc.write
	if w == nil {
		if sc.revoked {
			return nil
		}
		return newError(ProtocolViolation, "commit_pages without a write transaction")
	}
	if w.failure != nil {
		return nil
	}
	if w.delta == nil {
		return newError(ProtocolViolation, "commit_pages before commit_begin")
	}
	for _, pg := range m.pages {
		if err := w.delta.checkPage(pg); err != nil {
			w.failure = err
			return nil
		}
	}
	return sc.writeDeltaPages(w, m.pages)
}

// commitEnd runs check C; past it (the point of no return) the commit completes whatever the
// connection does (README.md §3.5).
func (sc *psServerConn) commitEnd(requestID uint32) (psMessage, error) {
	sc.mu.Lock()
	w := sc.write
	if w == nil {
		defer sc.mu.Unlock()
		if sc.revoked {
			sc.revoked = false
			return errIdleRevoked(), nil
		}
		return nil, newError(ProtocolViolation, "commit_end without a write transaction")
	}
	if w.failure == nil && w.delta == nil {
		sc.mu.Unlock()
		return nil, newError(ProtocolViolation, "commit_end before commit_begin")
	}
	if w.failure == nil {
		w.failure = w.delta.complete()
	}
	if w.failure != nil {
		err := w.failure
		sc.abortWriteLocked()
		sc.mu.Unlock()
		return nil, err
	}
	w.finishing = true
	sc.stopIdleLocked()
	sc.mu.Unlock()
	return nil, sc.finishClientCommit(requestID, w)
}

// finishClientCommit publishes a validated delta (page-service.md §5.6): meta, the single durable
// barrier, structural adoption (page accounting, the invalidation and resolution windows), the
// acknowledgement, then — still holding the gate — the catalog adoption and a maintenance commit if
// one is due. A write or sync failure poisons the owner, which stops serving.
func (sc *psServerConn) finishClientCommit(requestID uint32, w *psWriteTxn) error {
	o := sc.owner
	c := o.core
	st := c.storage
	d := w.delta
	o.commitMu.Lock()
	st.mu.Lock()
	capReached := false
	beganEmpty := false
	err := st.paging.withPager(func(p *pager) error {
		// The slot not holding the floor (README.md §3.5). A synchronous client's commit keeps its own
		// barrier (the begin_write flush left nothing pending); a relaxed one joins the batch.
		slot := p.commitSlot()
		beganEmpty = len(p.pending) == 0
		// A client cannot know that slot, so an encrypted meta, already checked against the database
		// key, is resealed for it (spec/fileformat/encrypted.md §14).
		sealed, err := sealMeta(p, slot, d.meta, d.manifestChain())
		if err != nil {
			return err
		}
		d.meta = sealed
		if err := p.writeBlock(slot, d.meta); err != nil {
			return err
		}
		if !w.relaxed {
			if err := p.sync(); err != nil {
				return err
			}
		}
		p.finishValidatedCommit(d.meta, slot, !w.relaxed, d.own())
		p.manifestProtected = d.protected
		capReached = w.relaxed && p.batchCapReached()
		return nil
	})
	var free []uint32
	if err == nil {
		free, err = readFreeList(st.paging, pageRef{page: d.m.freeListHead, tag: d.m.freeListTag}, d.m.pageCount)
	}
	if err != nil {
		st.mu.Unlock()
		o.commitMu.Unlock()
		o.stopServing()
		sc.mu.Lock()
		sc.write = nil
		sc.mu.Unlock()
		c.liveMu.Lock()
		sc.unpinLocked(w.base)
		c.liveMu.Unlock()
		o.releaseGate()
		// The meta may be durable: the outcome is unknown, so the client sees a lost connection and
		// resolves it with commit_status (under the next owner) rather than an error frame.
		_ = sc.conn.Close()
		return nil
	}
	st.pageCount, st.freePages = d.m.pageCount, free
	if w.relaxed {
		st.afterClientCommit(beganEmpty, capReached)
	}
	st.mu.Unlock()
	c.liveMu.Lock()
	sc.unpinLocked(w.base)
	o.published(d.txid, d.meta, d.written)
	o.foreign.Store(d.txid)
	c.liveMu.Unlock()
	o.commitMu.Unlock()
	sc.mu.Lock()
	sc.write = nil
	sc.mu.Unlock()
	switch o.nextAck.Swap(psAckNormal) {
	case psAckHangUp:
		_ = sc.conn.Close()
	case psAckCrash:
		os.Exit(70)
	default:
		sc.send(requestID, psCommitted{txid: d.txid})
	}
	if capReached && !o.capFlushClientCommit() {
		o.releaseGate()
		return nil
	}
	if c.adoptForeign(o) == nil {
		c.maintenanceCommit(o)
	}
	o.releaseGate()
	return nil
}

// capFlushClientCommit ends a batch a relaxed client commit brought to the cap (README.md §3.5): the
// flush runs at once, before any maintenance commit, and shrinks nothing — a shrink an owner commit
// deferred waits for the owner's own next barrier. A failed barrier revokes the storage, as a local cap
// flush does (validated-cow.md §4), and the owner stops serving. Called holding the gate.
func (o *pageServiceOwner) capFlushClientCommit() bool {
	st := o.core.storage
	st.mu.Lock()
	err := st.paging.withPager(func(p *pager) error { return p.flushPending(false) })
	if err != nil {
		st.paging.revoke(newError(IoError, "a deferred durability flush failed; reopen the database: "+err.Error()))
	}
	st.mu.Unlock()
	if err != nil {
		o.stopServing()
		return false
	}
	return true
}

// commitStatus resolves an unacknowledged commit (page-service.md §8.4), after any commit past its
// point of no return finishes.
func (sc *psServerConn) commitStatus(m psCommitStatus) psMessage {
	o := sc.owner
	// A commit past its point of no return holds commitMu until it is published.
	o.commitMu.Lock()
	o.commitMu.Unlock()
	o.core.liveMu.Lock()
	defer o.core.liveMu.Unlock()
	return psCommitStatusReply{status: o.resolve(m.txid, m.digest)}
}

func (sc *psServerConn) send(requestID uint32, m psMessage) bool {
	frame, err := encodePSFrame(requestID, m)
	if err != nil {
		return false
	}
	sc.sendMu.Lock()
	defer sc.sendMu.Unlock()
	_, err = sc.conn.Write(frame)
	return err == nil
}

// fail answers a request with a fatal error; the caller then closes the connection.
func (sc *psServerConn) fail(requestID uint32, err error) {
	code, message := "08P01", err.Error()
	var engine *EngineError
	if errors.As(err, &engine) {
		code, message = engine.Code(), engine.Message
	}
	sc.send(requestID, psError{sqlstate: code, message: message, flags: psErrorFlagFatal})
}

// adoptForeign brings the owner's committed roots up to the newest client commit (page-service.md §7):
// its catalog is loaded here, at an owner transaction begin, never in the commit path. A failure (a
// collation this owner's environment lacks, say) fails only the transaction that needed it.
func (c *sharedCore) adoptForeign(o *pageServiceOwner) error {
	if o == nil || o.foreign.Load() <= c.roots.Load().committed.txid {
		return nil
	}
	o.commitMu.Lock()
	defer o.commitMu.Unlock()
	if o.foreign.Load() <= c.roots.Load().committed.txid {
		return nil
	}
	c.storage.mu.Lock()
	loaded, err := loadEngineSharedPaging(c.storage.paging, c.env)
	if err == nil {
		c.storage.live = loaded.live
	}
	c.storage.mu.Unlock()
	if err != nil {
		return err
	}
	c.liveMu.Lock()
	rt := c.roots.Load()
	if c.pageOwner() == o && loaded.committed.txid > rt.committed.txid {
		c.roots.Store(&roots{committed: loaded.committed, attached: rt.attached})
	}
	c.liveMu.Unlock()
	c.planEpoch.Add(1)
	return nil
}

// maintenanceCommit is the owner-only free-list rebuild after a client commit (page-service.md §6):
// a client commit never walks the database, so when the pages off the free list pass the reclamation threshold
// and no pin holds back reclamation, the owner commits the unchanged snapshot at the next txid with
// in-commit reclamation. A client commit never lowers the high-water either, so the owner also commits
// when the free list ends at the high-water, and that commit truncates it (format.md *Trailing
// truncation*). Called holding the gate, after the catalog adoption.
func (c *sharedCore) maintenanceCommit(o *pageServiceOwner) {
	r := c.role.Load()
	if r.owner != o {
		return
	}
	st := c.storage
	rt := c.roots.Load()
	newTxid := rt.committed.txid + 1
	if c.oldestLiveVersion(newTxid) != newTxid {
		return
	}
	if !(st.liveAtCompaction == 0 || reclaimDue(st.pageCount, len(st.freePages), st.liveAtCompaction) || st.endsFree()) {
		return
	}
	snap := rt.committed.clone()
	snap.txid = newTxid
	budget := &budgetCtx{name: c.domainName(), prev: rt.committed, guard: c.roleGuard(r)}
	if err := st.commitDurable(snap, true, true, budget); err != nil {
		if r.paging != nil && r.paging.commitRequiresReopen() {
			o.stopServing()
		}
		return
	}
	c.liveMu.Lock()
	c.roots.Store(&roots{committed: snap, attached: rt.attached})
	o.published(snap.txid, st.lastMeta, st.lastWritten)
	c.liveMu.Unlock()
	c.planEpoch.Add(1)
}

// HangUpNextPageServiceAckForTesting makes a serving owner drop the connection instead of
// acknowledging its next client commit, after the commit is durable — the case commit_status resolves
// (page-service.md §8.4). Test-only; no effect on a handle that is not a page-service owner.
func HangUpNextPageServiceAckForTesting(db *Database) {
	if o := db.core.pageOwner(); o != nil {
		o.nextAck.Store(psAckHangUp)
	}
}

// CrashBeforeNextPageServiceAckForTesting makes a serving owner exit its process instead of
// acknowledging its next client commit, after the commit is durable, so the client resolves the commit
// under the next owner — itself, when it is the only other process (README.md §6.5). Test-only.
func CrashBeforeNextPageServiceAckForTesting(db *Database) {
	if o := db.core.pageOwner(); o != nil {
		o.nextAck.Store(psAckCrash)
	}
}
