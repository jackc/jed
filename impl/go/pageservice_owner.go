package jed

// The page-service owner (spec/design/page-service.md §4.1, §5; spec/pageservice/README.md): the
// presence-EX holder serves pages of its database to page clients over its endpoint. Each connection
// runs on its own goroutine, so a client request never blocks the owner's own sessions. A client's
// pins join the owner's reader registry (core.live), so the reclamation watermark covers every
// process (transactions.md §8). Phase 2: read path only — every connection is read-only.

import (
	"crypto/rand"
	"errors"
	"io"
	"net"
	"os"
	"sort"
	"strconv"
	"sync"
)

// psWindowEntry is one commit's written page indices (README.md §4).
type psWindowEntry struct {
	txid    uint64
	indices []uint32
}

// pageServiceOwner is the serving state of an owner. meta, metaTxid, window, and windowStart change
// only together with the published root, under core.liveMu, so begin_read observes a consistent
// (snapshot, meta, invalidation window) triple.
type pageServiceOwner struct {
	core        *sharedCore
	dbPath      string
	incarnation id16
	pageSize    uint32
	readOnly    bool

	// Guarded by core.liveMu.
	meta        []byte
	metaTxid    uint64
	window      []psWindowEntry
	windowStart uint64

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
	wg       sync.WaitGroup
}

// psServerConn is one client connection on the owner side.
type psServerConn struct {
	owner *pageServiceOwner
	conn  net.Conn
	id    uint64
	// pins counts this connection's pins per txid; each is also one count in core.live.
	pins map[uint64]int
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
func startPageServiceOwner(c *sharedCore, dbPath string) (*pageServiceOwner, error) {
	o := &pageServiceOwner{
		core:        c,
		dbPath:      dbPath,
		incarnation: newIncarnation(),
		pageSize:    c.storage.pageSize,
		readOnly:    c.storage.readOnly,
		conns:       map[*psServerConn]struct{}{},
	}
	txid := c.roots.Load().committed.txid
	meta, err := o.readCommittedMeta(txid)
	if err != nil {
		return nil, err
	}
	o.meta, o.metaTxid, o.windowStart = meta, txid, txid
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

// published records a main-domain commit: its meta and the indices it wrote. Called by the publisher
// under core.liveMu, together with the root swap.
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

// close stops serving: no new connections, every client sees EOF (releasing its pins), and the
// endpoint and owner record are removed while presence EX is still held.
func (o *pageServiceOwner) close() {
	o.connMu.Lock()
	o.closed = true
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
		sc := &psServerConn{owner: o, conn: conn, id: o.nextConn, pins: map[uint64]int{}}
		o.conns[sc] = struct{}{}
		o.wg.Add(1)
		o.connMu.Unlock()
		go sc.serve()
	}
}

// serve runs one connection until EOF or a protocol violation, then releases its pins.
func (sc *psServerConn) serve() {
	o := sc.owner
	defer func() {
		_ = sc.conn.Close()
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
			reply = psError{sqlstate: "25006", message: "cannot write through a read-only page-service connection"}
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
	flags := psHelloOKFlagReadOnly // phase 2: every connection is read-only
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
	txid := c.roots.Load().committed.txid
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
	if sc.pins[m.txid]--; sc.pins[m.txid] == 0 {
		delete(sc.pins, m.txid)
	}
	if c.live[m.txid]--; c.live[m.txid] <= 0 {
		delete(c.live, m.txid)
	}
	return nil
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

func (sc *psServerConn) send(requestID uint32, m psMessage) bool {
	frame, err := encodePSFrame(requestID, m)
	if err != nil {
		return false
	}
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
