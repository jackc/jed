package jed

// The page client (spec/design/page-service.md §4.2, §5.3–§5.4; spec/pageservice/README.md §3): a
// process that is not the owner runs its own engine over a pager whose byte store reads pages from the
// owner's page service instead of the file. Phase 2: reads only — a client handle is read-only.
//
// Pins (README.md §6.3). The client holds exactly one owner pin on each version a local reader pins
// (core.live), and none while idle, so it never holds back compaction. begin_read's pin is kept for
// the local reader that follows every read begin; when that reader's version has no local reader
// left, the client sends end_read. Page reads name the newest pinned version: the page count only
// grows between compactions, and a pin blocks compaction, so it bounds every older pinned snapshot.

import (
	"errors"
	"io"
	"net"
	"sync"
	"sync/atomic"
)

// psClient is one page-service connection. mu serializes request/reply pairs on the connection
// (phase 2 sends one request at a time); it is the innermost lock — callers may hold core.liveMu or
// the pager lock when they take it, and nothing is taken while it is held.
type psClient struct {
	dbPath   string
	conn     net.Conn
	mu       sync.Mutex
	nextID   uint32
	pageSize uint32
	connID   uint64
	// broken is the sticky failure after the connection is lost (08006) or poisoned by a fatal error.
	broken error

	// incarnation and adopted name the cache this process holds (README.md §3.2); refreshMu
	// serializes begin_read + adoption so the cache always matches exactly one adopted version.
	refreshMu   sync.Mutex
	incarnation id16
	adopted     uint64
	// meta is the adopted version's meta page, served for page indices 0 and 1.
	metaMu sync.Mutex
	meta   []byte

	// held is the set of versions this connection pins at the owner, guarded by core.liveMu once the
	// core exists. readTxid is its maximum (0 when empty), updated under core.liveMu before any
	// end_read is sent and read under mu, so a read_pages never names a version already released.
	held     map[uint64]bool
	readTxid atomic.Uint64
	// reserved counts begin_read pins whose local reader has not registered yet; releasePin keeps a
	// reserved version's pin (README.md §6.3). Guarded by core.liveMu.
	reserved map[uint64]int

	// release frees the process-local open-path claim when the client closes.
	release   func()
	closeOnce sync.Once
}

func connectionLost(err error) error {
	return newError(ConnectionFailure, "lost the connection to the database owner: "+err.Error())
}

// connectPageClient dials the endpoint and runs hello. A dial or handshake failure the opener may
// retry is returned as retry=true; an owner's refusal (0A000: protocol or format mismatch) is final.
func connectPageClient(dbPath, endpoint string) (cl *psClient, retry bool, err error) {
	conn, err := dialEndpoint(endpoint)
	if err != nil {
		return nil, true, err
	}
	cl = &psClient{dbPath: dbPath, conn: conn, held: map[uint64]bool{}, reserved: map[uint64]int{}}
	reply, err := cl.roundTrip(psHello{protocolMin: psProtocolVersion, protocolMax: psProtocolVersion, formatVersion: formatVersion, flags: psHelloFlagReadOnly, clientName: "jed-go"})
	if err != nil {
		_ = conn.Close()
		var engine *EngineError
		if errors.As(err, &engine) && engine.Code() != "08006" {
			return nil, false, err
		}
		return nil, true, err
	}
	ok, isOK := reply.(psHelloOK)
	if !isOK || !pageSizeValid(int(ok.pageSize)) {
		_ = conn.Close()
		return nil, false, newError(ProtocolViolation, "unexpected reply to hello")
	}
	cl.pageSize = ok.pageSize
	cl.connID = ok.connectionID
	return cl, false, nil
}

// roundTrip sends one request and returns its reply. An error reply comes back as the engine error it
// names; a lost connection is 08006 and breaks the client for good.
func (cl *psClient) roundTrip(m psMessage) (psMessage, error) {
	return cl.roundTripWith(func() (psMessage, error) { return m, nil })
}

// roundTripWith builds the request while holding the connection, so a request may depend on state
// that changes only before a send (readTxid).
func (cl *psClient) roundTripWith(build func() (psMessage, error)) (psMessage, error) {
	cl.mu.Lock()
	defer cl.mu.Unlock()
	if cl.broken != nil {
		return nil, cl.broken
	}
	m, err := build()
	if err != nil {
		return nil, err
	}
	id, err := cl.sendLocked(m)
	if err != nil {
		return nil, err
	}
	gotID, reply, err := readPSFrame(cl.conn, cl.pageSize)
	if err != nil {
		if errors.Is(err, io.EOF) || errors.Is(err, io.ErrUnexpectedEOF) {
			cl.broken = connectionLost(err)
			return nil, cl.broken
		}
		var engine *EngineError
		if !errors.As(err, &engine) {
			cl.broken = connectionLost(err)
			return nil, cl.broken
		}
		cl.broken = err
		return nil, err
	}
	if e, ok := reply.(psError); ok {
		failure := errorFromCode(e.sqlstate, e.message)
		if e.flags&psErrorFlagFatal != 0 {
			cl.broken = failure
			_ = cl.conn.Close()
		}
		return nil, failure
	}
	if gotID != id {
		cl.broken = newError(ProtocolViolation, "reply out of request order")
		_ = cl.conn.Close()
		return nil, cl.broken
	}
	return reply, nil
}

// send sends a request that has no reply (end_read).
func (cl *psClient) send(m psMessage) {
	cl.mu.Lock()
	defer cl.mu.Unlock()
	if cl.broken == nil {
		_, _ = cl.sendLocked(m)
	}
}

func (cl *psClient) sendLocked(m psMessage) (uint32, error) {
	cl.nextID++
	if cl.nextID == 0 {
		cl.nextID = 1
	}
	frame, err := encodePSFrame(cl.nextID, m)
	if err != nil {
		return 0, err
	}
	if _, err := cl.conn.Write(frame); err != nil {
		cl.broken = connectionLost(err)
		return 0, cl.broken
	}
	return cl.nextID, nil
}

// beginRead runs begin_read for the client's cache. The reply carries one new owner pin on its txid.
func (cl *psClient) beginRead() (psReadBegun, error) {
	reply, err := cl.roundTrip(psBeginRead{incarnation: cl.incarnation, lastTxid: cl.adopted})
	if err != nil {
		return psReadBegun{}, err
	}
	rb, ok := reply.(psReadBegun)
	if !ok {
		return psReadBegun{}, newError(ProtocolViolation, "unexpected reply to begin_read")
	}
	if _, ok := parseMeta(rb.meta); !ok {
		return psReadBegun{}, newError(DataCorrupted, "the owner sent an invalid meta page")
	}
	rb.meta = append([]byte(nil), rb.meta...)
	return rb, nil
}

func (cl *psClient) setMeta(meta []byte) {
	cl.metaMu.Lock()
	cl.meta = meta
	cl.metaMu.Unlock()
}

func (cl *psClient) currentMeta() []byte {
	cl.metaMu.Lock()
	defer cl.metaMu.Unlock()
	return cl.meta
}

// readPage fetches one page under the newest pinned version.
func (cl *psClient) readPage(index uint32) ([]byte, error) {
	reply, err := cl.roundTripWith(func() (psMessage, error) {
		txid := cl.readTxid.Load()
		if txid == 0 {
			return nil, newError(ObjectNotInPrerequisiteState, "a page client read a page outside any read transaction")
		}
		return psReadPages{txid: txid, indices: []uint32{index}}, nil
	})
	if err != nil {
		return nil, err
	}
	pages, ok := reply.(psPages)
	if !ok || len(pages.pages) != 1 {
		return nil, newError(ProtocolViolation, "unexpected reply to read_pages")
	}
	return append([]byte(nil), pages.pages[0]...), nil
}

// close drops the connection; the owner releases every pin at EOF.
func (cl *psClient) close() {
	cl.closeOnce.Do(func() {
		cl.mu.Lock()
		if cl.broken == nil {
			cl.broken = newError(ConnectionFailure, "the page-service connection is closed")
		}
		_ = cl.conn.Close()
		cl.mu.Unlock()
		if cl.release != nil {
			cl.release()
		}
	})
}

// serviceBlockStore is the client pager's byte store: page indices 0 and 1 are the adopted meta page,
// every other page comes from the owner. Read-only.
type serviceBlockStore struct {
	client *psClient
}

func (s *serviceBlockStore) readAt(off int64, length int) ([]byte, error) {
	ps := int64(s.client.pageSize)
	index := off / ps
	within := off - index*ps
	if length < 0 || within+int64(length) > ps {
		return nil, newError(IoError, "page-service read must stay within one page")
	}
	var page []byte
	if index < 2 {
		page = s.client.currentMeta()
	} else {
		var err error
		if page, err = s.client.readPage(uint32(index)); err != nil {
			return nil, err
		}
	}
	return append([]byte(nil), page[within:within+int64(length)]...), nil
}

func errClientReadOnly() error {
	return newError(ReadOnlySqlTransaction, "a page client cannot write the database")
}

func (s *serviceBlockStore) writeAt(int64, []byte) error { return errClientReadOnly() }
func (s *serviceBlockStore) sync() error                 { return errClientReadOnly() }
func (s *serviceBlockStore) setSize(int64) error         { return errClientReadOnly() }

// size is the adopted snapshot's page count: every page a reader of it may name lies below it.
func (s *serviceBlockStore) size() (int64, error) {
	m, ok := parseMeta(s.client.currentMeta())
	if !ok {
		return 0, newError(DataCorrupted, "invalid meta page")
	}
	return int64(m.pageCount) * int64(s.client.pageSize), nil
}

func (s *serviceBlockStore) close() error {
	s.client.close()
	return nil
}

// openPageClient builds a read-only engine over a connected client (README.md §3.2): one begin_read
// pins and adopts the owner's committed snapshot, then the ordinary loader reads the catalog skeleton
// through the service.
func openPageClient(cl *psClient, opts OpenOptions, env *Environment) (*engine, error) {
	rb, err := cl.beginRead()
	if err != nil {
		return nil, err
	}
	cl.incarnation, cl.adopted = rb.incarnation, rb.txid
	cl.held[rb.txid] = true
	cl.readTxid.Store(rb.txid)
	cl.setMeta(rb.meta)
	p, err := pagerFromStore(&serviceBlockStore{client: cl})
	if err != nil {
		return nil, err
	}
	p.trustMeta = true
	cacheBytes := opts.CacheBytes
	if cacheBytes <= 0 {
		cacheBytes = defaultCacheBytes
	}
	db, err := loadEnginePaged(p, cacheLeaves(cacheBytes, p.pageSize), env)
	if err != nil {
		return nil, err
	}
	// No local reader follows the open; every read begins with its own begin_read.
	cl.releasePin(rb.txid)
	db.path = cl.dbPath
	db.readOnly = true
	if opts.WorkMem != 0 {
		db.session.workMem = opts.WorkMem
	}
	return db, nil
}

// clientRefresh is a page client's read begin (README.md §3.2): one begin_read round trip, then the
// cache invalidation and, when the owner's version moved, the adoption of its snapshot. It returns the
// version whose pin it reserved for the local reader the caller registers next (refreshAndPin).
func (c *sharedCore) clientRefresh() (uint64, error) {
	cl := c.client
	cl.refreshMu.Lock()
	defer cl.refreshMu.Unlock()
	rb, err := cl.beginRead()
	if err != nil {
		return 0, err
	}
	c.liveMu.Lock()
	if cl.held[rb.txid] {
		cl.send(psEndRead{txid: rb.txid}) // a duplicate of a pin already held
	} else {
		cl.held[rb.txid] = true
		cl.updateReadTxid()
	}
	cl.reserved[rb.txid]++
	c.liveMu.Unlock()
	pool := c.storage.paging.pool
	all := rb.invalidate == psInvalidateAll || rb.incarnation != cl.incarnation
	if all {
		pool.invalidateAll()
	} else {
		for _, index := range rb.indices {
			pool.invalidate(index)
		}
	}
	if !all && rb.txid <= cl.adopted {
		return rb.txid, nil
	}
	cl.setMeta(rb.meta)
	c.storage.mu.Lock()
	if err := c.storage.paging.withPager(func(p *pager) error { return p.refreshAllocatedPages() }); err != nil {
		c.storage.mu.Unlock()
		c.unreserveAndRelease(rb.txid)
		return 0, err
	}
	loaded, err := loadEngineSharedPaging(c.storage.paging, c.env)
	if err != nil {
		c.storage.mu.Unlock()
		c.unreserveAndRelease(rb.txid)
		return 0, err
	}
	c.storage.pageCount = loaded.pageCount
	c.storage.freePages = loaded.freePages
	c.storage.live = loaded.live
	c.storage.mu.Unlock()
	c.liveMu.Lock()
	cl.incarnation, cl.adopted = rb.incarnation, rb.txid
	rt := c.roots.Load()
	c.roots.Store(&roots{committed: loaded.committed, attached: rt.attached})
	c.liveMu.Unlock()
	c.planEpoch.Add(1)
	return rb.txid, nil
}

// unreserveAndRelease drops a reservation whose read begin failed, releasing the pin if unused.
func (c *sharedCore) unreserveAndRelease(v uint64) {
	c.liveMu.Lock()
	defer c.liveMu.Unlock()
	c.client.unreserve(v)
	if c.live[v] == 0 {
		c.client.releasePin(v)
	}
}

// unreserve drops one reservation on v. Caller holds liveMu.
func (cl *psClient) unreserve(v uint64) {
	if cl.reserved[v]--; cl.reserved[v] <= 0 {
		delete(cl.reserved, v)
	}
}

// updateReadTxid sets readTxid to the newest held version. Caller holds liveMu (or owns the client
// exclusively, before the core exists).
func (cl *psClient) updateReadTxid() {
	newest := uint64(0)
	for v := range cl.held {
		newest = max(newest, v)
	}
	cl.readTxid.Store(newest)
}

// releasePin sends end_read for a held version v; readTxid moves off it first. Caller holds liveMu
// (or owns the client exclusively).
func (cl *psClient) releasePin(v uint64) {
	if !cl.held[v] || cl.reserved[v] > 0 {
		return
	}
	delete(cl.held, v)
	cl.updateReadTxid()
	cl.send(psEndRead{txid: v})
}

// errorFromCode rebuilds an engine error from an error frame's SQLSTATE. An unknown code (a newer
// owner) is reported as a protocol violation naming it.
func errorFromCode(code, message string) error {
	for i := 0; i <= len(errorDescs); i++ {
		if state := SqlState(i); state.Code() == code {
			return newError(state, message)
		}
	}
	return newError(ProtocolViolation, "the owner sent unknown SQLSTATE "+code+": "+message)
}
