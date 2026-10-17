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

// pageServiceOwner is the serving state of an owner. meta, metaTxid, window, windowStart, and
// resolution change only together with the published version, under core.liveMu, so begin_read
// observes a consistent (snapshot, meta, invalidation window) triple.
type pageServiceOwner struct {
	core        *sharedCore
	dbPath      string
	incarnation id16
	pageSize    uint32
	readOnly    bool
	idleTimeout time.Duration

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
	// hangUpNextAck is the commit-resolution test hook: drop the connection instead of the next
	// client commit's acknowledgement (HangUpNextPageServiceAckForTesting).
	hangUpNextAck atomic.Bool

	// fileMu guards file, the owner's own read handle on the database: page reads use positioned reads
	// on it, independent of the pager's lock, so a client never waits on the owner's commit. Compaction
	// replaces it.
	fileMu sync.RWMutex
	file   *os.File

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
	check psDeltaBase
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

// startPageServiceOwner makes c the serving owner of the database at dbPath: it records the committed
// meta, writes the owner record, binds the endpoint, and starts accepting. The caller holds presence
// EX. An owner that cannot bind (no private endpoint directory) still owns and records flags = 0.
func startPageServiceOwner(c *sharedCore, dbPath string, idleTimeoutMs uint64) (*pageServiceOwner, error) {
	o := &pageServiceOwner{
		core:        c,
		dbPath:      dbPath,
		incarnation: newIncarnation(),
		pageSize:    c.storage.pageSize,
		readOnly:    c.storage.readOnly,
		idleTimeout: time.Duration(idleTimeoutMs) * time.Millisecond,
		conns:       map[*psServerConn]struct{}{},
		gate:        make(chan struct{}, 1),
		closing:     make(chan struct{}),
	}
	txid := c.roots.Load().committed.txid
	meta, err := o.readCommittedMeta(txid)
	if err != nil {
		return nil, err
	}
	o.meta, o.metaTxid, o.windowStart = meta, txid, txid
	o.seedResolution(txid, meta)
	c.storage.recordCommits = true
	if o.file, err = os.Open(dbPath); err != nil {
		return nil, newError(IoError, "open database for page service: "+err.Error())
	}
	uid := currentUID()
	o.endpoint = endpointPath(dbPath, uid, o.incarnation)
	record := ownerRecord{serving: o.endpoint != "", uid: uid, incarnation: o.incarnation, pid: uint32(os.Getpid()), endpoint: o.endpoint}
	if err := writeOwnerRecord(dbPath, record); err != nil {
		_ = o.file.Close()
		return nil, err
	}
	if o.endpoint == "" {
		return o, nil
	}
	if o.listener, err = listenEndpoint(o.endpoint); err != nil {
		_ = o.file.Close()
		return nil, newError(IoError, "bind page-service endpoint: "+err.Error())
	}
	o.wg.Add(1)
	go o.acceptLoop()
	return o, nil
}

// readCommittedMeta reads the meta slot that published txid. Every commit writes slot txid & 1, so it
// is the one to read; the parse check guards a slot a later commit already overwrote.
func (o *pageServiceOwner) readCommittedMeta(txid uint64) ([]byte, error) {
	var meta []byte
	err := o.core.storage.paging.withPager(func(p *pager) error {
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

// seedResolution starts the resolution window with the recovered meta slots (README.md §4): the
// committed one and, when the other slot holds the commit just before it, that one too.
func (o *pageServiceOwner) seedResolution(txid uint64, meta []byte) {
	if txid > 0 {
		_ = o.core.storage.paging.withPager(func(p *pager) error {
			block, err := p.readBlock(uint32(txid&1) ^ 1)
			if err == nil {
				if m, ok := parseMeta(block); ok && m.txid == txid-1 {
					o.resolution = append(o.resolution, psResolution{txid: m.txid, digest: commitCRC64(0, block)})
				}
			}
			return nil
		})
	}
	o.resolution = append(o.resolution, psResolution{txid: txid, digest: commitCRC64(0, meta)})
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
	o.resolution = append(o.resolution, psResolution{txid: txid, digest: commitCRC64(0, meta)})
	if len(o.resolution) > psResolutionWindowCommits {
		o.resolution = append([]psResolution(nil), o.resolution[len(o.resolution)-psResolutionWindowCommits:]...)
	}
}

// compacted resets the window after compaction replaced the file: every older cache is invalid.
// Called under core.liveMu with the writer gate held and no pins, so no page read is in flight.
func (o *pageServiceOwner) compacted(txid uint64) error {
	meta, err := o.readCommittedMeta(txid)
	if err != nil {
		return err
	}
	file, err := os.Open(o.dbPath)
	if err != nil {
		return newError(IoError, "reopen database for page service: "+err.Error())
	}
	o.fileMu.Lock()
	old := o.file
	o.file = file
	o.fileMu.Unlock()
	_ = old.Close()
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

// close stops serving: no new connections, every client sees EOF (releasing its pins and the gate),
// and the endpoint and owner record are removed while presence EX is still held. A client commit past
// its point of no return finishes first.
func (o *pageServiceOwner) close() {
	o.connMu.Lock()
	if !o.closed {
		o.closed = true
		close(o.closing)
	}
	conns := make([]*psServerConn, 0, len(o.conns))
	for sc := range o.conns {
		conns = append(conns, sc)
	}
	o.connMu.Unlock()
	if o.listener != nil {
		_ = o.listener.Close()
	}
	for _, sc := range conns {
		_ = sc.conn.Close()
	}
	o.wg.Wait()
	if o.endpoint != "" {
		_ = os.Remove(o.endpoint)
	}
	_ = os.Remove(ownerRecordPath(o.dbPath))
	o.fileMu.Lock()
	if o.file != nil {
		_ = o.file.Close()
		o.file = nil
	}
	o.fileMu.Unlock()
}

// stopServing is the owner's response to a write or sync failure in a client commit (page-service.md
// §5.6): its storage handle is poisoned, so it closes every connection and accepts no more. The
// owner's own handle then refuses commits until it is reopened.
func (o *pageServiceOwner) stopServing() {
	o.connMu.Lock()
	if !o.closed {
		o.closed = true
		close(o.closing)
	}
	conns := make([]*psServerConn, 0, len(o.conns))
	for sc := range o.conns {
		conns = append(conns, sc)
	}
	o.connMu.Unlock()
	if o.listener != nil {
		_ = o.listener.Close()
	}
	for _, sc := range conns {
		_ = sc.conn.Close()
	}
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
		_ = sc.conn.Close()
		sc.mu.Lock()
		sc.closed = true
		if sc.write != nil && !sc.write.finishing {
			sc.abortWriteLocked()
		}
		sc.mu.Unlock()
		close(sc.done)
		sc.releasePins()
		o.connMu.Lock()
		delete(o.conns, sc)
		o.connMu.Unlock()
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
			reply = sc.beginRead(m)
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
	maxRead := uint32((psMaxFrameLength - psHeaderLength) / int(o.pageSize))
	if maxRead > psMaxReadPagesCap {
		maxRead = psMaxReadPagesCap
	}
	return sc.send(requestID, psHelloOK{protocol: psProtocolVersion, formatVersion: formatVersion, pageSize: o.pageSize, connectionID: sc.id, flags: flags, maxReadPages: maxRead})
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
		page := make([]byte, o.pageSize)
		if _, err := o.file.ReadAt(page, int64(index)*int64(o.pageSize)); err != nil {
			return nil, newError(IoError, "page service read: "+err.Error())
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
	go sc.waitForGate(requestID, m)
	return nil, nil
}

// waitForGate acquires the gate for a begin_write, then pins the committed snapshot and answers.
func (sc *psServerConn) waitForGate(requestID uint32, m psBeginWrite) {
	o := sc.owner
	c := o.core
	err := o.acquireGate(m.lockTimeoutMs, sc.done)
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
	check := psDeltaBase{committedTxid: txid, pageSize: o.pageSize, pageCount: st.pageCount}
	if grant {
		check.granted = make(map[uint32]bool, len(st.freePages))
		for _, p := range st.freePages {
			check.granted[p] = true
		}
	}
	sc.write = &psWriteTxn{base: txid, grant: grant, check: check}
	sc.armIdleLocked()
	sc.send(requestID, psWriteBegun{incarnation: o.incarnation, txid: txid, invalidate: invalidate, indices: indices, reuseGrant: grant, meta: meta})
}

// touch restarts the idle-in-transaction timer on every received frame.
func (sc *psServerConn) touch() {
	sc.mu.Lock()
	defer sc.mu.Unlock()
	if sc.write != nil && !sc.write.finishing {
		sc.armIdleLocked()
	}
}

func (sc *psServerConn) armIdleLocked() {
	if sc.owner.idleTimeout <= 0 {
		return
	}
	if sc.idle == nil {
		sc.idle = time.AfterFunc(sc.owner.idleTimeout, sc.revoke)
	} else {
		sc.idle.Reset(sc.owner.idleTimeout)
	}
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
	err := st.paging.withPager(func(p *pager) error {
		if err := p.writeBlock(uint32(d.txid&1), d.meta); err != nil {
			return err
		}
		if err := p.sync(); err != nil {
			return err
		}
		p.finishValidatedCommit(d.meta)
		p.manifestProtected = d.protected
		return nil
	})
	var free []uint32
	if err == nil {
		free, err = readFreeList(st.paging, d.m.freeListHead, d.m.pageCount)
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
	if o.hangUpNextAck.CompareAndSwap(true, false) {
		_ = sc.conn.Close()
	} else {
		sc.send(requestID, psCommitted{txid: d.txid})
	}
	if c.adoptForeign() == nil {
		c.maintenanceCommit()
	}
	o.releaseGate()
	return nil
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
func (c *sharedCore) adoptForeign() error {
	o := c.service
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
	if loaded.committed.txid > rt.committed.txid {
		c.roots.Store(&roots{committed: loaded.committed, attached: rt.attached})
	}
	c.liveMu.Unlock()
	c.planEpoch.Add(1)
	return nil
}

// maintenanceCommit is the owner-only free-list rebuild after a client commit (page-service.md §6):
// a client commit never walks the database, so when the high-water passes the reclamation threshold
// and no pin holds back reclamation, the owner commits the unchanged snapshot at the next txid with
// in-commit reclamation. Called holding the gate, after the catalog adoption.
func (c *sharedCore) maintenanceCommit() {
	st := c.storage
	rt := c.roots.Load()
	newTxid := rt.committed.txid + 1
	if c.oldestLiveVersion(newTxid) != newTxid {
		return
	}
	if !(st.liveAtCompaction == 0 || (st.pageCount > compactMinPages && uint64(st.pageCount) > compactGrowth*uint64(st.liveAtCompaction))) {
		return
	}
	snap := rt.committed.clone()
	snap.txid = newTxid
	budget := &budgetCtx{name: "main", prev: rt.committed}
	if err := st.commitDurable(snap, true, true, budget); err != nil {
		if st.paging.commitRequiresReopen() {
			c.service.stopServing()
		}
		return
	}
	c.liveMu.Lock()
	c.roots.Store(&roots{committed: snap, attached: rt.attached})
	c.service.published(snap.txid, st.lastMeta, st.lastWritten)
	c.liveMu.Unlock()
	c.planEpoch.Add(1)
}

// HangUpNextPageServiceAckForTesting makes a serving owner drop the connection instead of
// acknowledging its next client commit, after the commit is durable — the case commit_status resolves
// (page-service.md §8.4). Test-only; no effect on a handle that is not a page-service owner.
func HangUpNextPageServiceAckForTesting(db *Database) {
	if o := db.core.service; o != nil {
		o.hangUpNextAck.Store(true)
	}
}
